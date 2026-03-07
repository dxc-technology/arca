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
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">6</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">7</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">8</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">9</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">10</div>
    <div style="background:#4caf50;color:#fff;padding:4px 10px;font-weight:700;font-size:.75em;border-left:1px solid rgba(255,255,255,.3)">11</div>
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
| 6 | [AWS SigV4 Authentication](#phase-6-aws-sigv4-authentication) | <span style="color:#4caf50">&#x2714;</span> |
| 7 | [Disaster Recovery + Polish](#phase-7-disaster-recovery-polish) | <span style="color:#4caf50">&#x2714;</span> |
| 8 | [Admin API](#phase-8-admin-api) | <span style="color:#4caf50">&#x2714;</span> |
| 9 | [Web Console](#phase-9-web-console) | <span style="color:#4caf50">&#x2714;</span> |
| 10 | [S3 Compatibility Hardening](#phase-10-s3-compatibility-hardening) | <span style="color:#4caf50">&#x2714;</span> |
| 11 | [Documentation](#phase-11-documentation) | <span style="color:#4caf50">&#x2714;</span> |

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

**Verify**: `bin/arca start -d --build` starts server and prints auto-generated credentials; `arca credential list` shows the generated credential.

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

- [x] `arca-auth`: Full SigV4 implementation (canonical request, string-to-sign, signing key derivation, signature verification)
- [x] Test against AWS SigV4 test vectors (downloadable test suite)
- [x] Constant-time signature comparison via `subtle`
- [x] Auth middleware in `arca-proto`: parse Authorization header, look up credential in DB, verify signature
- [x] Virtual-hosted-style middleware: rewrite `bucket.s3.domain/key` -> `/bucket/key`
- [x] Trailing-slash normalization with original URI preservation (mc compatibility)
- [x] Environment variable credential override (`ARCA_ROOT_ACCESS_KEY`/`ARCA_ROOT_SECRET_KEY`) for testing
- [x] Integration tests: valid credentials succeed, bad credentials get `SignatureDoesNotMatch` / `InvalidAccessKeyId`

**Verify**: Set up credentials via `arca credential add`, all operations require auth, unauthenticated requests rejected.

---

## Phase 7 — Disaster Recovery + Polish

Recovery tools, operational logging, and graceful shutdown.

- [x] `arca recover` CLI: walk `data/` tree, read all `.meta` sidecar files, rebuild SQLite DB from scratch. Supports `--dry-run` for preview and `--skip-verify` to skip MD5 checksum verification. Preserves credentials across DB rebuild. Skips orphaned/malformed/corrupt sidecars with warnings.
- [x] `arca fsck` CLI: compare DB records against filesystem, report orphaned blobs / missing blobs / sidecar mismatches / orphaned sidecars / stale temp files. Optional `--verify-checksums` for MD5 verification of every blob. Exit code 0 = clean, 1 = issues found.
- [x] Structured logging with tracing (`--log-format text|json` on `serve` command)
- [x] Graceful shutdown (finish in-flight requests on SIGTERM/SIGINT)
**Verify**: Delete SQLite DB, run `arca recover`, verify all data accessible again.

---

## Phase 8 — Admin API

JSON-based administration API for the web console and other management tools.
Endpoints live under `/admin/*` on the same port (9000), using SigV4 auth.

- [x] Health, info, stats endpoints (`GET /admin/health`, `/admin/info`, `/admin/stats`)
- [x] Credential CRUD (`GET/POST /admin/credentials`, `DELETE /admin/credentials/{access_key_id}`)
- [x] Unit + integration tests

**Verify**: Admin API responds to health/info/stats requests; credential CRUD works via API.

---

## Phase 9 — Web Console

Web-based administration console and bucket browser, deployed as a **separate application**
(`console/` directory) that communicates with Arca exclusively via S3 API and Admin API.

- [x] Web console app: single-file Alpine.js + Tailwind CSS, "The Vault" dark theme, nginx:alpine Docker image
- [x] Admin dashboard: bento-grid layout with server info, storage stats, SVG donut chart, health indicator, auto-refresh
- [x] Bucket browser: list/create/delete buckets, prefix navigation with breadcrumbs, upload/download/delete objects, detail panel, treemap visualization
- [x] Credential management: card grid, create with reveal-once secret, delete with confirmation, lockout warning
- [x] Browser SigV4 signing via Web Crypto API, CORS middleware in Arca
- [x] Admin flag on credentials: `admin` boolean field, migration v5, CLI `--admin` flag
- [x] Admin-only access to Admin API endpoints (non-admin gets 403)
- [x] Role-aware console: admin sees dashboard + credentials + buckets; non-admin sees buckets only
- [x] Lockout prevention: cannot delete last admin credential

**Verify**: Web console connects to Arca and provides dashboard and bucket browsing.

---

## Phase 10 — S3 Compatibility Hardening

Run industry-standard compatibility tests and harden edge cases.

- [x] Set up Ceph s3-tests in Docker (`docker/s3-tests/Dockerfile` + `s3tests.conf`, `bin/s3-tests` runner)
- [x] Run test suite, triage failures — 198 pass / 540 fail / 91 skip (see `s3-tests/TRIAGE.md`)
- [x] Fix compatibility issues: `x-amz-request-id`/`x-amz-id-2`/`Server` headers, `GetBucketLocation`, `CreateBucket` idempotency, `ListObjectsV1`, empty delimiter handling, whitespace-preserving `DeleteObjects` XML parser, unimplemented PUT bucket ops return 501
- [x] Track pass/fail list (`s3-tests/passlist.txt`), HTML compatibility dashboard (`s3-tests/report.html`)
- [x] Performance testing with concurrent requests + large files (`tests/perf/perf_test.py`, `bin/perf-test`)

**Verify**: Ceph s3-tests running in CI, pass/fail list tracked, no regressions.

---

## Phase 11 — Documentation

Comprehensive manuals and guides for users, administrators, and operators.

- [x] Restructured documentation into grouped sections (Getting Started, User Guide, Reference, Operations)
- [x] Installation guide and Quick Start
- [x] CLI reference (extracted from configuration page)
- [x] Web Console guide with automated screenshots
- [x] Disaster Recovery guide (extracted from configuration + architecture)
- [x] Monitoring & Logging guide
- [x] Production Deployment guide
- [x] Automated screenshot tool (`bin/screenshots`, Playwright + Docker)

**Verify**: All manuals published on GitHub Pages, covering installation through production operations.
