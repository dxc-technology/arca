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
| TD-017 | `time` pinned to =0.3.47 | `time` 0.3.48 adds `From` impls that clash (E0119 coherence error) with `rcgen`'s blanket `From` conversions whenever time's `parsing`/`formatting` features are enabled elsewhere in the graph (they are, via transitive deps). 0.3.47 already contains the CVE-2026-25727 fix, so no security exposure — this is a freshness pin only | Unpin when rcgen or time resolves the conflict upstream (track rcgen releases past 0.14.8) | None | `Cargo.toml` |
| TD-014 | `recover` / `fsck` composite blobs | Composite blobs (the result of the optimised `CompleteMultipartUpload`) live as a sidecar that lists the part blob ids — no on-disk file exists at the composite's blob path. `recover::process_sidecar` rejects them as "orphaned sidecar (blob file missing)" and aborts the rebuild. `fsck` reports the same sidecar as an `orphaned_sidecars` entry (the parts themselves are not orphaned because they each carry their own sidecar). Both tools therefore produce false positives on any data directory that has experienced multipart uploads after the composite-concat optimisation. Runtime S3 reads/writes are unaffected — the request path fully handles composites. | (1) `recover`: when a sidecar has `composite: Some(parts)`, skip the blob-existence check, walk the part list, and verify each referenced part blob exists. Insert one `ObjectRecord` per composite that points at the composite blob_id (the part records are not surfaced to the metadata DB — they are a storage-layer detail). (2) `fsck`: treat composite sidecars as valid (no blob file expected); cross-check each referenced part exists; report a new `dangling_part` category when a composite references a missing part. Add unit tests for both tools using fixtures that mix composite and ordinary blobs. | None at the runtime level; recovery / fsck workflows on data with composites currently fail | `crates/arca-server/src/recover.rs:206`, `crates/arca-server/src/fsck.rs` (`orphaned_sidecars` collection, around line 112) |

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
| TD-015 | Cluster inter-node TLS cert verification | Verified mutual TLS with an operator-distributed cluster CA (`[cluster.tls]` — REQUIRED when the cluster runs over HTTPS, no insecure fallback): inter-node `reqwest` clients trust the CA and present the node's CA-signed client identity; `danger_accept_invalid_certs` removed from `membership.rs` and `client.rs`. The listener requests client certificates (optional at the TLS layer — S3 clients share the port) and `/cluster/v1/*` refuses requests without a CA-verified one. Material minted by `arca tls generate-cluster`. HA hardening R4 (decision H12) | <span style="color:#4caf50">&#x2714;</span> |
| TD-016 | Cluster control-plane reconcile was partial | The anti-entropy snapshot reconcile now covers EVERY control-plane family: grant attachments (`user_grants`, `team_grants`) and team memberships (per-row `updated_at`, migrations sqlite v22 / pg 0010, composite-key tombstones on detach/remove), `bucket_config` per-key LWW, `bucket_tags` as one set-level entity per bucket, `server_config` per-key LWW (node-local keys excluded at build AND on apply), plus in-progress multipart uploads and parts (D4). Child upserts are parent-filtered in `plan_control_merge` (a row whose parent resolved deleted is never adopted — replaces cascade tombstones and respects the PG join-table FKs). A returning node now fully self-heals on all families. HA hardening R5 (review D9/D4) | <span style="color:#4caf50">&#x2714;</span> |
| TD-011 | `time` crate CVE | Docker builder moved from `rust:1.85-alpine` to `rust:alpine` (currently 1.96); `time` updated to 0.3.47, which contains the CVE-2026-25727 fix. The MSRV pins for `home`/`serde_with`/`darling` in the Dockerfile became unnecessary and were removed. (0.3.48+ stays excluded for a non-security reason — see TD-017.) Dependency refresh, 2026-06-12 | <span style="color:#4caf50">&#x2714;</span> |
| TD-012 | `rustls-pemfile` unmaintained | PEM parsing migrated to `rustls-pki-types` (`PemObject`: `pem_slice_iter` / `from_pem_slice`) in `tls.rs` and `tls_generate.rs`; the `rustls-pemfile` dependency is gone from the workspace. Dependency refresh, 2026-06-12 | <span style="color:#4caf50">&#x2714;</span> |
| TD-013 | AMQP/Kafka connector tests | Root cause was NOT DNS: the AMQP receiver crashed on startup because `docker exec rabbitmq-diagnostics` (run as root in a tight `wait_for_amqp_receiver` loop) raced with RabbitMQ's cookie initialization, producing an EACCES error that killed the container. Fix: replace the `exec`-based wait for AMQP/Kafka with a `docker inspect` health poll (`_wait_for_container_healthy`), so the receiver is probed by the container's own healthcheck only. Kafka tests also had two unrelated test-side issues: consumer-group coordination raced with the first produce (fixed by switching to manual partition assign + `seek_to_end`), and the connectivity-failure test timed out because Arca's Kafka `test()` can take ~10s on unreachable hosts while the test HTTP timeout was also 10s (fixed by raising the conftest SigV4 default to 30s). | <span style="color:#4caf50">&#x2714;</span> |
