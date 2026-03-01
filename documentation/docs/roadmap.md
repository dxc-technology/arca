# Roadmap

Implementation plan for Arca's MVP. Each phase builds on the previous one and ends with verification: unit tests, boto3 integration tests, and manual `aws s3` CLI checks — all inside Docker containers.

## Progress Overview

| Phase | Name | Status |
|:-----:|------|:------:|
| 0 | [Project Skeleton](#phase-0-project-skeleton) | :material-check-circle: |
| 1 | [Bucket Operations](#phase-1-bucket-operations) | :material-circle-outline: |
| 2 | [Core Object Operations](#phase-2-core-object-operations) | :material-circle-outline: |
| 3 | [CopyObject + ListObjectsV2](#phase-3-copyobject-listobjectsv2) | :material-circle-outline: |
| 4 | [Multipart Upload](#phase-4-multipart-upload) | :material-circle-outline: |
| 5 | [AWS SigV4 Authentication](#phase-5-aws-sigv4-authentication) | :material-circle-outline: |
| 6 | [Disaster Recovery + Polish](#phase-6-disaster-recovery-polish) | :material-circle-outline: |
| 7 | [S3 Compatibility Hardening](#phase-7-s3-compatibility-hardening) | :material-circle-outline: |

<!-- Status icons: :material-circle-outline: = not started, :material-progress-clock: = in progress, :material-check-circle: = done -->

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

## Phase 1 — Bucket Operations

Implement the 4 bucket operations with SQLite metadata storage.

- [ ] SQLite schema (`buckets` table), `SqliteMetadataStore` bucket methods
- [ ] `BucketUsecase`: validate bucket name (S3 naming rules), create, delete (must be empty), head, list
- [ ] Axum handlers in `arca-proto/handlers/bucket.rs`
- [ ] XML response types: `ListAllMyBucketsResult`
- [ ] Unit tests for `SqliteMetadataStore` + `BucketUsecase`
- [ ] Integration test: `test_buckets.py` with boto3

**Verify**: `aws s3 mb`, `aws s3 ls`, `aws s3 rb` all work.

---

## Phase 2 — Core Object Operations

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

## Phase 3 — CopyObject + ListObjectsV2

Add object copying and listing with prefix/delimiter/pagination support.

- [ ] `ObjectUsecase::copy`: real filesystem copy (no reference counting for MVP)
- [ ] `SqliteMetadataStore::list_objects`: SQL with `key > ?` for pagination, `key LIKE ?` for prefix, delimiter handling for common prefixes
- [ ] Continuation token: base64-encoded last key (opaque to client)
- [ ] XML response: `ListBucketResult` with Contents, CommonPrefixes, IsTruncated, NextContinuationToken
- [ ] Integration tests: `test_list.py` (prefix, delimiter, pagination, max-keys, common prefixes)

**Verify**: `aws s3 ls s3://bucket/prefix/`, `aws s3 cp s3://bucket/a s3://bucket/b`.

---

## Phase 4 — Multipart Upload

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

## Phase 5 — AWS SigV4 Authentication

Full AWS Signature V4 implementation with auth middleware.

- [ ] `arca-auth`: Full SigV4 implementation (canonical request, string-to-sign, signing key derivation, signature verification)
- [ ] Test against AWS SigV4 test vectors (downloadable test suite)
- [ ] Constant-time signature comparison via `subtle`
- [ ] Auth middleware in `arca-proto`: parse Authorization header, verify, inject identity into request extensions
- [ ] Virtual-hosted-style middleware: rewrite `bucket.s3.domain/key` -> `/bucket/key`
- [ ] Credentials loaded from config/env vars (MVP)
- [ ] Integration tests: valid credentials succeed, bad credentials get `SignatureDoesNotMatch`

**Verify**: Set up credentials, all operations require auth, unauthenticated requests rejected.

---

## Phase 6 — Disaster Recovery + Polish

Recovery tools, operational logging, and graceful shutdown.

- [ ] `arca recover` CLI: walk `data/` tree, read all `.meta` sidecar files, rebuild SQLite DB from scratch
- [ ] `arca fsck` CLI: compare DB records against filesystem, report orphaned blobs / missing blobs / metadata mismatches
- [ ] Structured logging with tracing (JSON output for production)
- [ ] Graceful shutdown (finish in-flight requests on SIGTERM)

**Verify**: Delete SQLite DB, run `arca recover`, verify all data accessible again.

---

## Phase 7 — S3 Compatibility Hardening

Run industry-standard compatibility tests and harden edge cases.

- [ ] Set up Ceph s3-tests in Docker (`s3-tests/Dockerfile` + `s3tests.conf`)
- [ ] Run test suite, triage failures (many expected: ACLs, versioning, etc.)
- [ ] Fix XML namespace/formatting issues, header edge cases, error code mismatches
- [ ] Track pass/fail list, prevent regressions in CI
- [ ] Performance testing with concurrent requests + large files
- [ ] Documentation
