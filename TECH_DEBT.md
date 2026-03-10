# Technical Debt

Workarounds, hardcoded values, and temporary fixes that pass S3 compatibility
tests or satisfy client expectations but need proper implementation post-MVP.

Each entry has a unique ID referenced in the source code via `// TECHDEBT(TD-XXX):`
comments. Run `grep -r 'TECHDEBT' crates/` to find all markers.

---

## Active Items

| ID | Area | Workaround | Proper Fix | Tests Affected | Files |
|----|------|-----------|------------|----------------|-------|
| TD-001 | Owner identity | Owner ID and DisplayName hardcoded to `"arca"` in ListBuckets XML | Introduce account/user model; derive owner ID from credential; store owner on bucket/object creation | `test_bucket_listv2_fetchowner_notempty` (not yet passing) | `xml_types.rs` |
| TD-002 | Storage class | Always `"STANDARD"` — no storage class field in ObjectRecord | Add `storage_class` field to ObjectRecord, DB column, accept `x-amz-storage-class` header | None currently failing | `types.rs`, `bucket.rs`, `xml_types.rs` |
| TD-003 | Versioning | ListObjectVersions returns `VersionId=null`, `IsLatest=true` for all objects — no actual version tracking | Implement object versioning: version ID generation, version history in DB, delete markers | All `Versioning` category tests | `xml_types.rs`, `bucket.rs` |
| TD-004 | Region | Bucket region hardcoded to `"us-east-1"` in HeadBucket header and GetBucketLocation (empty LocationConstraint = us-east-1) | Add region to server config; store per-bucket region; return in responses | None currently failing | `bucket.rs`, `xml_types.rs` |
| ~~TD-005~~ | ~~Request ID~~ | **RESOLVED** — Error XML placeholder `<RequestId>` is now replaced by the request-ID middleware with the real `x-amz-request-id` value | — | — | — |
| TD-006 | Encryption config | `GET /{bucket}?encryption` always returns `ServerSideEncryptionConfigurationNotFoundError` — correct for "not configured" but there's no way to actually configure encryption | Implement `PutBucketEncryption` / `GetBucketEncryption` with SSE-S3 at minimum | Encryption category tests | `bucket.rs` |
| TD-007 | Unimplemented ops | ~35 bucket-level GET/PUT operations return 501 NotImplemented (ACLs, lifecycle, versioning, CORS, policies, etc.) | Implement each feature as needed post-MVP | All tests in skipped/unimplemented categories | `bucket.rs` |
| TD-008 | Content-Type source | Multipart upload captures Content-Type from `CreateMultipartUpload` request — S3 spec is ambiguous but some clients set it at init time | Verify this matches AWS behavior; potentially accept from CompleteMultipartUpload too | None currently failing | `multipart.rs` |

---

## Legend

- **Tests Affected**: Ceph s3-tests that are impacted (either currently failing, or passing due to the workaround)
- **Files**: Source files containing the workaround (abbreviated — full paths under `crates/`)
- Items are removed from this table when properly implemented

## How to Use

1. When adding a new workaround, assign the next `TD-XXX` ID
2. Add a `// TECHDEBT(TD-XXX): brief description` comment in the source code
3. Add a row to the table above
4. When implementing the proper fix, remove both the code comment and the table row
