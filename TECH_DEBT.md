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
| TD-014 | `recover` / `fsck` composite blobs | Composite blobs (the result of the optimised `CompleteMultipartUpload`) live as a sidecar that lists the part blob ids — no on-disk file exists at the composite's blob path. `recover::process_sidecar` rejects them as "orphaned sidecar (blob file missing)" and aborts the rebuild. `fsck` reports the same sidecar as an `orphaned_sidecars` entry (the parts themselves are not orphaned because they each carry their own sidecar). Both tools therefore produce false positives on any data directory that has experienced multipart uploads after the composite-concat optimisation. Runtime S3 reads/writes are unaffected — the request path fully handles composites. | (1) `recover`: when a sidecar has `composite: Some(parts)`, skip the blob-existence check, walk the part list, and verify each referenced part blob exists. Insert one `ObjectRecord` per composite that points at the composite blob_id (the part records are not surfaced to the metadata DB — they are a storage-layer detail). (2) `fsck`: treat composite sidecars as valid (no blob file expected); cross-check each referenced part exists; report a new `dangling_part` category when a composite references a missing part. Add unit tests for both tools using fixtures that mix composite and ordinary blobs. | None at the runtime level; recovery / fsck workflows on data with composites currently fail | `crates/arca-server/src/recover.rs:206`, `crates/arca-server/src/fsck.rs` (`orphaned_sidecars` collection, around line 112) |
| TD-015 | Cluster inter-node TLS cert verification | The cluster membership health-probe and the cluster transport client build their `reqwest` clients with `danger_accept_invalid_certs(true)` so peers presenting self-signed certs under clustered TLS can be reached. Inter-node requests are authenticated by SigV4 + the shared `[cluster].secret` (signed headers), not by TLS, so accepting the cert does **not** weaken request authentication — TLS is confidentiality-only on this path. The residual risk is purely confidentiality: a network MITM could read/inject inter-node traffic (Phase 29 HA, M1/M2). | Distribute a shared cluster CA (or pin peer cert fingerprints advertised via discovery), build the cluster `reqwest` clients with that root, and drop `danger_accept_invalid_certs`. | None | `crates/arca-server/src/cluster/membership.rs`, `crates/arca-server/src/cluster/client.rs` |

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
| TD-013 | AMQP/Kafka connector tests | Root cause was NOT DNS: the AMQP receiver crashed on startup because `docker exec rabbitmq-diagnostics` (run as root in a tight `wait_for_amqp_receiver` loop) raced with RabbitMQ's cookie initialization, producing an EACCES error that killed the container. Fix: replace the `exec`-based wait for AMQP/Kafka with a `docker inspect` health poll (`_wait_for_container_healthy`), so the receiver is probed by the container's own healthcheck only. Kafka tests also had two unrelated test-side issues: consumer-group coordination raced with the first produce (fixed by switching to manual partition assign + `seek_to_end`), and the connectivity-failure test timed out because Arca's Kafka `test()` can take ~10s on unreachable hosts while the test HTTP timeout was also 10s (fixed by raising the conftest SigV4 default to 30s). | <span style="color:#4caf50">&#x2714;</span> |
