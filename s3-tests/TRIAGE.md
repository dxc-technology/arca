# S3 Compatibility Triage

Results from running [Ceph s3-tests](https://github.com/ceph/s3-tests) against Arca.

## Summary

| Metric | Count |
|--------|-------|
| Total | 829 |
| Passed | 198 |
| Failed | 540 |
| Skipped | 91 |
| Pass rate | 23.9% |

## Category Breakdown

| Category | Pass | Fail | Skip | Notes |
|----------|------|------|------|-------|
| Bucket | 91 | 66 | 3 | Core ops work; delimiter/prefix edge cases, encoding, max-keys=0 need fixes |
| Object | 18 | 47 | 0 | Core CRUD works; metadata replace, conditional ops, anonymous access fail |
| Multipart | 11 | 51 | 0 | Basic lifecycle works; SSE, abort edge cases, list-parts fail |
| Copy | 2 | 8 | 0 | Basic copy works; metadata retain/replace, conditional copy fail |
| List | 0 | 4 | 0 | V1/V2 work; encoding type, unordered listing not supported |
| Auth | 3 | 2 | 0 | SigV4 works; anonymous access not implemented |
| Other | 32 | 50 | 73 | Mixed: logging, lifecycle debug, ownership controls |
| ACL/Policy | 7 | 78 | 3 | **Expected** — ACLs not in MVP |
| Encryption | 21 | 100 | 0 | **Expected** — SSE not in MVP |
| Lifecycle | 4 | 39 | 10 | **Expected** — Lifecycle not in MVP |
| Versioning | 0 | 20 | 0 | **Expected** — Versioning not in MVP |
| Object Lock | 3 | 33 | 0 | **Expected** — Object Lock not in MVP |
| CORS | 0 | 12 | 0 | **Expected** — S3 CORS config not in MVP |
| Presigned/POST | 5 | 26 | 0 | **Expected** — Presigned URLs not in MVP |
| Tagging | 1 | 2 | 0 | **Expected** — Tagging not in MVP |
| Headers | 0 | 2 | 0 | Response header edge cases |
| Notifications | 0 | 0 | 2 | **Expected** — Not in MVP |

## Failure Categories

### Expected Failures (unimplemented features)

These features are explicitly out of scope for the MVP:

- **ACLs/Bucket Policies** (~81 tests): All ACL/grant/policy operations
- **Server-Side Encryption** (~100 tests): SSE-S3, SSE-C, SSE-KMS
- **Object Versioning** (~20 tests): Enable/suspend versioning, version-specific ops
- **Lifecycle Rules** (~49 tests): Expiration, transition policies
- **Object Lock/Retention** (~36 tests): Governance/compliance mode
- **CORS Configuration** (~12 tests): PUT/GET bucket CORS
- **Presigned URLs/POST** (~31 tests): Pre-signed operations, browser POST upload
- **Tagging** (~3 tests): Object/bucket tagging
- **Notifications** (~2 tests): Event notifications
- **S3 Select** (0 in test_s3.py): SQL queries on objects

### Fixable Bugs (identified)

1. **Delimiter/prefix handling in V1 list**: `test_bucket_list_delimiter_prefix` — V1 list returns first Content key instead of first common prefix
2. **Empty delimiter in response**: `test_bucket_list_delimiter_empty` — Empty delimiter should not be included in response
3. **max-keys=0**: `test_bucket_list_maxkeys_zero` — Should return 0 objects but IsTruncated=false
4. **Encoding type**: `test_bucket_list_encoding_basic` — URL encoding of special chars in keys
5. **Owner in ListObjectsV2**: `test_bucket_listv2_fetchowner_notempty` — Need Owner element in Contents when fetch-owner=true
6. **Metadata operations**: `test_object_copy_retaining_metadata` / `replacing_metadata` — x-amz-metadata-directive handling

### Fixed in This Phase

- `x-amz-request-id` / `x-amz-id-2` / `Server` response headers
- `GetBucketLocation` (`?location`)
- `CreateBucket` idempotency (200 instead of 409)
- Whitespace preservation in DeleteObjects XML parsing
- ListObjects V1 support (Marker/NextMarker format)
- Empty delimiter not emitted in XML response
- Unimplemented PUT bucket operations (versioning, etc.) return 501 instead of hanging
