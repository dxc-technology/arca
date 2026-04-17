# Technical Debt

Workarounds, hardcoded values, and temporary fixes that pass S3 compatibility
tests or satisfy client expectations but need proper implementation post-MVP.

Each entry has a unique ID referenced in the source code via `// TECHDEBT(TD-XXX):`
comments. Run `grep -r 'TECHDEBT' crates/` to find all markers.

---

## Active Items

| ID | Area | Workaround | Proper Fix | Tests Affected | Files |
|----|------|-----------|------------|----------------|-------|
| TD-007 | Unimplemented ops | ~33 bucket-level GET/PUT operations return 501 NotImplemented (ACLs, CORS, policies, etc.) | Implement each feature as needed post-MVP | All tests in skipped/unimplemented categories | `bucket.rs` |
| TD-009 | ~~Audit write contention~~ | **RESOLVED**: Audit writes now batched via bounded mpsc channel (capacity 10,000) with dedicated writer task doing bulk inserts in a single transaction | — | — | `middleware/audit.rs`, `state.rs` |
| TD-010 | SSE-C multipart | SSE-C headers on CreateMultipartUpload and UploadPart are rejected with `InvalidArgument`. SSE-C + multipart requires encrypting each part independently and tracking per-part nonce prefixes | Implement SSE-C for multipart: per-part encryption, nonce tracking, assembly of encrypted parts at complete time | `test_ssec.py::TestSsecMultipartRejection` | `multipart.rs`, `object.rs` |
| TD-011 | `time` crate CVE | `time` pinned to 0.3.41 (CVE-2026-25727: stack exhaustion via RFC 2822 parsing, CVSS 6.5). Not exploitable in Arca (only uses `OffsetDateTime`/`Duration` for cert generation, never RFC 2822 parsing). Fix requires `time` >= 0.3.47 which needs Rust >= 1.88 | Upgrade Rust toolchain to >= 1.88, unpin `time` in `Cargo.toml` | None | `Cargo.toml`, `tls_generate.rs` |
| TD-012 | `rustls-pemfile` unmaintained | `rustls-pemfile` 2.2.0 is unmaintained (RUSTSEC-2025-0134). PEM parsing functionality moved to `rustls-pki-types`. No security vulnerability, just maintenance risk | Migrate PEM parsing from `rustls-pemfile` to `rustls-pki-types` | None | `tls.rs`, `Cargo.toml` |
| TD-013 | AMQP/Kafka connector tests | `docker compose run --rm test` cannot resolve service hostnames (`amqp-receiver`, `kafka-receiver`) from the Python test container, despite Arca itself connecting successfully (verified via admin API curl). Likely a `docker compose run` DNS resolution quirk with the `test` container's network attachment. Server-side connectors are functional | Fix test infrastructure: either use `docker compose exec` on a long-running test container, or add receivers to the test service's `depends_on`, or use IP-based connection in tests | `test_amqp.py`, `test_kafka.py` | `docker-compose.connector-amqp.yml`, `docker-compose.connector-kafka.yml` |

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
| TD-002 | Storage class | `storage_class` field added to ObjectRecord, DB migration v13, `x-amz-storage-class` header accepted on PutObject, returned in list and head responses. Phase 22 | <span style="color:#4caf50">&#x2714;</span> |
| TD-008 | Content-Type source | Verified: Content-Type captured from CreateMultipartUpload matches AWS behavior. Clients set it at init time. Phase 22 | <span style="color:#4caf50">&#x2714;</span> |
