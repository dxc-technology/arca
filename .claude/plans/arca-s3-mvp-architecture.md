# Arca - S3-Compatible Object Storage Server

## Context

MinIO open source has been frozen. We're building **Arca**, a new open source S3-compatible object storage server from scratch in Rust. The goal is 100% S3 API compatibility for a focused MVP subset, designed for eventual production scale but starting with a single-node implementation.

We're building the entire S3 HTTP protocol adapter ourselves (no `s3s` dependency) to maintain full control and avoid pre-1.0 external dependency risk. Reference: **RustFS** (22K+ stars, Apache 2.0) validates the general architectural patterns (layered design, Tower middleware, use-case pattern).

## Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Language | Rust | Performance, memory safety, ideal for I/O-heavy storage |
| S3 Protocol | Custom adapter with Axum | Full control, no pre-1.0 dependency risk |
| HTTP Framework | Axum 0.8 + Tower | Modern, tokio-native, excellent middleware ecosystem |
| XML | quick-xml + serde | 10-50x faster than xml-rs, serde integration |
| Auth | Custom SigV4 using hmac/sha2 | Well-documented algorithm, testable against AWS test vectors |
| Storage | UUID-based blobs + `.meta` sidecar files | No path-traversal risks. Sidecar enables disaster recovery |
| Metadata DB | SQLite via tokio-rusqlite (behind trait) | Simple for v1, swappable to Postgres later |
| License | AGPL-3.0-or-later | Strong network copyleft |
| Testing | Ceph s3-tests + custom boto3 integration tests | Industry standard for S3 compatibility |
| Port | 9000 | Industry convention (MinIO compatible) |

## MVP API Surface (15 operations)

- **Bucket**: CreateBucket, DeleteBucket, HeadBucket, ListBuckets
- **Object**: PutObject, GetObject, DeleteObject, HeadObject, CopyObject
- **Listing**: ListObjectsV2
- **Multipart**: CreateMultipartUpload, UploadPart, CompleteMultipartUpload, AbortMultipartUpload
- **Auth**: AWS Signature V4

## Architecture

```
                     HTTP Request
                          |
                 +--------v--------+
                 |  Tower Middleware | (tracing, timeouts)
                 +--------+--------+
                          |
                 +--------v--------+
                 | Virtual Host    | Rewrites bucket.s3.domain -> /bucket/path
                 | Middleware      |
                 +--------+--------+
                          |
                 +--------v--------+
                 | SigV4 Auth     | Verifies AWS Signature V4
                 | Middleware      | Injects identity into extensions
                 +--------+--------+
                          |
                 +--------v--------+
                 | Axum Router    | Routes by method + path
                 | (arca-proto)   | Dispatches by query params
                 +--------+--------+
                          |
                 +--------v--------+
                 | Use Cases      | Business logic
                 | (arca-server)  | Bucket/Object/Multipart
                 +--------+--------+
                          |
           +--------------+--------------+
           |                             |
  +--------v--------+          +--------v--------+
  | MetadataStore   |          |   BlobStore     |
  | (trait)         |          |   (trait)       |
  +--------+--------+          +--------+--------+
           |                             |
  +--------v--------+          +--------v--------+
  | SqliteMetadata  |          | FsBlobStore     |
  |                 |          | UUID + sidecar  |
  +-----------------+          +-----------------+
           |                             |
      SQLite DB                  data/ab/cd/{uuid}
      (WAL mode)                 data/ab/cd/{uuid}.meta
```

## Project Structure

```
arca/
  Cargo.toml                        # Workspace root
  Cargo.lock
  LICENSE-AGPL-3.0
  README.md
  CLAUDE.md
  config/
    default.toml                     # Server config (bind, storage, auth)
  docker/
    Dockerfile                       # Multi-stage: rust builder + debian-slim runtime
    Dockerfile.test                  # Python + boto3 test runner
    docker-compose.yml               # Dev: arca + test services
    s3-tests/
      Dockerfile                     # Ceph s3-tests runner
      s3tests.conf                   # Config pointing at Arca
  crates/
    arca-core/                       # Shared types, traits, errors (NO I/O)
      src/
        lib.rs
        types.rs                     # BlobId, BucketInfo, ObjectRecord, PartRecord, etc.
        error.rs                     # ArcaError (internal) + S3Error (user-facing, with HTTP status + XML code)
        store/
          mod.rs
          blob.rs                    # BlobStore trait
          metadata.rs                # MetadataStore trait
        s3/
          mod.rs
          xml_types.rs               # XML request/response structs (serde + quick-xml)
    arca-auth/                       # AWS SigV4 verification (zero I/O deps, independently testable)
      src/
        lib.rs
        sigv4.rs                     # verify_sigv4, canonical_request, string_to_sign, derive_signing_key
        canonical.rs                 # URI/query/header canonicalization, parse Authorization header
      tests/
        sigv4_test.rs                # Test against AWS SigV4 test vectors
    arca-proto/                      # S3 HTTP protocol adapter (Axum)
      src/
        lib.rs                       # build_router(), AppState
        router.rs                    # All Axum routes + middleware wiring
        middleware/
          mod.rs
          virtual_host.rs            # Host header -> path rewrite for virtual-hosted-style
          auth.rs                    # SigV4 verification middleware
        handlers/
          mod.rs
          bucket.rs                  # list_buckets, create/delete/head_bucket
          object.rs                  # put/get/head/delete_object (streaming), query-param dispatch
          multipart.rs               # create/complete/abort multipart, upload_part
        xml/
          mod.rs
          response.rs                # XmlResponse<T> impl IntoResponse, to_xml()
          error.rs                   # XmlErrorResponse(S3Error) -> S3 XML error body
        extract/
          mod.rs
          s3_params.rs               # S3Headers extractor (x-amz-* headers, user metadata)
    arca-storage/                    # Storage implementations
      src/
        lib.rs
        fs/
          mod.rs
          blob.rs                    # FsBlobStore: UUID paths, atomic writes, MD5 during stream
          sidecar.rs                 # .meta JSON file read/write
        sqlite/
          mod.rs
          metadata.rs                # SqliteMetadataStore: all MetadataStore methods
          migrations/
            001_initial.sql          # buckets, objects, multipart_uploads, parts tables
        recover.rs                   # Walk data dir, read .meta files, rebuild SQLite DB
    arca-server/                     # Binary: wires everything, use-case layer
      src/
        main.rs                      # tokio::main, config, migrations, dependency wiring, serve
        config.rs                    # Config struct (server, storage, auth) from TOML + env
        cli.rs                       # clap: serve, recover, fsck subcommands
        usecases/
          mod.rs
          bucket.rs                  # BucketUsecase: validate name, CRUD
          object.rs                  # ObjectUsecase: put (stream+MD5), get, delete, head, copy
          multipart.rs               # MultipartUsecase: lifecycle, composite ETag
  tests/
    integration/
      conftest.py                    # boto3 client fixture
      test_buckets.py
      test_objects.py
      test_multipart.py
      test_list.py
      requirements.txt               # boto3, pytest
```

## Crate Dependency Graph (strict, no cycles)

```
arca-server (binary)
  ├── arca-proto     (HTTP layer)
  ├── arca-storage   (storage impls)
  ├── arca-auth      (auth verification)
  └── arca-core      (shared types/traits)

arca-proto
  ├── arca-core
  └── arca-auth

arca-auth
  └── arca-core

arca-storage
  └── arca-core
```

## Key Design Details

### S3 Routing via Query Parameters

Many S3 operations share the same HTTP method + path but differ by query params. Axum doesn't support query-based routing, so handlers dispatch internally:

- `PUT /:bucket/*key` -> PutObject OR UploadPart (if `?partNumber=&uploadId=`)
- `GET /:bucket` -> ListObjectsV2 (if `?list-type=2`) OR HeadBucket
- `POST /:bucket/*key` -> CreateMultipartUpload (if `?uploads`) OR CompleteMultipartUpload (if `?uploadId=`)
- `DELETE /:bucket/*key` -> DeleteObject OR AbortMultipartUpload (if `?uploadId=`)

This is the same approach used by MinIO, Ceph RGW, and RustFS.

### Streaming (never buffer full objects)

- **PutObject**: Body stream -> tee to MD5 hasher + file writer -> atomic rename. Uses `x-amz-content-sha256: UNSIGNED-PAYLOAD` for auth (body hash skipped; integrity via Content-MD5).
- **GetObject**: `tokio::fs::File` -> `ReaderStream` -> Axum body. Range requests via seek + take.
- **UploadPart**: Same streaming as PutObject, into temp part file.

### Auth: Body Hash Strategy

For streaming uploads, buffering 5GB for SHA-256 is impossible. S3 clients send `x-amz-content-sha256: UNSIGNED-PAYLOAD` or `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`. Auth middleware verifies request signature (headers + URI) only, not body hash. Body integrity is separately guaranteed by `Content-MD5` or checksums.

### Sidecar `.meta` Format (JSON)

```json
{
  "bucket": "my-bucket",
  "key": "photos/2024/vacation.jpg",
  "content_type": "image/jpeg",
  "size": 1234567,
  "etag": "d41d8cd98f00b204e9800998ecf8427e",
  "user_metadata": {"x-amz-meta-author": "pietro"},
  "created_at": "2026-02-27T14:30:00Z"
}
```

Write order: blob file -> sidecar .meta -> SQLite insert. On `arca recover`, walk data dir, read all `.meta` files, rebuild DB.

### MetadataStore: put_object returns old record

`put_object` returns `Option<ObjectRecord>` of the overwritten object (if any), so the caller can delete the orphaned blob. Without this, blobs accumulate unboundedly on repeated writes to the same key.

## Key Dependencies

```toml
[workspace.dependencies]
# HTTP + Async
axum = { version = "0.8", features = ["macros"] }
tokio = { version = "1", features = ["full"] }
tower = { version = "0.5", features = ["full"] }
tower-http = { version = "0.6", features = ["trace", "timeout"] }
hyper = { version = "1", features = ["full"] }
hyper-util = "0.1"
http = "1"
bytes = "1"
futures = "0.3"

# XML (our own protocol layer)
quick-xml = { version = "0.36", features = ["serialize"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Auth / Crypto
hmac = "0.12"
sha2 = "0.10"
md-5 = "0.10"
hex = "0.4"
subtle = "2"                         # Constant-time comparison (timing attack prevention)
base64 = "0.22"

# Storage
tokio-rusqlite = "0.6"
rusqlite = { version = "0.32", features = ["bundled"] }
uuid = { version = "1", features = ["v4"] }
tempfile = "3"

# Config + CLI + Logging
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
chrono = { version = "0.4", features = ["serde"] }
async-trait = "0.1"
thiserror = "1"
anyhow = "1"
```

## Implementation Phases

### Phase 0: Project Skeleton
- Initialize Cargo workspace with all 5 crates (arca-core, arca-auth, arca-proto, arca-storage, arca-server)
- Scaffold Axum router with stub handlers returning `S3Error::NotImplemented`
- Wire `XmlErrorResponse` so all requests return valid S3 XML errors
- Dockerfile (multi-stage: `rust:1.83-bookworm` builder + `debian:bookworm-slim` runtime), docker-compose.yml
- LICENSE, README.md, CLAUDE.md, config/default.toml
- **Verify**: `docker compose up` starts server; `aws s3 ls --endpoint-url http://localhost:9000` returns valid S3 XML error (NotImplemented)

### Phase 1: Bucket Operations
- SQLite schema (`buckets` table), `SqliteMetadataStore` bucket methods
- `BucketUsecase`: validate bucket name (S3 naming rules), create, delete (must be empty), head, list
- Axum handlers in `arca-proto/handlers/bucket.rs`
- XML response types: `ListAllMyBucketsResult`
- Unit tests for SqliteMetadataStore + BucketUsecase
- Integration test: `test_buckets.py` with boto3
- **Verify**: `aws s3 mb`, `aws s3 ls`, `aws s3 rb` all work

### Phase 2: PutObject + GetObject + HeadObject + DeleteObject
- `FsBlobStore`: UUID path generation, streaming write with concurrent MD5 computation, atomic rename (temp file + rename), sidecar write
- `FsBlobStore.get()`: streaming read via tokio::fs::File, byte range support (seek + take)
- SQLite `objects` table, `SqliteMetadataStore` object methods (put returns old record for cleanup)
- `ObjectUsecase`: put (stream + MD5 -> ETag), get, head, delete
- Axum handlers with streaming request/response bodies
- Unit tests for FsBlobStore, SqliteMetadataStore, ObjectUsecase
- Integration test: `test_objects.py`
- **Verify**: `aws s3 cp localfile s3://bucket/key` and back, range requests, ETags, Content-Type

### Phase 3: CopyObject + ListObjectsV2
- `ObjectUsecase::copy`: real filesystem copy (no reference counting for MVP)
- `SqliteMetadataStore::list_objects`: SQL with `key > ?` for pagination, `key LIKE ?` for prefix, delimiter handling for common prefixes
- Continuation token: base64-encoded last key (opaque to client)
- XML response: `ListBucketResult` with Contents, CommonPrefixes, IsTruncated, NextContinuationToken
- Integration tests: `test_list.py` (prefix, delimiter, pagination, max-keys, common prefixes)
- **Verify**: `aws s3 ls s3://bucket/prefix/`, `aws s3 cp s3://bucket/a s3://bucket/b`

### Phase 4: Multipart Upload
- SQLite tables: `multipart_uploads`, `parts`
- `FsBlobStore`: put_part (stream to temp part file), assemble_parts (concatenate parts into final blob), delete_parts
- `MultipartUsecase`: create (generate upload_id UUID), upload_part, complete (validate parts, assemble, compute composite ETag), abort (delete parts + DB records)
- Composite ETag: `hex(MD5(binary_MD5(part1) || binary_MD5(part2) || ...))-{count}`
- XML: `InitiateMultipartUploadResult`, `CompleteMultipartUploadResult`, parse `CompleteMultipartUpload` request body
- Query-param dispatch in object/multipart handlers
- Integration test: `test_multipart.py`
- **Verify**: `aws s3 cp largefile s3://bucket/key` (triggers multipart in aws-cli), ETag format is `"<hex>-<count>"`

### Phase 5: AWS SigV4 Authentication
- `arca-auth`: Full SigV4 implementation (canonical request, string-to-sign, signing key derivation, signature verification)
- Test against AWS SigV4 test vectors (downloadable test suite)
- Constant-time signature comparison via `subtle`
- Auth middleware in `arca-proto`: parse Authorization header, verify, inject identity into request extensions
- Virtual-hosted-style middleware: rewrite `bucket.s3.domain/key` -> `/bucket/key`
- Credentials loaded from config/env vars (MVP)
- Integration tests: valid credentials succeed, bad credentials get `SignatureDoesNotMatch`
- **Verify**: Set up credentials, all operations require auth, unauthenticated requests rejected

### Phase 6: Disaster Recovery + Polish
- `arca recover` CLI: walk `data/` tree, read all `.meta` sidecar files, rebuild SQLite DB from scratch
- `arca fsck` CLI: compare DB records against filesystem, report orphaned blobs / missing blobs / metadata mismatches
- Structured logging with tracing (JSON output for production)
- Graceful shutdown (finish in-flight requests on SIGTERM)
- **Verify**: Delete SQLite DB, run `arca recover`, verify all data accessible again

### Phase 7: S3 Compatibility Hardening
- Set up Ceph s3-tests in Docker (s3-tests/Dockerfile + s3tests.conf)
- Run test suite, triage failures (many will be expected: ACLs, versioning, etc.)
- Fix XML namespace/formatting issues, header edge cases, error code mismatches
- Track pass/fail list, prevent regressions in CI
- Performance testing with concurrent requests + large files
- Documentation

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| SigV4 implementation bugs | High (auth bypass) | Test against AWS official test vectors; constant-time comparison |
| XML response format mismatches | High (client breakage) | Ceph s3-tests catches these; test with multiple S3 clients (boto3, aws-cli, rclone) |
| SQLite write concurrency | Low (MVP) | WAL mode; trait abstraction allows Postgres swap later |
| Crash between blob write and DB | Data inconsistency | Write order: blob -> sidecar -> DB. `recover` rebuilds from sidecars |
| ListObjectsV2 delimiter edge cases | High (compatibility) | Extensive integration tests with edge cases |
| Multipart composite ETag | Medium | Binary MD5 concat (not hex), thorough unit tests |
| Axum streaming body handling | Medium | Test with large files (>1GB), verify memory stays constant |

## Explicitly NOT in MVP

Object versioning, ACLs/bucket policies, server-side encryption, object tagging, lifecycle rules, CORS, object lock, presigned URLs, admin API, metrics endpoint, replication, multi-node/distributed mode.

## Verification

After each phase, verify with (all inside Docker containers):
1. `cargo test --workspace` — unit tests
2. `pytest tests/integration/` — boto3 integration tests via docker-compose
3. `aws s3` CLI commands against running Arca container
4. After Phase 7: Ceph s3-tests subset via Docker
