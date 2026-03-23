# Technical Debt

Workarounds, hardcoded values, and temporary fixes that pass S3 compatibility
tests or satisfy client expectations but need proper implementation post-MVP.

Each entry has a unique ID referenced in the source code via `// TECHDEBT(TD-XXX):`
comments. Run `grep -r 'TECHDEBT' crates/` to find all markers.

---

## Active Items

| ID | Area | Workaround | Proper Fix | Tests Affected | Files |
|----|------|-----------|------------|----------------|-------|
| TD-002 | Storage class | Always `"STANDARD"` — no storage class field in ObjectRecord | Add `storage_class` field to ObjectRecord, DB column, accept `x-amz-storage-class` header | None currently failing | `types.rs`, `bucket.rs`, `xml_types.rs` |
| TD-007 | Unimplemented ops | ~33 bucket-level GET/PUT operations return 501 NotImplemented (ACLs, lifecycle, CORS, policies, etc.) | Implement each feature as needed post-MVP | All tests in skipped/unimplemented categories | `bucket.rs` |
| TD-008 | Content-Type source | Multipart upload captures Content-Type from `CreateMultipartUpload` request — S3 spec is ambiguous but some clients set it at init time | Verify this matches AWS behavior; potentially accept from CompleteMultipartUpload too | None currently failing | `multipart.rs` |
| TD-009 | Audit write contention | Every S3/admin request inserts one row into `audit_log` synchronously via `tokio::spawn`. Under heavy load, SQLite write contention may become a bottleneck | Batch audit writes using a bounded `tokio::sync::mpsc` channel with a dedicated writer task doing bulk inserts | None currently failing | `middleware/audit.rs` |
| TD-010 | SSE-C multipart | SSE-C headers on CreateMultipartUpload and UploadPart are rejected with `InvalidArgument`. SSE-C + multipart requires encrypting each part independently and tracking per-part nonce prefixes | Implement SSE-C for multipart: per-part encryption, nonce tracking, assembly of encrypted parts at complete time | `test_ssec.py::TestSsecMultipartRejection` | `multipart.rs`, `object.rs` |
| TD-011 | `time` crate CVE | `time` pinned to 0.3.41 (CVE-2026-25727: stack exhaustion via RFC 2822 parsing, CVSS 6.5). Not exploitable in Arca (only uses `OffsetDateTime`/`Duration` for cert generation, never RFC 2822 parsing). Fix requires `time` >= 0.3.47 which needs Rust >= 1.88 | Upgrade Rust toolchain to >= 1.88, unpin `time` in `Cargo.toml` | None | `Cargo.toml`, `tls_generate.rs` |
| TD-012 | `rustls-pemfile` unmaintained | `rustls-pemfile` 2.2.0 is unmaintained (RUSTSEC-2025-0134). PEM parsing functionality moved to `rustls-pki-types`. No security vulnerability, just maintenance risk | Migrate PEM parsing from `rustls-pemfile` to `rustls-pki-types` | None | `tls.rs`, `Cargo.toml` |

---

## Legend

- **Tests Affected**: Ceph s3-tests that are impacted (either currently failing, or passing due to the workaround)
- **Files**: Source files containing the workaround (abbreviated — full paths under `crates/`)
- Items are moved to the Resolved table when properly implemented

## How to Use

1. When adding a new workaround, assign the next `TD-XXX` ID
2. Add a `// TECHDEBT(TD-XXX): brief description` comment in the source code
3. Add a row to the Active Items table above
4. When implementing the proper fix, remove the code comment and move the row to the Resolved table below

---

## Resolved Items

| ID | Area | Resolution | Status |
|----|------|------------|:------:|
| TD-001 | Owner identity | Owner ID derived from credential's user. Buckets and objects store creator's username. Migration v8 adds `owner` columns and `user_id` on credentials. Phase 16 | <span style="color:#4caf50">&#x2714;</span> |
| TD-003 | Versioning | Full object versioning: PutBucketVersioning/GetBucketVersioning, version IDs, delete markers, version-specific GET/HEAD/DELETE/COPY, ListObjectVersions with real data. Migration v9 adds versioning columns. Phase 17 | <span style="color:#4caf50">&#x2714;</span> |
| TD-004 | Region | Region configurable in `[server]` TOML section or via Admin API (`/admin/settings/region`). Per-bucket region via `bucket_config`. HeadBucket and GetBucketLocation return effective region. Phase 18 | <span style="color:#4caf50">&#x2714;</span> |
| TD-005 | Request ID | Error XML placeholder `<RequestId>` replaced by the request-ID middleware with the real `x-amz-request-id` value | <span style="color:#4caf50">&#x2714;</span> |
| TD-006 | Encryption config | `PutBucketEncryption` / `GetBucketEncryption` / `DeleteBucketEncryption` implemented with SSE-S3 (AES-256-GCM). Per-bucket config in `bucket_config` table, global default from `[encryption]` config section. Phase 13 | <span style="color:#4caf50">&#x2714;</span> |
