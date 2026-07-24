<p align="center">
  <img src="logo.svg" width="120" alt="Arca logo">
</p>

<h1 align="center">Arca</h1>

<p align="center">
  Open source S3-compatible object storage server written in Rust.<br>
  <a href="https://dxc-technology.github.io/arca/">Documentation</a> · <a href="CHANGELOG.md">Changelog</a> · <a href="https://dxc-technology.github.io/arca/report.html">Project Report</a>
</p>

---

Arca is a ground-up implementation of the S3 API, designed as a **drop-in replacement** for AWS S3, MinIO, and other S3-compatible storage services. It runs as a single node or as a symmetric, self-configuring high-availability cluster, with at-rest encryption, object versioning, RBAC, lifecycle rules, object lock, event notifications, and a web console.

## Status

Production-grade and actively developed. **29 of 31** planned phases are complete (latest release **v0.27.1**), covering 60+ S3 operations plus encryption, versioning, RBAC, monitoring, lifecycle, object lock, tagging, notifications, compression, replication, and high-availability clustering. Arca passes the [Ceph s3-tests](https://github.com/ceph/s3-tests) compatibility suite with **369/825 passing and 0 unexpected failures** (100% pass rate on implemented features; RGW-only extensions out of scope). See the [roadmap](https://dxc-technology.github.io/arca/roadmap/) for what's next.

## Features

- **S3 API compatible** — works with aws-cli, boto3, MinIO Client (mc), rclone, and any S3 client
- **High availability** — symmetric, self-configuring, fully-replicated cluster with real-time replication, anti-entropy self-healing, and quorum / available consistency modes (no shared storage, no coordinator)
- **Object versioning** — version IDs, delete markers, version-specific operations
- **Access control (RBAC)** — users, teams, grants, and bucket policies with SigV4 auth
- **Server-side encryption** — AES-256-GCM at-rest (SSE-S3) with envelope encryption and per-object DEKs; master key from config or Vault/OpenBAO KMS; SSE-C and per-bucket encryption supported
- **Lifecycle & Object Lock** — expiration/noncurrent rules with a background worker; WORM retention and legal hold (COMPLIANCE/GOVERNANCE)
- **Event notifications** — bucket notifications delivered to 13 connectors (webhook, Kafka, AMQP, Redis, NATS, MQTT, PostgreSQL, MySQL, MongoDB, Elasticsearch, Syslog, SMTP, gRPC)
- **Replication** — one-way per-rule replication to any S3-compatible destination, loop-safe for mirrors
- **Transparent compression** — per-bucket at-rest compression (zstd, lz4, snappy, gzip, brotli, xz)
- **Pluggable metadata backend** — SQLite (default) or PostgreSQL, behind traits
- **Streaming-first** — never buffers full objects in memory; concurrent hashing during upload
- **Disaster recovery & integrity** — sidecar `.meta` files enable full DB rebuild (`arca recover`); `arca fsck` checks filesystem/DB consistency
- **Native TLS** — HTTPS without a reverse proxy, with auto-detection and SIGHUP cert reload
- **Monitoring** — Prometheus metrics, audit log, and an admin API under `/admin/*`
- **Web console** — browser-based UI for buckets, objects, credentials, users, policies, notifications, replication, and cluster topology
- **Presigned URLs** — query-string auth for GET/PUT/HEAD/DELETE
- **S3 compatibility tested** — 369/825 Ceph s3-tests passing, 0 unexpected failures

## S3 API Surface

| Category  | Operations                                                            |
|-----------|-----------------------------------------------------------------------|
| Bucket    | CreateBucket, DeleteBucket, HeadBucket, ListBuckets, Put/Get/DeleteBucketEncryption |
| Object    | PutObject, GetObject, DeleteObject, HeadObject, CopyObject, GetObjectAttributes (+ SHA256/CRC32 checksums) |
| Listing   | ListObjectsV1, ListObjectsV2, DeleteObjects (batch)                   |
| Multipart | CreateMultipartUpload, UploadPart, UploadPartCopy, CompleteMultipartUpload, AbortMultipartUpload, ListMultipartUploads, ListParts |
| Versioning | Put/GetBucketVersioning, ListObjectVersions, version-specific Get/Head/Delete, delete markers |
| Tagging   | Put/Get/DeleteObjectTagging, Put/Get/DeleteBucketTagging              |
| Lifecycle | Put/Get/DeleteBucketLifecycleConfiguration                           |
| Object Lock | Put/GetObjectLockConfiguration, Put/GetObjectRetention, Put/GetObjectLegalHold |
| Policy    | Put/Get/DeleteBucketPolicy (RBAC: users, teams, grants)              |
| Notifications | Put/GetBucketNotificationConfiguration                           |
| Auth      | AWS Signature V4 (header-based and query-string / presigned)         |

### S3 Features Supported

- Prefix/delimiter listing with pagination and continuation tokens
- Byte range requests (start-end, suffix, open-ended)
- Conditional requests (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since)
- User metadata (`x-amz-meta-*`) and system metadata (Cache-Control, Content-Encoding, Content-Disposition, Content-Language, Expires)
- CopyObject with COPY/REPLACE metadata directive
- GetObject response overrides (response-content-type, response-cache-control, etc.)
- DeleteObjects with per-key conditionals (ETag, LastModifiedTime, Size)
- CompleteMultipartUpload with conditional headers and part deduplication
- Encoding-type=url for ListObjects responses
- Unicode metadata support
- Server-side encryption (SSE-S3): AES-256-GCM with per-object DEKs and master key envelope encryption

## Quick Start

```bash
# Start the server
bin/arca start -d --build

# Or with HTTPS (place cert + key PEM files in ./certs/ first):
bin/arca start -d --build --tls

# Credentials are printed on first startup — check the logs
bin/arca logs | grep "Access Key"

# Use aws-cli
export AWS_ACCESS_KEY_ID=<from logs>
export AWS_SECRET_ACCESS_KEY=<from logs>

aws s3 mb s3://my-bucket --endpoint-url http://localhost:9000
aws s3 cp myfile.txt s3://my-bucket/ --endpoint-url http://localhost:9000
aws s3 ls s3://my-bucket --endpoint-url http://localhost:9000
```

### Web Console

```bash
bin/console start -d --build
```

Open [http://localhost:9080](http://localhost:9080), enter the Arca endpoint (`http://localhost:9000`) and your credentials.

### Admin API

```bash
# Health check
curl http://localhost:9000/admin/health

# Server info and storage stats (requires SigV4 signing)
aws --endpoint-url http://localhost:9000 s3api head-bucket --bucket admin-info  # use any S3 client with SigV4
```

## Architecture

Five-crate Cargo workspace with strict dependency graph (no cycles):

```
arca-server → arca-proto, arca-storage, arca-auth → arca-core
```

| Crate | Role |
|-------|------|
| `arca-core` | Shared types, traits (`BlobStore`, `MetadataStore`), errors. Zero I/O dependencies. |
| `arca-auth` | AWS SigV4 verification. Zero I/O, independently testable against AWS test vectors. |
| `arca-proto` | S3 HTTP protocol adapter (Axum 0.8 + Tower). Handlers, XML ser/de, middleware. |
| `arca-storage` | Storage implementations: filesystem blobs (UUID + sidecar), SQLite (WAL mode) or PostgreSQL metadata, with optional encryption/compression/cluster decorators. |
| `arca-server` | Binary. TOML config, CLI (`serve`, `recover`, `fsck`, `credential`, `tls`), dependency wiring. |

### Key Design Decisions

- **No `s3s` crate** — custom S3 HTTP protocol adapter with Axum, avoiding pre-1.0 dependency risk
- **Streaming I/O** — PutObject streams through MD5 hasher + file writer concurrently; GetObject streams from disk via ReaderStream
- **Crash-safe write order** — blob file → sidecar `.meta` JSON → SQLite insert; enables `arca recover` to rebuild the database from the filesystem
- **Separated web console** — keeps the binary small (8.6 MB scratch image), allows independent release cycles, minimizes attack surface

## CLI

```bash
arca serve                          # start the server
arca serve --config-path ./my.toml  # custom config
arca credential add                 # create new credential
arca credential add --admin         # create admin credential
arca credential list                # list all credentials
arca credential remove <key_id>     # remove a credential
arca tls generate                   # generate self-signed CA + server cert
arca tls generate --sans "host,ip"  # custom SANs
arca recover                        # rebuild DB from sidecar files
arca recover --dry-run              # preview recovery without writing
arca fsck                           # check filesystem/DB consistency
arca fsck --verify-checksums        # also verify MD5 of every blob
```

## Development

All development happens inside Docker containers — no local toolchain required.

```bash
bin/build                # build Docker image
bin/arca start -d --build --dev  # start in development mode (debian-slim, has shell)
bin/arca start -d --build --tls  # start with HTTPS (certs in ./certs/)
bin/arca stop            # stop server
bin/arca logs -f         # follow server logs
bin/test                 # run all tests (unit + integration)
bin/test unit            # unit tests only
bin/test integration     # integration tests only
bin/test tls             # TLS integration tests
bin/s3-tests             # run Ceph s3-tests compatibility suite
bin/docs-serve           # serve documentation locally (http://localhost:8000)
```

## Test Coverage

| Suite | Tests | Details |
|-------|------:|---------|
| Unit tests (Rust) | 911 | arca-auth: 39, arca-core: 248, arca-proto: 64, arca-server: 280, arca-storage: 280 |
| Integration — boto3 | 146 | buckets, objects, list, multipart, copy, folders, auth, admin, credentials, conditional ops |
| Integration — RBAC | 42 | user/team/grant CRUD, attachments, effective grants, /admin/me (with and without grants), E2E access control |
| Integration — Versioning | 21 | versioning config, PUT/GET/HEAD/DELETE with versionId, delete markers, batch delete with VersionId, ListVersions, copy |
| Integration — Tagging | 23 | Put/Get/Delete object tags, tag limits and validation, tagging on versioned objects, tag-directive on copy |
| Integration — Monitoring | 29 | Prometheus metrics, audit log, metrics history, instance settings, preview limits, region (TD-004) |
| Integration — Lifecycle | 17 | Put/Get/Delete lifecycle config, expiration rules, noncurrent version, abort upload, tag/and filters |
| Integration — Object Lock | 19 | Put/Get config (at creation, on empty existing buckets, rejection on non-empty), retention, legal hold, COMPLIANCE/GOVERNANCE enforcement, bypass, delete markers |
| Integration — API Completeness | 11 | ListParts, GetObjectAttributes, checksum SHA256/CRC32 roundtrip, storage class, pagination |
| Integration — Hardening | 7 | body size limits, health endpoint, metadata headers, cache transparency and invalidation |
| Integration — Presigned URLs | 17 | presigned GET/PUT/HEAD/DELETE, security, admin presign, special chars, endpoint override |
| Integration — SSE-C | 17 | put/get roundtrip, error handling, head, copy, range, delete, validation, multipart rejection |
| Integration — Encryption | 16 | encrypted put/get, ETag, range reads, multipart, copy, bucket config |
| Integration — Per-bucket Encryption | 8 | per-bucket enable/disable, plain vs encrypted, ETag, head, revert |
| Integration — KMS | 10 | Vault/OpenBAO key fetch, encrypted put/get, headers, ETag, multipart, copy, range, admin info |
| Integration — TLS | 7 | HTTPS health/info/put/get/multipart, minio client, wrong CA rejection |
| Integration — PostgreSQL | 21 | buckets, objects, multipart, versioning, tags, lifecycle, copy, range, admin health, concurrent-write smoke (commit-ordered seq) |
| Integration — Export/Import | 20 | export all/single/multiple sections, secret masking, import dry_run/skip/overwrite, masked credentials, node-identity guard, bucket create/skip, round-trip |
| Integration — Notifications | 20 | Put/Get config (Topic/Queue/Lambda), filters, webhook delivery, event format, batch delete, admin API, auth token, connector type roundtrip |
| Integration — Compression | 10 | put/get roundtrip, ETag vs plaintext MD5, MIME skip, small object skip, range reads (intra- and cross-frame), per-bucket ?compression subresource (PUT/GET/DELETE), every algorithm (zstd/lz4/snappy/gzip/brotli/xz), unknown-algorithm rejection |
| Integration — MinIO | 99 | mirrors boto3 suite + streaming, file-based, data integrity APIs |
| Integration — Replication | 4 | basic PutObject replication, delete-marker propagation, tag sync, two-way mirror no-loop invariant |
| Integration — HA Cluster | 59 | 3-node replication to all nodes, read-after-write, write with one node down (quorum), failover read, read-only without quorum (503), anti-entropy catch-up (objects + the full control plane: bucket, credential, grant attach/detach, bucket versioning + tag set, Object-Lock retention, multipart abort/complete — including CompleteMultipartUpload on a returned node fetching part bytes from a peer), real network partition (isolated node 503s while reads keep working, majority side writes, heal convergence), available mode (split-brain writes to the same key with single LWW winner at heal, 1/3 minority still writable), cluster-wide 507 InsufficientStorage, config-drift detection + drift quorum exclusion (1 drifted node → still writable; drifted majority → 503 while reads keep working), worker-leader gate (exactly one node — the lowest eligible node_id — runs the lifecycle evaluator: expiry happens exactly once cluster-wide, with automatic leader failover when that node stops), syncing readiness (a restarted node answers 503 "syncing" until its first anti-entropy pass completes, and the data written during its downtime is readable the moment it reports ready), per-node admin views (`?node=` proxy really reaches a specific node's own audit log, self short-circuit, all-nodes merge ordered/labeled with per-source report, 404 on an unknown node, all four families proxied, 503 on a dead target, merged view shrinking to the eligible sources), write-aware health (`?writable=1` 200 cluster-wide, 503 read_only on a quorum-less survivor whose plain health stays 200), proactive blob repair (a payload file deleted from a node's volume restored by the sweep — observed on the volume, never via a GET that would mask it with lazy read-repair — and served intact), tombstone-GC liveness guard (a deletion while a node is down beyond the grace blocks GC on both survivors; at re-entry the deletion is learned everywhere — no resurrection — and the guard releases) |
| Integration — Maintenance jobs | 8 | admin API: auth, job-type/mode validation, no-op job lifecycle, single-job lock, pause/resume/cancel transitions, 404 on unknown job |
| Integration — Re-encryption | 2 | hot encrypt→decrypt round-trip (copy-on-write, ETag preserved), no-candidate job completes cleanly |
| Integration — Migrate DB | 1 | SQLite→PostgreSQL offline metadata migration + row-count verify |
| Integration — Migrate Topology | 1 | single→cluster config emission + DB ops, then →single round-trip |
| Connector integrations | 84 | Redis, NATS, MQTT, PostgreSQL, MySQL, MongoDB, Kafka, AMQP, Elasticsearch, Syslog, SMTP, gRPC — each: delivery, custom destination, delete event, multiple events, payload format, connectivity test |
| **Arca tests** | **1,626** | **All tests written for this project** |
| [Ceph s3-tests](https://dxc-technology.github.io/arca/s3-compatibility/) | 825 | 369 pass, 365 fail, 91 skip — 0 unexpected failures (RGW-only extensions excluded) |
| **Total** | **1,995** | |

## License

Arca is licensed under the [GNU Affero General Public License](LICENSE-AGPL-3.0), either version 3 of the License or (at your option) any later version (SPDX: `AGPL-3.0-or-later`), a strong copyleft license covering network use: derivative works must remain under the same terms.
