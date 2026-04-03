<p align="center">
  <img src="logo.svg" width="120" alt="Arca logo">
</p>

<h1 align="center">Arca</h1>

<p align="center">
  Open source S3-compatible object storage server written in Rust.<br>
  <a href="https://dxc-technology.github.io/arca/">Documentation</a> · <a href="CHANGELOG.md">Changelog</a> · <a href="https://dxc-technology.github.io/arca/report.html">Project Report</a>
</p>

<p align="center">
  <img src="s3-compatibility-badge.svg" alt="S3 Compatibility">
</p>

---

Arca is a ground-up implementation of the S3 API, designed as a **drop-in replacement** for AWS S3, MinIO, and other S3-compatible storage services. It targets 100% compatibility on a focused subset of operations, starting as a single-node server with a clear path toward production scale.

## MVP Status: Complete

The MVP is complete. All 12 implementation phases (0–11) have been delivered. Arca implements 15 S3 operations with **100% pass rate on implemented features** against the [Ceph s3-tests](https://github.com/ceph/s3-tests) compatibility suite.

## Features

- **S3 API compatible** — works with aws-cli, boto3, MinIO Client (mc), rclone, and any S3 client
- **Streaming-first** — never buffers full objects in memory; concurrent MD5 hashing during upload
- **Disaster recovery** — sidecar `.meta` files alongside every blob enable full database rebuild (`arca recover`)
- **Integrity checking** — `arca fsck` detects orphaned blobs, missing files, sidecar mismatches, and stale temp files
- **Modular storage** — metadata backend behind traits (SQLite now, Postgres later)
- **Admin API** — JSON endpoints for health, stats, and credential management under `/admin/*`
- **Server-side encryption** — AES-256-GCM at-rest encryption (SSE-S3) with envelope encryption, per-object DEKs, and streaming chunk-based encrypt/decrypt. Master key from config file or Vault/OpenBAO KMS
- **Native TLS** — HTTPS without a reverse proxy (`bin/arca start --tls`), with auto-detection and SIGHUP cert reload
- **Web console** — browser-based UI for managing buckets, objects, and credentials
- **S3 compatibility tested** — 338/829 Ceph s3-tests passing, 0 unexpected failures

## S3 API Surface

| Category  | Operations                                                            |
|-----------|-----------------------------------------------------------------------|
| Bucket    | CreateBucket, DeleteBucket, HeadBucket, ListBuckets, PutBucketEncryption, GetBucketEncryption, DeleteBucketEncryption |
| Object    | PutObject, GetObject, DeleteObject, HeadObject, CopyObject            |
| Listing   | ListObjectsV1, ListObjectsV2, DeleteObjects (batch)                   |
| Multipart | CreateMultipartUpload, UploadPart, UploadPartCopy, CompleteMultipartUpload, AbortMultipartUpload, ListMultipartUploads |
| Auth      | AWS Signature V4 (header-based)                                       |

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
| `arca-storage` | Storage implementations: filesystem blobs (UUID + sidecar), SQLite metadata (WAL mode). |
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
| Unit tests (Rust) | 502 | arca-auth: 37, arca-core: 179, arca-proto: 37, arca-server: 77, arca-storage: 172 |
| Integration — boto3 | 146 | buckets, objects, list, multipart, copy, folders, auth, admin, credentials, conditional ops |
| Integration — RBAC | 41 | user/team/grant CRUD, attachments, effective grants, E2E access control |
| Integration — Versioning | 21 | versioning config, PUT/GET/HEAD/DELETE with versionId, delete markers, batch delete with VersionId, ListVersions, copy |
| Integration — Monitoring | 29 | Prometheus metrics, audit log, metrics history, instance settings, preview limits, region (TD-004) |
| Integration — Lifecycle | 17 | Put/Get/Delete lifecycle config, expiration rules, noncurrent version, abort upload, tag/and filters |
| Integration — Object Lock | 16 | Put/Get config, retention, legal hold, COMPLIANCE/GOVERNANCE enforcement, bypass, delete markers |
| Integration — API Completeness | 11 | ListParts, GetObjectAttributes, checksum SHA256/CRC32 roundtrip, storage class, pagination |
| Integration — Hardening | 7 | body size limits, health endpoint, metadata headers, cache transparency and invalidation |
| Integration — Presigned URLs | 17 | presigned GET/PUT/HEAD/DELETE, security, admin presign, special chars, endpoint override |
| Integration — SSE-C | 17 | put/get roundtrip, error handling, head, copy, range, delete, validation, multipart rejection |
| Integration — Encryption | 16 | encrypted put/get, ETag, range reads, multipart, copy, bucket config |
| Integration — Per-bucket Encryption | 8 | per-bucket enable/disable, plain vs encrypted, ETag, head, revert |
| Integration — KMS | 10 | Vault/OpenBAO key fetch, encrypted put/get, headers, ETag, multipart, copy, range, admin info |
| Integration — TLS | 7 | HTTPS health/info/put/get/multipart, minio client, wrong CA rejection |
| Integration — PostgreSQL | 20 | buckets, objects, multipart, versioning, tags, lifecycle, copy, range, admin health |
| Integration — Notifications | 21 | Put/Get config (Topic/Queue/Lambda), filters, webhook delivery, event format, batch delete, admin API, auth token, connector type roundtrip |
| Integration — MinIO | 99 | mirrors boto3 suite + streaming, file-based, data integrity APIs |
| **Arca tests** | **1,005** | **All tests written for this project** |
| [Ceph s3-tests](https://dxc-technology.github.io/arca/s3-compatibility/) | 830 | 352 pass, 387 fail, 91 skip — 0 unexpected failures |
| **Total** | **1,835** | |

## License

Arca is licensed under the [GNU Affero General Public License](LICENSE-AGPL-3.0), either version 3 of the License or (at your option) any later version (SPDX: `AGPL-3.0-or-later`), a strong copyleft license covering network use: derivative works must remain under the same terms.
