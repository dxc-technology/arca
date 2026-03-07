# S3 Compatibility Triage

Results from running [Ceph s3-tests](https://github.com/ceph/s3-tests) against Arca.

## Summary

| Metric | Count |
|--------|-------|
| Total | 829 |
| Passed | 198 |
| Failed | 540 |
| Skipped | 91 |
| Expected fail (unimplemented features) | 310 + 127 miscategorized = **437** |
| **Unexpected failures (real bugs)** | **52** |
| **Strategic decisions needed** | **60** |
| Pass rate (overall) | 23.9% |
| Pass rate (implemented features only) | ~79% |

## How to Read This Report

Tests are classified into three buckets:

1. **Expected failures** — tests for features explicitly out of MVP scope (ACLs, versioning, encryption, etc.). These are fine.
2. **Fixable bugs** — tests for *implemented* APIs that fail due to bugs in Arca. These should be fixed.
3. **Strategic decisions** — tests that fall in a gray area: features that are partially related to implemented APIs but require new capabilities. These need discussion.

---

## A. Fixable Bugs (52 tests)

Bugs in already-implemented functionality, grouped by root cause. Severity reflects how many real-world S3 clients would hit the issue.

### CRITICAL

#### 1. Multipart: basic upload fails with NoSuchUpload (2 tests)

`test_multipart_upload`, `test_multipart_upload_small`

**Symptom**: `CompleteMultipartUpload` returns `NoSuchUpload` even though `CreateMultipartUpload` and `UploadPart` succeeded.

**Root cause**: The s3-tests use `x-amz-copy-source` in the PUT for UploadPartCopy. In `object.rs:29`, the handler checks `x-amz-copy-source` *before* checking `partNumber`/`uploadId` query params. So UploadPartCopy requests get dispatched to `copy_object()` instead of `upload_part()`. The test then calls `CompleteMultipartUpload` with part ETags that were never stored as parts — hence `NoSuchUpload` or `InvalidPart`.

Wait — for the basic `test_multipart_upload` and `test_multipart_upload_small`, these don't use copy. Let me re-examine.

**Likely root cause**: These tests use `boto3`'s high-level multipart which may include `Content-MD5` or other headers that cause a mismatch. Need to run with `-s` to see the actual HTTP exchange. Could also be that the upload_id format doesn't round-trip through the XML parse correctly (e.g. whitespace in XML).

**Files**: `crates/arca-proto/src/handlers/multipart.rs` (CompleteMultipartUpload), `crates/arca-core/src/s3/xml_types.rs` (parse_complete_multipart_upload)

---

### HIGH

#### 2. ListObjects: delimiter+prefix returns key instead of CommonPrefix (6 tests)

`test_bucket_list_delimiter_prefix`, `test_bucket_listv2_delimiter_prefix`, `test_bucket_list_delimiter_prefix_ends_with_delimiter`, `test_bucket_listv2_delimiter_prefix_ends_with_delimiter`, `test_bucket_list_delimiter_prefix_underscore`, `test_bucket_listv2_delimiter_prefix_underscore`

**Symptom**: When listing with both prefix and delimiter, the first CommonPrefix entry is the full key (`boo/bar`) instead of the prefix group (`boo/`).

**Root cause**: `extract_common_prefixes()` in `bucket.rs:413-441` works correctly for the general case, but the tests use a prefix like `boo` (without trailing delimiter). The `after_prefix` slice starts at `boo/bar` → `"/bar"`, finds `/` at position 0, so the common prefix becomes `boo/` — this should actually work. The real issue is likely that the SQL query with `start_after` or prefix filter is not returning the right records, or the `is_truncated` logic with `max_keys` interacts badly with prefix grouping. Specifically: when `delimiter` is set, records that collapse into CommonPrefixes still count toward `max_keys`, but the current code fetches `max_keys + 1` raw records from DB *before* grouping. If many keys collapse into one CommonPrefix, the DB returns too few records to produce the expected contents+prefixes. This is a fundamental issue: the DB query doesn't know about delimiter grouping, so it can't correctly limit results.

**Files**: `crates/arca-proto/src/handlers/bucket.rs:163-185` (V2), `bucket.rs:265-287` (V1), `bucket.rs:413-441` (extract_common_prefixes)

#### 3. CopyObject: x-amz-metadata-directive not implemented (3 tests)

`test_object_copy_retaining_metadata`, `test_object_copy_replacing_metadata`, `test_object_copy_to_itself_with_metadata`

**Symptom**: Copy always uses source content-type; user metadata is never preserved or replaced.

**Root cause**: `copy_object()` in `object.rs:135-253` ignores the `x-amz-metadata-directive` header entirely. It always copies the source `content_type` (line 209) and never reads `x-amz-meta-*` headers from the request. When directive is `COPY` (default), it should retain *all* source metadata. When `REPLACE`, it should use the new headers.

Additionally, `ObjectRecord` has no field for user metadata (`x-amz-meta-*`) — the entire user metadata storage pipeline is missing (see bug #4).

**Files**: `crates/arca-proto/src/handlers/object.rs:135-253`

#### 4. Object metadata: x-amz-meta-* headers not stored/returned (5 tests)

`test_object_set_get_metadata_none_to_good`, `test_object_set_get_metadata_none_to_empty`, `test_object_set_get_metadata_overwrite_to_empty`, `test_object_write_cache_control`, `test_object_content_encoding_aws_chunked`

**Symptom**: PutObject with `x-amz-meta-foo: bar` succeeds, but GetObject/HeadObject doesn't return the metadata. Same for `Cache-Control` and `Content-Encoding`.

**Root cause**: `ObjectRecord` in `types.rs:38-46` only has `content_type: Option<String>`. There is no field for:
- User metadata (`x-amz-meta-*` headers)
- `Cache-Control`
- `Content-Encoding`
- `Content-Disposition`
- `Content-Language`
- `Expires`

The `put_object` handler in `object.rs:23-129` only extracts `Content-Type` (line 69-73) and ignores all other headers. The DB schema (`sqlite/metadata.rs`) similarly has no columns for these.

**Fix requires**: Adding a `metadata: HashMap<String, String>` field to `ObjectRecord`, a `metadata` TEXT column (JSON) to the objects table, and extracting/returning these headers in the handlers.

**Files**: `crates/arca-core/src/types.rs:38-46`, `crates/arca-proto/src/handlers/object.rs:69-73`, `crates/arca-storage/src/sqlite/metadata.rs`

#### 5. Multipart: UploadPartCopy not implemented (4 tests)

`test_multipart_copy_small`, `test_multipart_copy_multiple_sizes`, `test_multipart_copy_special_names`, `test_multipart_copy_without_range`

**Symptom**: `CompleteMultipartUpload` returns `InvalidPart` after UploadPartCopy.

**Root cause**: In `object.rs:29`, the `put_object` handler checks `x-amz-copy-source` *before* checking `partNumber`/`uploadId`. So `PUT /{bucket}/{key}?partNumber=1&uploadId=X` with `x-amz-copy-source` header gets dispatched to `copy_object()` (which creates a new object) instead of being handled as UploadPartCopy (which should store a part). The part is never created, so `CompleteMultipartUpload` can't find it.

**Fix**: Reorder the dispatch in `put_object`: check for `partNumber`/`uploadId` first, and if both are present AND `x-amz-copy-source` is present, handle as UploadPartCopy. Otherwise fall through to CopyObject.

**Files**: `crates/arca-proto/src/handlers/object.rs:23-56`

#### 6. Multipart: metadata not stored from CreateMultipartUpload (5 tests)

`test_multipart_upload_resend_part`, `test_encryption_sse_c_multipart_upload`, `test_encryption_sse_c_unaligned_multipart_upload`, `test_encryption_sse_c_multipart_bad_download`, `test_sse_kms_multipart_upload`

**Symptom**: After multipart upload, `GetObject` returns empty metadata `{}` instead of `{'foo': 'bar'}`.

**Root cause**: Same as bug #4 — `MultipartUploadRecord` has `content_type` but no `metadata` field. The `x-amz-meta-*` headers sent during `CreateMultipartUpload` are ignored. Even if they were stored, the final `ObjectRecord` created in `CompleteMultipartUpload` wouldn't carry them.

**Files**: `crates/arca-proto/src/handlers/multipart.rs:24-68`, `crates/arca-core/src/types.rs:91-98`

#### 7. ListMultipartUploads: not implemented (2 tests)

`test_list_multipart_upload`, `test_lifecycle_multipart_expiration`

**Symptom**: `KeyError: 'Uploads'` — the ListMultipartUploads API (`GET /{bucket}?uploads`) is not handled.

**Root cause**: The `get_bucket` handler in `bucket.rs:38-90` dispatches based on `list-type` and `versions` params but doesn't check for `uploads`. The `?uploads` query parameter is only handled in `post_object` (POST method) for CreateMultipartUpload, not in GET for ListMultipartUploads.

**Files**: `crates/arca-proto/src/handlers/bucket.rs:38-90`

#### 8. Range requests: suffix range `bytes=-N` returns full object (1 test)

`test_ranged_request_return_trailing_bytes_response_code`

**Symptom**: `Range: bytes=-7` on "testcontent" (10 bytes) should return "content" (last 7 bytes) but returns the full object.

**Root cause**: `parse_range_header()` in `object.rs:461-485` parses `bytes=START-END` and `bytes=START-` but doesn't handle `bytes=-N` (suffix range). When the start part is empty, `parts[0].parse()` returns `None`, and the function returns `None` (no range = full object).

**Fix**: Add suffix range handling: if `parts[0]` is empty and `parts[1]` is a valid number N, return `ByteRange { start: file_size - N, end: None }`.

**Files**: `crates/arca-proto/src/handlers/object.rs:461-485`

#### 9. Content-Type not preserved in raw GET (1 test)

`test_object_raw_response_headers`

**Symptom**: PutObject with `Content-Type: foo/bar` succeeds, but GetObject returns `Content-Type: application/octet-stream`.

**Root cause**: This specific test uses a raw HTTP PUT (not boto3), possibly without proper S3 signing. But more fundamentally, if the Content-Type is being stored (the `put_object` handler does extract it at line 69-73), then the issue may be that the test's PUT doesn't set the header in a way that Arca's handler reads it. More likely: the test creates the object via a presigned URL or anonymous PUT, which we don't support. Need to verify by running the specific test.

**Files**: `crates/arca-proto/src/handlers/object.rs:69-73, 316-318`

---

### MEDIUM

#### 10. ListObjects: max-keys=0 returns IsTruncated=true (2 tests)

`test_bucket_list_maxkeys_zero`, `test_bucket_listv2_maxkeys_zero`

**Root cause**: When `max_keys=0`, `fetch_limit = max_keys + 1 = 1`. If there are any objects, `records.len() (1) > max_keys (0)` → `is_truncated = true`. But S3 says `max-keys=0` should return `IsTruncated=false` (you asked for 0 items and got 0).

**Fix**: Special-case `max_keys == 0`: skip the DB query and return empty results with `is_truncated = false`.

**Files**: `bucket.rs:120-133` (V2), `bucket.rs:249-262` (V1)

#### 11. ListObjects: encoding-type=url not applied (2 tests)

`test_bucket_list_encoding_basic`, `test_bucket_listv2_encoding_basic`

**Root cause**: The `encoding_type` param is parsed but hardcoded to `None` in the XML params (`bucket.rs:209`, `bucket.rs:307`). When `encoding-type=url`, keys containing `+` and other special chars should be URL-encoded in the XML response.

**Files**: `bucket.rs:209, 307`

#### 12. ListObjectsV2: fetch-owner=true missing Owner (1 test)

`test_bucket_listv2_fetchowner_notempty`

**Root cause**: The `ListEntry` struct has no `owner` field, and the XML builder doesn't emit `<Owner>` in `<Contents>`. When `fetch-owner=true`, each entry should include `<Owner><ID>...</ID><DisplayName>...</DisplayName></Owner>`.

**Files**: `crates/arca-core/src/types.rs:49-56`, XML builder in `xml_types.rs`

#### 13. CopyObject: copy to same key without REPLACE should error (1 test)

`test_object_copy_to_itself`

**Root cause**: S3 requires that copying an object to itself returns `InvalidRequest` unless `x-amz-metadata-directive: REPLACE` is set. `copy_object()` doesn't check for this.

**Files**: `object.rs:135-253`

#### 14. AbortMultipartUpload: non-existent upload should error (1 test)

`test_abort_multipart_upload_not_found`

**Root cause**: `abort_multipart_upload()` in `multipart.rs:347-373` calls `delete_multipart_upload` which returns an empty parts list for non-existent uploads, and then returns 204 regardless. It should return `NoSuchUpload` if the upload doesn't exist.

**Fix**: Check if the upload exists before deleting, or have `delete_multipart_upload` return an indicator.

**Files**: `multipart.rs:347-373`

#### 15. Multipart: resend part with different number fails with InvalidPartOrder (1 test)

`test_multipart_resend_first_finishes_last`

**Root cause**: The test uploads part 1 twice (first a large slow upload, then a fast one). The fast upload replaces part 1's record. Then it completes with parts [1, 2]. The completion should work because both parts exist. The `InvalidPartOrder` error suggests the XML parser or validation has an issue with the order.

**Files**: `multipart.rs:207-211`

#### 16. Range requests: invalid/empty object ranges return 200 instead of 416 (2 tests)

`test_ranged_request_invalid_range`, `test_ranged_request_empty_object`

**Root cause**: `parse_range_header()` returns `None` for unparseable or unsatisfiable ranges, causing the handler to return the full object with 200. It should return 416 Range Not Satisfiable.

**Fix**: Return a dedicated error variant from `parse_range_header` to distinguish "no range header" from "invalid range".

**Files**: `object.rs:461-485, 297-307`

#### 17. GET/DELETE on non-existent bucket returns wrong error (4 tests)

`test_object_raw_authenticated_bucket_gone`, `test_object_raw_get_bucket_gone`, `test_object_raw_get_object_gone`, `test_object_delete_key_bucket_gone`

**Root cause**: `get_object()` and `delete_object()` don't check `head_bucket()` first. If a bucket doesn't exist, the key lookup returns `NoSuchKey` instead of `NoSuchBucket`. The 403 vs 404 discrepancy comes from the auth middleware returning `AccessDenied` before the handler even runs (when using anonymous/different credentials).

**Fix**: Add bucket existence check in `get_object` and `delete_object` handlers, before the key lookup.

**Files**: `object.rs:282-335` (get_object), `object.rs:374-414` (delete_object)

#### 18. ListObjects: delimiter with special chars (1 test)

`test_bucket_list_delimiter_not_skip_special`

**Root cause**: The test uses delimiter `+` with keys like `0`, `1999`, `1999+`, `2000`. Expected CommonPrefixes: `1999+`. The issue is likely related to how the keys are stored or how prefix matching works with special characters.

**Files**: `bucket.rs:413-441`

---

### LOW

#### 19. Empty delimiter included in V1 response XML (1 test)

`test_bucket_list_delimiter_empty`

**Root cause**: When `delimiter` is `""` (empty string), the V1 handler passes `Some("")` to the XML builder, which emits `<Delimiter></Delimiter>`. S3 omits the element entirely for empty delimiter.

**Fix**: Filter `delimiter` to `None` when it's empty before passing to XML builder.

**Files**: `bucket.rs:297-308`

#### 20. Unicode metadata breaks SigV4 (1 test)

`test_object_set_get_unicode_metadata`

**Root cause**: `SignatureDoesNotMatch` when metadata values contain unicode. The SigV4 canonical headers computation in `arca-auth` may not handle non-ASCII characters correctly.

**Files**: `crates/arca-auth/src/sigv4.rs`

#### 21. Multipart empty parts: InvalidArgument instead of MalformedXML (1 test)

`test_multipart_upload_empty`

**Root cause**: `multipart.rs:199-205` returns `InvalidArgument` for empty parts list. S3 returns `MalformedXML`.

**Fix**: Change error code to `MalformedXML`.

**Files**: `multipart.rs:199-205`

#### 22. Request-ID in error body doesn't match response header (1 test)

`test_object_requestid_matches_header_on_error`

**Root cause**: The error XML body contains a `<RequestId>` generated at XML construction time, while the `x-amz-request-id` header is set by middleware. These are two separate UUIDs.

**Fix**: Pass the request ID from the middleware into the error response builder.

**Files**: Error response middleware and `xml/error_response.rs`

#### 23. Bucket name: dot-dash / dash-dot not rejected (2 tests)

`test_bucket_create_naming_dns_dot_dash`, `test_bucket_create_naming_dns_dash_dot`

**Root cause**: `validate_bucket_name()` in `bucket_name.rs` checks for `..` but not for `.-` or `-.` sequences. AWS rejects these.

**Fix**: Add `name.contains(".-") || name.contains("-.")` check.

**Files**: `crates/arca-core/src/s3/bucket_name.rs:40-50`

#### 24. DeleteObjects: 1000-key limit not enforced (2 tests)

`test_multi_object_delete_key_limit`, `test_multi_objectv2_delete_key_limit`

**Root cause**: `delete_objects()` in `bucket.rs:588-676` doesn't check if the number of keys exceeds 1000.

**Fix**: Add a check after parsing the XML body.

**Files**: `bucket.rs:588-676`

---

## B. Strategic Decisions (60 tests)

These tests require discussion — they either need new APIs, new features, or touch on scope boundaries.

### B1. Conditional Headers: If-Match / If-None-Match / If-Modified-Since / If-Unmodified-Since (19 tests)

GET (4), PUT (6), DELETE (6), COPY (2), multipart (1)

**What's needed**: Parse conditional headers from requests and check them against the object's ETag and Last-Modified before proceeding. Return 304 Not Modified or 412 Precondition Failed as appropriate.

**Impact**: Widely used by SDKs and CDNs for caching and optimistic concurrency. boto3, aws-cli, and all major S3 clients send these headers. Would fix 19 tests.

**Recommendation**: **Implement** — this is core S3 semantics and relatively straightforward. It's header parsing + comparison logic in the existing handlers, no new storage changes needed.

### B2. GetObjectAttributes API (4 tests)

`test_get_object_attributes`, `test_get_multipart_object_attributes`, `test_get_paginated_multipart_object_attributes`, `test_get_single_multipart_object_attributes`

**What's needed**: New API endpoint `GET /{bucket}/{key}?attributes` returning object metadata in a structured response. Currently returns 500 (unhandled query param).

**Recommendation**: **Stub with 501 NotImplemented** — this is a newer AWS API (2022) not critical for MVP. The 500 should become a clean 501.

### B3. Checksums: CRC32, SHA256, CRC64NVME (10 tests)

`test_object_checksum_sha256`, `test_object_checksum_crc64nvme`, `test_multipart_checksum_sha256`, `test_multipart_use_cksum_helper_*` (5), `test_get_checksum_object_attributes`, `test_get_multipart_checksum_object_attributes`

**What's needed**: Support `x-amz-checksum-*` headers, compute and store additional checksums beyond MD5.

**Recommendation**: **Defer** — AWS added these in 2022-2024. Not needed for MVP compatibility. Most clients still use MD5/ETag.

### B4. GetObject partNumber / x-amz-mp-parts-count (5 tests)

`test_multipart_get_part`, `test_multipart_single_get_part`, `test_multipart_sse_c_get_part`, `test_non_multipart_get_part`, `test_non_multipart_sse_c_get_part`

**What's needed**: Support `?partNumber=N` on GET to retrieve individual parts of a multipart object. Return `x-amz-mp-parts-count` header.

**Recommendation**: **Defer** — used by parallel download tools but not critical for MVP.

### B5. ListBuckets pagination (1 test)

`test_list_buckets_paginated`

**What's needed**: Support `max-buckets` and continuation token parameters on ListBuckets.

**Recommendation**: **Defer** — very recent AWS addition (2024). No client depends on it yet.

### B6. UploadPartCopy range validation (2 tests)

`test_multipart_copy_invalid_range`, `test_multipart_copy_improper_range`

**What's needed**: UploadPartCopy with invalid byte ranges should return an error. This is part of implementing UploadPartCopy (bug #5).

**Recommendation**: **Implement alongside UploadPartCopy** (bug #5).

### B7. Bucket encryption config error codes (2 tests)

`test_get_bucket_encryption_s3`, `test_get_bucket_encryption_kms`

**What's needed**: `GET /{bucket}?encryption` on a bucket without encryption config should return `ServerSideEncryptionConfigurationNotFoundError`. Currently returns empty 200.

**Recommendation**: **Quick fix** — return the proper error code for this unimplemented GET.

### B8. Anonymous access (4 tests)

`test_list_buckets_anonymous`, `test_object_raw_get`, `test_object_anon_put_write_access`, `test_object_raw_get_bucket_acl`

**What's needed**: Requires ACL support to allow unauthenticated access to specific resources.

**Recommendation**: **Defer** — ACLs are explicitly out of MVP scope.

### B9. Multi-user / ownership (3 tests)

`test_bucket_create_exists_nonowner`, `test_object_copy_not_owned_bucket`, `test_object_copy_not_owned_object_bucket`

**What's needed**: Multiple user identities with separate bucket ownership.

**Recommendation**: **Defer** — single-user focus for MVP.

### B10. RGW-specific headers (4 tests)

`test_bucket_head_extended`, `test_head_bucket_usage`

**What's needed**: `x-rgw-object-count`, `x-rgw-bytes-used` headers on HeadBucket. These are Ceph/RGW extensions, not part of AWS S3.

**Recommendation**: **Skip** — not AWS S3 API. These are Ceph-specific headers.

### B11. Miscellaneous edge cases (6 tests)

| Test | Issue |
|------|-------|
| `test_get_object_torrent` | GetObjectTorrent not implemented (niche feature) |
| `test_account_usage` | Account-level usage API (not S3 standard) |
| `test_bucket_list_unordered` (x2) | `?list-type=unordered` — RGW extension |
| `test_put_obj_enc_conflict_c_s3` | SSE-C vs SSE-S3 conflict detection |
| `test_bucket_list_return_data` | Needs GetObjectAcl (calls it during verification) |
| `test_get_bucket_policy_status` | GetBucketPolicyStatus (needs policy support) |

**Recommendation**: **Skip all** — either RGW-specific, niche, or depends on unimplemented features.

---

## C. Miscategorized Expected Failures (127 tests)

These tests fail because they use unimplemented features (policies, ACLs, versioning, etc.) but are categorized under "Bucket", "Object", or "Multipart" in the report tool. They should be reclassified as expected failures.

| Root cause | Count | Details |
|------------|-------|---------|
| Requires PutBucketPolicy | 28 | Bucket logging, policy-gated ops |
| Requires PutBucketVersioning | 38 | Version-specific ops miscategorized |
| Requires PutBucketAcl / ACL grants | 25 | ACL-gated access, bucket grants |
| Requires Ownership Controls | 8 | BucketOwnerEnforced/Preferred |
| Requires Tagging | 6 | Object/bucket tagging |
| Requires Encryption | 5 | PutBucketEncryption |
| Requires Anonymous access | 4 | Unauthenticated requests |
| Checksums (new AWS feature) | 10 | CRC32, SHA256, CRC64NVME |
| Requires Lifecycle | 1 | PutBucketLifecycleConfiguration |
| Other NotImplemented | 2 | Logging error codes |

**Action**: Update `report.py` categorizer to correctly classify these as expected failures.

---

## D. Recommended Fix Priority

### Phase 1: Quick wins — DONE ✅

All 8 fixes implemented, 177 unit tests + 103 integration tests passing.

| Bug | Tests fixed | Status |
|-----|------------|--------|
| #10 max-keys=0 | 2 | ✅ Done |
| #19 empty delimiter in XML | 1 | ✅ Done |
| #21 empty parts error code | 1 | ✅ Done |
| #23 bucket name dot-dash | 2 | ✅ Done |
| #24 delete key limit | 2 | ✅ Done |
| #14 abort non-existent upload | 1 | ✅ Done |
| #17 bucket existence check in GET/DELETE | 4 | ✅ Done |
| B7 bucket encryption error code | 2 | ✅ Done |
| **Subtotal** | **15** | |

### Phase 2: Medium effort, high value

| Bug | Tests fixed | Effort |
|-----|------------|--------|
| #8 suffix range bytes=-N | 1 | easy |
| #16 invalid range → 416 | 2 | easy |
| #13 copy to self check | 1 | easy |
| #11 encoding-type=url | 2 | medium |
| #5 UploadPartCopy dispatch | 4+2 | medium |
| #2 delimiter+prefix grouping | 6 | medium |
| B1 conditional headers | 19 | medium |
| **Subtotal** | **37** | |

### Phase 3: Larger effort (schema changes)

| Bug | Tests fixed | Effort |
|-----|------------|--------|
| #4 + #6 user metadata storage | 10 | large (schema migration) |
| #3 metadata-directive | 3 | medium (depends on #4) |
| #12 fetch-owner | 1 | medium |
| #7 ListMultipartUploads | 2 | medium |
| **Subtotal** | **16** | |

### Total: fixing all would take us from 198 → ~266 passing tests (32%)

Combined with reclassifying 127 miscategorized tests as expected, the "unexpected failure" count would drop from 230 → ~42, making the report much cleaner.
