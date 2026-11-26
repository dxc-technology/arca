<p align="center">
  <img src="logo.svg" width="120" alt="Arca logo">
</p>

<h1 align="center">Arca</h1>

<p align="center">
  Open source S3-compatible object storage server written in Rust.<br>
  <a href="https://dxc-technology.github.io/arca/">Documentation</a> · <a href="CHANGELOG.md">Changelog</a>
</p>

---

Arca is a ground-up implementation of the S3 API, designed as a **drop-in replacement** for AWS S3, MinIO, and other S3-compatible storage services. It runs as a single node or as a symmetric, self-configuring high-availability cluster, with at-rest encryption, object versioning, RBAC, lifecycle rules, object lock, event notifications, and a web console.

## Status

Production-grade and actively developed. **30 of 31** planned phases are complete (latest release **v0.36.0**), covering 60+ S3 operations plus encryption, versioning, RBAC, monitoring, lifecycle, object lock, tagging, notifications, compression, replication, and high-availability clustering. Arca passes the [Ceph s3-tests](https://github.com/ceph/s3-tests) compatibility suite with **383/825 passing and 0 unexpected failures** (100% pass rate on implemented features; RGW-only extensions out of scope). See the [roadmap](https://dxc-technology.github.io/arca/roadmap/) for what's next.

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
- **S3 compatibility tested** — 383/825 Ceph s3-tests passing, 0 unexpected failures

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

> **Conditional requests are atomic compare-and-swap** on a single node (and within one node in a cluster): `If-Match`/`If-None-Match`/`x-amz-if-match-*` preconditions are evaluated authoritatively inside the same transaction that performs the write, so a concurrent conditional write to the same key never races to a false `200 OK`. In a cluster this guarantee is **per node, not cluster-wide** (TD-025): two conditional writes for the same key landing on two different nodes at once can both be accepted, converging afterward by last-writer-wins. See the [HA guide](documentation/docs/guide/ha.md#design-trade-offs-and-honest-limitations) for the mitigation (sticky load-balancer routing).

## Quick Start

```bash
# Start the server
bin/arca start -d --build

# Or, with no build at all, run a published image (public, linux/amd64 + linux/arm64):
bin/arca start -d --release latest
# ...or plain Docker, without even cloning:
docker run -d --name arca -p 9000:9000 -v arca-data:/data ghcr.io/dxc-technology/arca:latest

# Or with HTTPS: your cert + key PEM files in ./certs/, or a local CA made for you
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
# Start the console (after the server: it inherits TLS from it)
bin/console start -d --build

# Or, with no build at all, run the published image (public, linux/amd64 + linux/arm64):
bin/console start -d --release latest
# ...or plain Docker, without even cloning:
docker run -d --name arca-console -p 9080:8080 \
    -e ARCA_ENDPOINT=http://localhost:9000 ghcr.io/dxc-technology/arca-console:latest

# With HTTPS the console follows the server started with --tls (no flag of its own)
```

Open [http://localhost:9080](http://localhost:9080), enter the Arca endpoint (`http://localhost:9000`) and your credentials. The console listens on 8080 (HTTP) and 8443 (HTTPS) inside the container, mapped to 9080 and 9443 on the host by the scripts.

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
arca credential add                 # create new credential (root user)
arca credential add --user <id>     # create credential for another user
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
bin/arca start -d --build --tls  # start with HTTPS (your certs in ./certs/, or local ones)
bin/arca stop            # stop server
bin/arca logs -f         # follow server logs
bin/test                 # run all tests (unit + integration)
bin/test unit            # unit tests only
bin/test integration     # integration tests only
bin/test tls             # TLS integration tests
bin/test tls-permissions # TLS file-permission tests (non-root containers)
bin/s3-tests             # run Ceph s3-tests compatibility suite
bin/docs-serve           # serve documentation locally (http://localhost:8000)
```

## Test Coverage

<!--
How to update this table (keep it in sync with AGENTS.md, "Test Coverage Table"):
- Each row sums the suites of the `bin/test` modes listed in its Details column. Recount the affected rows, then recompute the two bold totals from the rows (Arca tests = all rows above it, Total = Arca tests + Ceph).
- Unit row: the per-crate counts in Details must add up to the row, plus the one `tests/open_files_limit.rs` test.
- Ceph row: copy total, passed, failed, skipped and unexpected failures from `s3-tests/summary.json` after `bin/s3-tests`.
-->

| Suite | Tests | Details |
|-------|------:|---------|
| Unit tests (Rust) | 1452 | `bin/test unit`: arca-auth 47, arca-core 289, arca-proto 232, arca-server 413, arca-storage 470, plus 1 integration test (`tests/open_files_limit.rs`) |
| S3 API integration | 570 | `bin/test integration`, `virtual-host`, `presigned`, `compression`: boto3 and MinIO clients, multipart, copy, versioning, tagging, lifecycle, Object Lock, conditional writes, checksums and body integrity, aws-chunked signatures, virtual-hosted style, presigned URLs, compression |
| Access control and admin | 141 | `bin/test integration`: RBAC and its side doors, monitoring and audit, export/import, maintenance jobs |
| Encryption and TLS | 72 | `bin/test encryption`, `per-bucket-encryption`, `ssec`, `kms`, `recrypt`, `tls`, `tls-permissions`: SSE-S3, SSE-C, SSE-KMS, re-encryption, HTTPS and certificate permissions |
| Storage backends | 33 | `bin/test postgres`, `migrate-db`, `migrate-topology`: PostgreSQL metadata, SQLite to PostgreSQL migration, single node to cluster migration |
| Cluster and replication | 72 | `bin/test cluster`, `replication`: 3-node HA (quorum, failover, partitions, anti-entropy, repair) and bucket replication |
| Notifications and connectors | 105 | `bin/test notifications`, `connectors`: event delivery and the 12 connectors |
| Shell libraries and tooling | 303 | `bin/test scripts`, `s3-report`, `perf-report`: `bin/lib/*.sh` with `docker` and `gh` mocked, Ceph report and perf suite tooling |
| **Arca tests** | **2,748** | **All tests written for this project** |
| [Ceph s3-tests](https://dxc-technology.github.io/arca/s3-compatibility/) | 825 | 383 pass, 351 fail, 91 skip, 0 unexpected failures (RGW-only extensions excluded; 2 pass on AWS behaviour where RGW differs) |
| **Total** | **3,573** | |

## License

Arca is licensed under the [GNU Affero General Public License](LICENSE-AGPL-3.0), either version 3 of the License or (at your option) any later version (SPDX: `AGPL-3.0-or-later`), a strong copyleft license covering network use: derivative works must remain under the same terms.

> **ℹ️ Contains AI-generated code** — substantially all of Arca's source code, tests and documentation was generated with AI assistance, under human direction and review. See [NOTICE](NOTICE) for details.
