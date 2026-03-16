# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Arca is an open source S3-compatible object storage server written in Rust. It is designed as a drop-in replacement for AWS S3, MinIO, and other S3-compatible storage services, targeting 100% S3 API compatibility for a focused MVP subset (15 operations). Starts as single-node with eventual production scale in mind. Licensed under AGPL-3.0-or-later.

The full architecture plan lives in `.claude/plans/arca-s3-mvp-architecture.md`.

## Build & Test

All development happens inside Docker containers — never install libraries on the host.

Convenience scripts live in `bin/`. They fully abstract Docker Compose, so the user never needs to interact with compose directly. All build operations go through `bin/build`.

**Plugin system**: Features (TLS, encryption, KMS) are composable via `--flags`. Each feature has a config fragment in `config/fragments/` and an optional compose overlay in `docker/`. The shared library `bin/lib/compose.sh` handles config generation (TOML concatenation) and compose command building. Adding a new feature = 1 fragment + a few lines in the library.

```bash
# Build
bin/build                        # build Docker image
bin/build --dev                  # build development image (has shell)
bin/build --console              # build console image
bin/build --binary               # extract Linux binary to build/arca-<arch>
bin/build --binary --arch amd64  # cross-compile for x86_64

# Run
bin/arca start -d                # start server in background
bin/arca start -d --build --dev  # rebuild dev image and start
bin/arca start -d --tls          # start with TLS (certs in ./certs/)
bin/arca start -d --encryption   # start with encryption (SSE-S3)
bin/arca start -d --kms          # start with OpenBAO KMS
bin/arca start -d --tls --kms    # combine features freely
bin/arca start -d --config <path>  # custom config (escape hatch)
bin/arca stop                    # stop server + all associated services
bin/arca status                  # show container status
bin/arca logs -f                 # follow server logs

# Console (inherits features from running Arca via .arca-env)
bin/console start -d             # start console
bin/console start -d --build     # rebuild and start
bin/console stop                 # stop console

# Tests
bin/test                 # run unit + integration tests
bin/test unit            # unit tests only
bin/test integration     # integration tests only (server must be running)
bin/test unit -p arca-core   # pass extra args to cargo test
bin/test tls             # TLS integration tests (self-contained)
bin/test encryption      # encryption integration tests
bin/test per-bucket-encryption   # per-bucket encryption tests
bin/test kms             # KMS integration tests (with OpenBAO)

# Manual S3 CLI verification against running Arca
aws s3 ls --endpoint-url http://localhost:9000

# Documentation
bin/docs-build           # build mkdocs site (output to docs/)
bin/docs-serve           # serve locally with live reload (http://localhost:8000)
bin/docs-publish         # build + commit + push docs to update GitHub Pages
```

### Build constraints

- **Rust 1.85** in Docker builder (Alpine). `getrandom 0.4` requires edition 2024 which needs >= 1.85.
- `ring` crate needs `perl` in Alpine (`apk add --no-cache musl-dev perl`).
- `time` crate pinned to 0.3.41 (0.3.47+ requires Rust 1.88).
- `Cargo.lock*` glob in Dockerfile allows building with or without committed lockfile.
- `bin/build` only rebuilds the `arca` service image, NOT `unit-test` or `test`. After code changes, run `docker compose -f docker/docker-compose.yml build unit-test test` or test images will be stale.

## Documentation

The documentation site uses MkDocs with Material theme, built inside a Docker container.

**Dual-directory structure**: source files live in `documentation/docs/`, built output goes to `docs/` (served by GitHub Pages). Always update BOTH directories, then commit changes in both `documentation/` and `docs/`.

After every phase completion or significant feature change, update documentation BEFORE committing code: roadmap checkboxes, configuration page, and installation/Quick Start as needed. Rebuild with `bin/docs-build`.

## Architecture

Five-crate Cargo workspace with strict dependency graph (no cycles):

- **arca-core** — Shared types, traits (`BlobStore`, `MetadataStore`), error types. Zero I/O dependencies.
- **arca-auth** — AWS SigV4 verification. Zero I/O dependencies, independently testable against AWS test vectors.
- **arca-proto** — S3 HTTP protocol adapter (Axum 0.8 + Tower). Handlers, XML ser/de, middleware (virtual-host rewrite, auth).
- **arca-storage** — Storage implementations: `FsBlobStore` (UUID + sidecar), `SqliteMetadataStore` (tokio-rusqlite, WAL mode).
- **arca-server** — Binary crate. Wires dependencies, config (TOML + env), CLI (clap: `serve`, `recover`, `fsck`), use-case layer (`BucketUsecase`, `ObjectUsecase`, `MultipartUsecase`).

Dependency direction: `arca-server` -> `arca-proto`, `arca-storage`, `arca-auth` -> `arca-core`.

## Key Design Constraints

**No `s3s` crate** — we build our own S3 HTTP protocol adapter with Axum. This was an explicit decision to avoid pre-1.0 dependency risk.

**S3 query-parameter routing** — S3 overloads the same HTTP method+path with different query params. Handlers dispatch internally (e.g., `PUT /:bucket/*key` -> PutObject vs UploadPart based on `?partNumber=&uploadId=`).

**Streaming-first** — never buffer full objects in memory. PutObject streams through MD5 hasher + file writer concurrently. GetObject streams from tokio::fs::File via ReaderStream.

**Storage write order** — blob file -> sidecar `.meta` JSON -> SQLite insert. This ordering enables disaster recovery: `arca recover` walks the `blobs/` directory, reads `.meta` files, and rebuilds the DB from scratch. Blob files use `O_CREAT | O_EXCL` to prevent UUID collision; `UNIQUE(bucket, key)` in the objects table prevents duplicate keys; `arca fsck` detects orphaned blobs and conflicting sidecars.

**`put_object` returns old record** — `MetadataStore::put_object` returns `Option<ObjectRecord>` of the overwritten object so the caller can delete the orphaned blob.

**Multipart composite ETag** — `hex(MD5(binary_MD5(part1) || binary_MD5(part2) || ...))-{count}`. Note: concat the binary MD5 bytes, not hex strings.

**Auth body hash** — Accept `UNSIGNED-PAYLOAD` for streaming uploads. Verify request signature only (headers + URI), not body hash. Body integrity relies on Content-MD5.

**Configuration migration without data migration** — Arca must allow any configuration change (storage backend, encryption, node topology, etc.) without requiring data migration to a new instance. Changes are applied via offline CLI tools (`arca migrate-*`) or live reconfiguration that operate in-place on the existing data directory. This is a hard architectural constraint: unlike MinIO, which forces a fresh instance when changing topology, Arca must always provide a migration path that preserves existing data in place.

**TLS** — `tokio-rustls` + `hyper_util` (not `axum-server`), `ring` crypto backend (not `aws-lc-rs`). Single port for everything including health checks. ArcSwap for cert hot-reload on SIGHUP.

**Encryption (SSE-S3)** — AES-256-GCM via `ring` crate, `EncryptingBlobStore` wraps `FsBlobStore`. Chunk-based streaming (64 KiB). Envelope encryption: random DEK per object, wrapped by master KEK. ETag computed on plaintext. Mixed-mode coexistence. `bucket_config` table for per-bucket settings.

**KMS (SSE-KMS)** — Fetch master key from Vault/OpenBAO KV v2 at startup, cache in memory. `[encryption.kms]` config section, mutually exclusive with `master_key`. `reqwest` with `rustls-tls` backend.

## Web Console & Admin API

**Separated console** — The web console is a separate application in `console/`, not embedded in the Arca binary. Reasons: keeps the binary small (minimal scratch image), allows independent release cycles, supports split deployment (Arca on hardened VM, console on k8s), minimizes attack surface on the storage engine. The console is just another API client.

**Admin API on same port** — Admin endpoints live under `/admin/*` on port 9000, coexisting with the S3 API via path-prefix routing. Auth uses SigV4 (same as S3). Response format is JSON (not S3 XML).

## Known Gotchas

- **Axum `Router::layer()` runs AFTER routing** — middleware applied this way only executes for matched routes. To modify the request URI before routing (e.g. trailing-slash normalization), wrap the Router externally with a tower Layer/Service.
- **mc sends `?uploads=`** (with `=`), not `?uploads` — query param matching in handlers must account for both forms.
- **HTTP/2 SigV4 host header** — In HTTP/2, browsers send `:authority` pseudo-header instead of `Host`. hyper does NOT synthesize a `host` header. Auth middlewares must synthesize `host` from URI authority when missing.
- **Multipart + encryption** — Parts MUST have sidecars written after `put()` when encryption is enabled, so `get()` during `CompleteMultipartUpload` assembly can detect and decrypt them.
- **Console Alpine.js scopes** — Each view is a separate `x-data` scope, they do NOT share state. Store needed data locally in each view's `load()` method.
- **TLS auto-detect limitation** — Fails when multiple key files exist in the cert directory. Tests use `tls-explicit` fragment with explicit filenames to avoid ambiguity.

## Technical Debt

Workarounds and known shortcuts are tracked in `TECH_DEBT.md` at the repo root. The roadmap (`documentation/docs/roadmap.md`) also has a tech debt section that should be kept in sync.

When implementing a workaround or shortcut instead of a proper solution:

1. Add a `TECHDEBT(TD-XXX)` comment in the code at the workaround site.
2. Add a corresponding entry in `TECH_DEBT.md` with the same ID, description, affected files, and proposed fix.
3. Update the tech debt section at the bottom of `documentation/docs/roadmap.md`.

When fixing a tech debt item, remove the `TECHDEBT` markers from code, mark it resolved in `TECH_DEBT.md`, and update the roadmap.

## Test Coverage Table

`README.md` contains a "Test Coverage" table with counts for unit tests, integration tests (boto3 + MinIO), and Ceph s3-tests. **Update this table whenever test counts change** — after adding/removing tests, running Ceph s3-tests with new results, or any change that affects the numbers.

## Session Startup

When starting a new session, always read these files first to rebuild context:

- `CLAUDE.md` (this file)
- `documentation/docs/roadmap.md` — current phase status, post-MVP progress, what's done and what's next
- `TECH_DEBT.md` — active workarounds and their IDs

## Versioning

This project uses **semantic versioning** (MAJOR.MINOR.PATCH).

**Version locations** (all must be in sync):

- `Cargo.toml` root `[workspace.package]` — single source of truth, inherited by all 5 crates via `version.workspace = true`
- `console/index.html` line 10 (`window.ARCA_CONSOLE_VERSION`) — must be bumped manually (standalone HTML, no build tooling)
- `documentation/docs/roadmap.md` Phase Summary table — historical per-phase version tags, update when completing a phase

When asked to bump the version:

1. Diff the current `main` branch against the latest release tag to understand what changed.
2. Determine the correct semver component to bump:
   - **PATCH** — bug fixes, internal refactors, doc-only changes, no API/behavior changes.
   - **MINOR** — new features, new S3 operations, new CLI commands, backward-compatible additions.
   - **MAJOR** — breaking changes to config format, storage layout, API contracts, or anything requiring user migration steps.
3. Propose the new version number with a brief motivation (what changed and why it maps to that semver level).
4. Wait for Pietro's approval or counter-proposal before applying the version bump.
