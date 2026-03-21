# Technical Debt

Workarounds, hardcoded values, and temporary fixes that pass S3 compatibility
tests or satisfy client expectations but need proper implementation post-MVP.

Each entry has a unique ID referenced in the source code via `// TECHDEBT(TD-XXX):`
comments. Run `grep -r 'TECHDEBT' crates/` to find all markers.

---

## Active Items

| ID | Area | Workaround | Proper Fix | Tests Affected | Files |
|----|------|-----------|------------|----------------|-------|
| TD-001 | Owner identity | RESOLVED ✅ — Owner ID derived from credential's user. Buckets and objects store creator's username. Migration v8 adds `owner` columns and `user_id` on credentials | — | — | — |
| TD-002 | Storage class | Always `"STANDARD"` — no storage class field in ObjectRecord | Add `storage_class` field to ObjectRecord, DB column, accept `x-amz-storage-class` header | None currently failing | `types.rs`, `bucket.rs`, `xml_types.rs` |
| TD-003 | Versioning | RESOLVED ✅ — Full object versioning implemented in Phase 17. PutBucketVersioning/GetBucketVersioning, version IDs, delete markers, version-specific GET/HEAD/DELETE/COPY, ListObjectVersions with real data. Migration v9 adds versioning columns to objects table | — | — | — |
| TD-004 | Region | RESOLVED ✅ — Region configurable in `[server]` TOML section or via Admin API (`/admin/settings/region`). Per-bucket region via `bucket_config`. HeadBucket and GetBucketLocation return effective region. Phase 18 | — | — | — |
| TD-005 | Request ID | RESOLVED ✅ — Error XML placeholder `<RequestId>` is now replaced by the request-ID middleware with the real `x-amz-request-id` value | — | — | — |
| TD-006 | Encryption config | RESOLVED ✅ — `PutBucketEncryption` / `GetBucketEncryption` / `DeleteBucketEncryption` implemented in Phase 13 with SSE-S3 (AES-256-GCM). Per-bucket config stored in `bucket_config` table, global default from `[encryption]` config section | — | — | — |
| TD-007 | Unimplemented ops | ~33 bucket-level GET/PUT operations return 501 NotImplemented (ACLs, lifecycle, CORS, policies, etc.) | Implement each feature as needed post-MVP | All tests in skipped/unimplemented categories | `bucket.rs` |
| TD-008 | Content-Type source | Multipart upload captures Content-Type from `CreateMultipartUpload` request — S3 spec is ambiguous but some clients set it at init time | Verify this matches AWS behavior; potentially accept from CompleteMultipartUpload too | None currently failing | `multipart.rs` |
| TD-005 | Audit write contention | Every S3/admin request inserts one row into `audit_log` synchronously via `tokio::spawn`. Under heavy load, SQLite write contention may become a bottleneck | Batch audit writes using a bounded `tokio::sync::mpsc` channel with a dedicated writer task doing bulk inserts | None currently failing | `middleware/audit.rs` |
| TD-010 | SSE-C multipart | SSE-C headers on CreateMultipartUpload and UploadPart are rejected with `InvalidArgument`. SSE-C + multipart requires encrypting each part independently and tracking per-part nonce prefixes | Implement SSE-C for multipart: per-part encryption, nonce tracking, assembly of encrypted parts at complete time | `test_ssec.py::TestSsecMultipartRejection` | `multipart.rs`, `object.rs` |

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
