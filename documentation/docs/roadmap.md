# Roadmap

Implementation plan for Arca's MVP. Each phase builds on the previous one and ends with verification: unit tests, boto3 integration tests, and manual `aws s3` CLI checks — all inside Docker containers.

## Progress Overview

| Phase | Name | Status |
|:-----:|------|:------:|
| 0 | [Project Skeleton](#phase-0-project-skeleton) | :white_check_mark: |
| 1 | [Configuration & Storage Foundation](#phase-1-configuration-storage-foundation) | |
| 2 | [Bucket Operations](#phase-2-bucket-operations) | |
| 3 | [Core Object Operations](#phase-3-core-object-operations) | |
| 4 | [CopyObject + ListObjectsV2](#phase-4-copyobject-listobjectsv2) | |
| 5 | [Multipart Upload](#phase-5-multipart-upload) | |
| 6 | [AWS SigV4 Authentication](#phase-6-aws-sigv4-authentication) | |
| 7 | [Disaster Recovery + Polish](#phase-7-disaster-recovery-polish) | |
| 8 | [S3 Compatibility Hardening](#phase-8-s3-compatibility-hardening) | |

<!-- Status: :white_check_mark: = done, :construction: = in progress, empty = not started -->

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

- [ ] Config file at `/etc/arca/config.toml` (default), `config/default.toml` as repo template, `--config-path` override
- [ ] SQLite database initialization in `arca-storage` (tokio-rusqlite, WAL mode)
- [ ] Schema: `credentials` table (access_key_id, secret_access_key, created_at, active)
- [ ] `arca credential add/list/remove` CLI subcommands (direct SQLite access)
- [ ] Auto-generate root credential on first startup if none exist, print to stdout
- [ ] Unit tests for config loading, credential CRUD, auto-generation
- [ ] Integration test: server starts, prints generated credentials

**Verify**: `bin/run --build -d` starts server and prints auto-generated credentials; `arca credential list` shows the generated credential.

---

## Phase 2 — Bucket Operations

Implement the 4 bucket operations with SQLite metadata storage.

- [ ] SQLite schema (`buckets` table), `SqliteMetadataStore` bucket methods
- [ ] `BucketUsecase`: validate bucket name (S3 naming rules), create, delete (must be empty), head, list
- [ ] Axum handlers in `arca-proto/handlers/bucket.rs`
- [ ] XML response types: `ListAllMyBucketsResult`
- [ ] Unit tests for `SqliteMetadataStore` + `BucketUsecase`
- [ ] Integration test: `test_buckets.py` with boto3

**Verify**: `aws s3 mb`, `aws s3 ls`, `aws s3 rb` all work.

---

## Phase 3 — Core Object Operations

Implement PutObject, GetObject, HeadObject, and DeleteObject with streaming I/O.

- [ ] `FsBlobStore`: UUID path generation, streaming write with concurrent MD5 computation, atomic rename (temp file + rename), sidecar write
- [ ] `FsBlobStore.get()`: streaming read via `tokio::fs::File`, byte range support (seek + take)
- [ ] SQLite `objects` table, `SqliteMetadataStore` object methods (put returns old record for cleanup)
- [ ] `ObjectUsecase`: put (stream + MD5 -> ETag), get, head, delete
- [ ] Axum handlers with streaming request/response bodies
- [ ] Unit tests for `FsBlobStore`, `SqliteMetadataStore`, `ObjectUsecase`
- [ ] Integration test: `test_objects.py`

**Verify**: `aws s3 cp localfile s3://bucket/key` and back, range requests, ETags, Content-Type.

---

## Phase 4 — CopyObject + ListObjectsV2

Add object copying and listing with prefix/delimiter/pagination support.

- [ ] `ObjectUsecase::copy`: real filesystem copy (no reference counting for MVP)
- [ ] `SqliteMetadataStore::list_objects`: SQL with `key > ?` for pagination, `key LIKE ?` for prefix, delimiter handling for common prefixes
- [ ] Continuation token: base64-encoded last key (opaque to client)
- [ ] XML response: `ListBucketResult` with Contents, CommonPrefixes, IsTruncated, NextContinuationToken
- [ ] Integration tests: `test_list.py` (prefix, delimiter, pagination, max-keys, common prefixes)

**Verify**: `aws s3 ls s3://bucket/prefix/`, `aws s3 cp s3://bucket/a s3://bucket/b`.

---

## Phase 5 — Multipart Upload

Full multipart upload lifecycle with composite ETag computation.

- [ ] SQLite tables: `multipart_uploads`, `parts`
- [ ] `FsBlobStore`: put_part (stream to temp part file), assemble_parts (concatenate parts into final blob), delete_parts
- [ ] `MultipartUsecase`: create (generate upload_id UUID), upload_part, complete (validate parts, assemble, compute composite ETag), abort (delete parts + DB records)
- [ ] Composite ETag: `hex(MD5(binary_MD5(part1) || binary_MD5(part2) || ...))-{count}`
- [ ] XML: `InitiateMultipartUploadResult`, `CompleteMultipartUploadResult`, parse `CompleteMultipartUpload` request body
- [ ] Query-param dispatch in object/multipart handlers
- [ ] Integration test: `test_multipart.py`

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

**Verify**: Delete SQLite DB, run `arca recover`, verify all data accessible again.

---

## Phase 8 — S3 Compatibility Hardening

Run industry-standard compatibility tests and harden edge cases.

- [ ] Set up Ceph s3-tests in Docker (`s3-tests/Dockerfile` + `s3tests.conf`)
- [ ] Run test suite, triage failures (many expected: ACLs, versioning, etc.)
- [ ] Fix XML namespace/formatting issues, header edge cases, error code mismatches
- [ ] Track pass/fail list, prevent regressions in CI
- [ ] Performance testing with concurrent requests + large files
- [ ] Documentation
