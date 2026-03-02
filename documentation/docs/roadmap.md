# Roadmap

Implementation plan for Arca's MVP. Each phase builds on the previous one and ends with verification: unit tests, boto3 integration tests, and manual `aws s3` CLI checks — all inside Docker containers.

## Progress Overview

<!-- progress-bar -->
<div style="padding:12px 0">
  <div style="display:inline-flex;border-radius:6px;overflow:hidden;border:1px solid rgba(128,128,128,.3)">
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em">0</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">1</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">2</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">3</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">4</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">5</div>
    <div style="background:transparent;color:inherit;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(128,128,128,.3);opacity:.5">6</div>
    <div style="background:transparent;color:inherit;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(128,128,128,.3);opacity:.5">7</div>
    <div style="background:transparent;color:inherit;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(128,128,128,.3);opacity:.5">8</div>
    <div style="background:transparent;color:inherit;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(128,128,128,.3);opacity:.5">9</div>
  </div>
</div>
<!-- /progress-bar -->

| Phase | Name | Status |
|:-----:|------|:------:|
| 0 | [Project Skeleton](#phase-0-project-skeleton) | <span style="color:#4caf50">&#x2714;</span> |
| 1 | [Configuration & Storage Foundation](#phase-1-configuration-storage-foundation) | <span style="color:#4caf50">&#x2714;</span> |
| 2 | [Bucket Operations](#phase-2-bucket-operations) | <span style="color:#4caf50">&#x2714;</span> |
| 3 | [Core Object Operations](#phase-3-core-object-operations) | <span style="color:#4caf50">&#x2714;</span> |
| 4 | [CopyObject + ListObjectsV2](#phase-4-copyobject-listobjectsv2) | <span style="color:#4caf50">&#x2714;</span> |
| 5 | [Multipart Upload](#phase-5-multipart-upload) | <span style="color:#4caf50">&#x2714;</span> |
| 6 | [AWS SigV4 Authentication](#phase-6-aws-sigv4-authentication) | |
| 7 | [Disaster Recovery + Polish](#phase-7-disaster-recovery-polish) | |
| 8 | [Web Console](#phase-8-web-console) | |
| 9 | [S3 Compatibility Hardening](#phase-9-s3-compatibility-hardening) | |

<!-- Status: green checkmark = done, :construction: = in progress, empty = not started -->

---

## Phase 0 — Project Skeleton

Set up the Cargo workspace with all 5 crates and the basic infrastructure.

- [x] Initialize Cargo workspace: `arca-core`, `arca-auth`, `arca-proto`, `arca-storage`, `arca-server`
- [x] Scaffold Axum router with stub handlers returning `S3Error::NotImplemented`
- [x] Wire `XmlErrorResponse` so all requests return valid S3 XML errors
- [x] Dockerfile (multi-stage: `rust:1.85-bookworm` builder + `debian:bookworm-slim` runtime), docker-compose.yml
- [x] LICENSE, CLAUDE.md, `config/default.toml`

**Verify**: `docker compose up` starts server; `aws s3 ls --endpoint-url http://localhost:9000` returns valid S3 XML error (NotImplemented).

---

## Phase 1 — Configuration & Storage Foundation

Establish the configuration model and SQLite database foundation. MinIO-like approach: static server settings in a TOML config file, dynamic data (credentials, users) in the database.

- [x] Config file at `/etc/arca/config.toml` (default), `config/default.toml` as repo template, `--config-path` override
- [x] SQLite database initialization in `arca-storage` (tokio-rusqlite, WAL mode)
- [x] Schema: `credentials` table (access_key_id, secret_access_key, created_at, active)
- [x] `arca credential add/list/remove` CLI subcommands (direct SQLite access)
- [x] Auto-generate root credential on first startup if none exist, print to stdout
- [x] Unit tests for config loading, credential CRUD, auto-generation
- [x] Integration test: server starts, prints generated credentials

**Verify**: `bin/run --build -d` starts server and prints auto-generated credentials; `arca credential list` shows the generated credential.

---

## Phase 2 — Bucket Operations

Implement the 4 bucket operations with SQLite metadata storage.

- [x] SQLite schema (`buckets` table), `SqliteMetadataStore` bucket methods
- [x] Bucket name validation (S3 naming rules) in `arca-core`, handlers call MetadataStore directly
- [x] Axum handlers in `arca-proto/handlers/bucket.rs` with `AppState` injection
- [x] XML response types: `ListAllMyBucketsResult`
- [x] Unit tests for validation (~22 tests), `SqliteMetadataStore` (~7 tests), XML types (~3 tests)
- [x] Integration test: `test_buckets.py` with boto3 (~10 tests)

**Verify**: `aws s3 mb`, `aws s3 ls`, `aws s3 rb` all work.

---

## Phase 3 — Core Object Operations

Implement PutObject, GetObject, HeadObject, and DeleteObject with streaming I/O.

- [x] `FsBlobStore`: UUID path generation, configurable prefix depth (1–4, default 2), streaming write with concurrent MD5 computation, atomic rename (temp file + rename), sidecar `.meta` JSON write
- [x] `FsBlobStore.get()`: streaming read via `tokio::fs::File`, byte range support (seek + take)
- [x] SQLite `objects` table, `SqliteMetadataStore` object methods (put returns old record for cleanup), `bucket_is_empty` check
- [x] Axum handlers with streaming request/response bodies (handlers call BlobStore + MetadataStore directly)
- [x] DeleteBucket now checks bucket is empty before deleting (returns BucketNotEmpty 409)
- [x] Unit tests for `FsBlobStore` (~12 tests), `SqliteMetadataStore` objects (~7 tests)
- [x] Integration test: `test_objects.py` (~12 tests)

**Verify**: `aws s3 cp localfile s3://bucket/key` and back, range requests, ETags, Content-Type.

---

## Phase 4 — CopyObject + ListObjectsV2

Add object copying and listing with prefix/delimiter/pagination support.

- [x] CopyObject: `PUT /{bucket}/{*key}` with `x-amz-copy-source` header, stream-through copy via BlobStore get → put
- [x] `SqliteMetadataStore::list_objects`: SQL with `key > ?` for pagination, `key LIKE ?` for prefix, delimiter handling for common prefixes
- [x] Continuation token: base64-encoded last key (opaque to client)
- [x] XML response: `ListBucketResult` with Contents, CommonPrefixes, IsTruncated, NextContinuationToken
- [x] Integration tests: `test_list.py` (~12 tests), `TestCopyObject` (~6 tests), unit tests for XML builders and list_objects

**Verify**: `aws s3 ls s3://bucket/prefix/`, `aws s3 cp s3://bucket/a s3://bucket/b`.

---

## Phase 5 — Multipart Upload

Full multipart upload lifecycle with composite ETag computation.

- [x] SQLite tables: `multipart_uploads`, `parts`
- [x] `MetadataStore` multipart methods: create/get/delete upload, put/list parts
- [x] Parts stored as normal blobs via `BlobStore::put()`, stream-concatenated at complete time
- [x] Composite ETag: `hex(MD5(binary_MD5(part1) || binary_MD5(part2) || ...))-{count}`
- [x] XML: `InitiateMultipartUploadResult`, `CompleteMultipartUploadResult`, parse `CompleteMultipartUpload` request body
- [x] Query-param dispatch in object/multipart handlers
- [x] Part size validation at complete time (non-last parts >= 5 MB)
- [x] Integration test: `test_multipart.py` (~12 tests)

**Verify**: `aws s3 cp largefile s3://bucket/key` (triggers multipart in aws-cli), ETag format is `"<hex>-<count>"`.

---

## Phase 6 — AWS SigV4 Authentication

Full AWS Signature V4 implementation with auth middleware. Credentials loaded from the SQLite database (managed via `arca credential` CLI from Phase 1).

- [ ] `arca-auth`: Full SigV4 implementation (canonical request, string-to-sign, signing key derivation, signature verification)
- [ ] Test against AWS SigV4 test vectors (downloadable test suite)
- [ ] Constant-time signature comparison via `subtle`
- [ ] Auth middleware in `arca-proto`: parse Authorization header, look up credential in DB, verify signature, inject identity into request extensions
- [ ] Virtual-hosted-style middleware: rewrite `bucket.s3.domain/key` -> `/bucket/key`
- [ ] Integration tests: valid credentials succeed, bad credentials get `SignatureDoesNotMatch`

**Verify**: Set up credentials via `arca credential add`, all operations require auth, unauthenticated requests rejected.

---

## Phase 7 — Disaster Recovery + Polish

Recovery tools, operational logging, and graceful shutdown.

- [ ] `arca recover` CLI: walk `data/` tree, read all `.meta` sidecar files, rebuild SQLite DB from scratch
- [ ] `arca fsck` CLI: compare DB records against filesystem, report orphaned blobs / missing blobs / metadata mismatches
- [ ] Structured logging with tracing (JSON output for production)
- [ ] Graceful shutdown (finish in-flight requests on SIGTERM)
- [ ] User manual (structure, content, and style TBD before starting this phase)

**Verify**: Delete SQLite DB, run `arca recover`, verify all data accessible again.

---

## Phase 8 — Web Console

Web-based administration console and bucket browser. Serves from the Arca binary itself (embedded static assets). Design TBD.

- [ ] Frontend app (framework and design to be decided)
- [ ] Embedded static asset serving from the Arca binary
- [ ] Admin dashboard: server status, storage usage, credential management
- [ ] Bucket browser: list buckets, browse objects, upload/download, delete
- [ ] Authentication via Arca credentials

**Verify**: Navigate to `http://localhost:9000/console`, log in, browse buckets and objects, upload a file.

---

## Phase 9 — S3 Compatibility Hardening

Run industry-standard compatibility tests and harden edge cases.

- [ ] Set up Ceph s3-tests in Docker (`s3-tests/Dockerfile` + `s3tests.conf`)
- [ ] Run test suite, triage failures (many expected: ACLs, versioning, etc.)
- [ ] Fix XML namespace/formatting issues, header edge cases, error code mismatches
- [ ] Track pass/fail list, prevent regressions in CI
- [ ] Performance testing with concurrent requests + large files
- [ ] Documentation
