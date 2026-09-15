# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Arca is an open source S3-compatible object storage server written in Rust. It is designed as a drop-in replacement for AWS S3, MinIO, and other S3-compatible storage services, targeting 100% S3 API compatibility for a focused MVP subset (15 operations). Starts as single-node with eventual production scale in mind. Licensed under AGPL-3.0-or-later.

The full architecture plan lives in `.claude/plans/arca-s3-mvp-architecture.md`.

## Dependency Licensing Policy (HARD CONSTRAINT)

Arca's own license (AGPL-3.0-or-later) does not exempt its dependencies: a copyleft dependency (GPL/AGPL/LGPL/MPL/SSPL/CDDL/EPL/OSL) anywhere in the transitive tree would stack extra source-disclosure or network-copyleft obligations, beyond AGPL's own, onto every user of Arca. To keep the license surface simple and predictable for anyone consuming Arca:

- **Only permissively licensed dependencies are allowed**, transitively down to the leaves: MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, BSL-1.0, Unicode-3.0, CC0-1.0, 0BSD, MIT-0, CDLA-Permissive-2.0, Unlicense, and equivalents.
- **Copyleft is forbidden** — no GPL, AGPL, LGPL, MPL, SSPL, CDDL, EPL, or OSL, direct or transitive. A crate multi-licensed with `OR` is acceptable **only if** it always offers a permissive option (e.g. `Apache-2.0 OR LGPL-2.1-or-later OR MIT` is fine because Apache-2.0/MIT can be chosen).
- **Before adding or upgrading any dependency**, run the full transitive audit in a container and confirm zero copyleft:
  ```bash
  docker run --rm -v "$PWD":/work -w /work rust:alpine sh -c \
    'apk add --no-cache musl-dev >/dev/null; cargo install cargo-license >/dev/null; \
     cargo license --all-features --tsv' \
    | awk -F"\t" "NR>1{print \$5}" | grep -wiE "GPL|AGPL|LGPL|MPL-|SSPL|CDDL|EPL|OSL"
  # must print nothing (an OR-with-permissive-option match must be reviewed by hand)
  ```
- If a needed capability is only available under a copyleft license, **STOP and ask Pietro** before pulling it in — do not add it on your own initiative.
- After any dependency change, **regenerate `THIRD-PARTY-NOTICES`** so the attribution file stays in sync (config in `about.toml` / `about.hbs`). The `cargo-about` binary is gated behind the `cli` feature, so the install must pass `--features cli`:
  ```bash
  docker run --rm -v "$PWD":/work -w /work rust:alpine sh -c \
    'apk add --no-cache build-base perl; cargo install cargo-about --features cli; \
     cargo about generate --all-features about.hbs -o THIRD-PARTY-NOTICES.md'
  ```

## Language

Arca is an international project: **everything committed to the repository must be in English** — code, comments, documentation, commit messages, and the planning/review documents under `.claude/`. Conversations with Pietro may happen in Italian, but no Italian may end up in tracked files.

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
bin/test tls-permissions # TLS file-permission tests (named volume, self-contained)
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

- The Docker builder's Rust version is whatever its pinned Alpine base image tag carries — read `docker/Dockerfile`, don't assume. Minimum is **1.85**: `getrandom 0.4` requires edition 2024.
- `ring` crate needs `perl` in Alpine; `rdkafka` (Kafka connector) needs `cmake make g++ curl-dev linux-headers zlib-dev zlib-static` for librdkafka static build.
- `time` crate pinned to 0.3.47; 0.3.48+ excluded for a coherence clash with `rcgen` (TD-017), not for MSRV reasons.
- `Cargo.lock*` glob in Dockerfile allows building with or without committed lockfile.
- `bin/build` only rebuilds the `arca` service image, NOT `unit-test` or `test`. After code changes, run `docker compose -f docker/docker-compose.yml build unit-test test` or test images will be stale.

## Base Image Pinning and Upgrades

Every image Arca **deploys** pins its base image to an exact version tag, so the
same Dockerfile keeps producing the same toolchain and the same runtime
packages over time:

| Dockerfile | Stage | Base |
|---|---|---|
| `docker/Dockerfile` | `builder` (and `dev`, which inherits it) | Rust on Alpine — **pinned** |
| `docker/Dockerfile` | `production` | `scratch` — no tag exists, nothing to pin |
| `docker/Dockerfile` | `development` | Debian slim — **pinned** |
| `console/Dockerfile` | — | Alpine — **pinned** |

**Never write the concrete pinned versions anywhere but the `FROM` lines
themselves.** The Dockerfiles are the single source of truth; a tag copied into
this file, the documentation site or a guide is a second place to forget at the
next bump. State the principle and point at the Dockerfile. (`CHANGELOG.md` is
the one exception — it records what changed on a given date, so exact versions
belong there.)

Images used **only for testing and tooling** deliberately stay unpinned
(`python:alpine`, `python:3-slim`, `mcr.microsoft.com/playwright/python:…`, and
the third-party services in the `docker-compose.connector-*.yml` /
`docker-compose.kms.yml` / `docker-compose.cluster.yml` overlays). They are
never distributed, and floating tags there mean one less thing to maintain.
Do not pin them.

### Upgrading a pinned base image

Bumping a base image tag is a deliberate, reviewed step, never a drive-by edit,
because **the in-Dockerfile package pins are relative to a specific base image
version** and a newer base can already ship the fixed version — or a newer one,
which makes the pin dead weight (or, with `=`, a build failure).

Those package pins exist to remediate a specific advisory the base image had
not picked up yet. Each one sits next to a comment naming the package, the
advisories that forced it, and the version the base ships — find them with:

```bash
grep -rn "apk add\|apt-get install" docker/Dockerfile console/Dockerfile \
    docker/webhook-receiver/Dockerfile docker/grpc-receiver/Dockerfile
```

As of writing, the console pins `libcrypto3`/`libssl3` (openssl) and the two
connector-test receivers pin `libuuid` (util-linux). Read the comments for the
current constraints rather than trusting this paragraph.

Procedure:

1. **Find the new tag.** Query the registry, don't guess — for Docker Hub
   library images:
   ```bash
   curl -s "https://hub.docker.com/v2/repositories/library/alpine/tags/?page_size=100&ordering=last_updated" \
     | jq -r '.results[] | "\(.name)\t\(.last_updated)"'
   ```
   Pick the exact version that the floating tag currently resolves to, and
   confirm the pin is digest-identical to it before committing:
   ```bash
   docker manifest inspect alpine:latest     | jq -r '.manifests[0].digest'
   docker manifest inspect alpine:<new-tag>  | jq -r '.manifests[0].digest'   # must match
   ```
   For Debian prefer the **numeric** point release (`<major>.<minor>-slim`)
   over `stable-slim`: same rootfs, but the numeric tag also pins the apt suite
   to the codename, so `apt-get install` cannot silently cross a Debian major
   release. For Rust keep the Alpine suffix (`-alpine<major>.<minor>`) — it
   fixes the apk repository branch, not just the compiler.

2. **Revise every package pin for that image.** For each pin the `grep` above
   turns up, check what the *new* base already ships and drop or raise the pin
   accordingly:
   ```bash
   docker run --rm alpine:<new-tag> sh -c 'apk update >/dev/null && apk policy libcrypto3 libssl3'
   docker run --rm debian:<new-tag> sh -c 'apt-get update >/dev/null 2>&1 && apt-cache policy ca-certificates curl'
   ```
   If the base's own version already satisfies the advisory, **remove the pin**
   and delete its comment block — a stale `>=` constraint that the base has
   overtaken is misleading, and the CVE list in the comment stops being true.
   If it does not, keep the pin and update the comment to name the new base's
   version. Never replace a pin with a blanket `apk upgrade` / `apt upgrade`:
   that makes the build irreproducible.

3. **Re-check the Rust side** when bumping the builder: a new compiler can
   unblock crate versions that MSRV pins in `Cargo.toml` were working around
   (see the `time`/TD-017 bullet under Build constraints), and Alpine branch
   changes can move the C dependencies `ring`/`rdkafka` build against.

4. **Rebuild and verify.** `bin/build --dev && bin/build --console`, then
   `bin/test unit` and `bin/test integration`, then scan: `trivy image` must
   report **0 findings** for `arca`, `arca-console`, `arca-webhook-receiver`
   and `arca-grpc-receiver`. Scan the old and the new image with the *same*
   tool before claiming an improvement — counts from different scanners are
   not comparable.

5. **Record it in `CHANGELOG.md`** — and nowhere else. If the bump changed a
   package pin or the Rust version, say so there. No other file should need
   editing; if one does, it was carrying a version it shouldn't have. Then
   `bin/docs-build` if you touched the changelog (the docs site symlinks it).

## Documentation

The documentation site uses MkDocs with Material theme, built inside a Docker container.

**Dual-directory structure**: source files live in `documentation/docs/`, built output goes to `docs/` (served by GitHub Pages). Always update BOTH directories, then commit changes in both `documentation/` and `docs/`.

After every phase completion or significant feature change, update documentation BEFORE committing code: roadmap checkboxes, configuration page, and installation/Quick Start as needed. Rebuild with `bin/docs-build`.

### Console documentation and screenshots

The console user manual lives at `documentation/docs/guide/console.md`. Screenshots are automated via Playwright in Docker. When the console gains new features, update both the screenshots and the documentation:

1. **Update `docker/screenshots/take_screenshots.py`** — add new screenshot captures (seed data in Phase A, capture in Phase B). Each screenshot is a numbered step. The script runs inside a container that shares network with a dedicated `screenshots-console` service (required for Web Crypto API / localhost).
2. **Update `documentation/docs/guide/console.md`** — add/update sections and screenshot references.
3. **Run `bin/screenshots --build`** — rebuilds all images (arca, console, screenshots), starts Arca with encryption enabled (for encryption indicators in screenshots), seeds sample data, and captures all screenshots to `documentation/docs/assets/screenshots/`.
4. **Run `bin/docs-build`** — rebuilds the MkDocs site so `docs/` has the new screenshots and HTML.
5. Commit changes in `bin/`, `docker/screenshots/`, `documentation/`, and `docs/`.

`bin/screenshots` uses the `compose.sh` library and enables encryption by default. Pass `--no-encryption` to capture without encryption indicators.

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

After running `bin/s3-tests`, the report script generates `s3-tests/summary.json` with machine-readable results (total, passed, failed, skipped, pass_pct, unexpected_failures, per-category breakdown). **Always read `s3-tests/summary.json` after Ceph tests finish and update the Ceph row in the README test coverage table with the actual numbers.** The SVG badge at `s3-compatibility-badge.svg` is auto-updated by the report script.

## Session Startup

When starting a new session, always read these files first to rebuild context:

- `CLAUDE.md` (this file)
- `documentation/docs/roadmap.md` — current phase status, post-MVP progress, what's done and what's next
- `TECH_DEBT.md` — active workarounds and their IDs

## Versioning and Releases

This project uses **semantic versioning** (MAJOR.MINOR.PATCH). The full release procedure is in `RELEASING.md`. Read that file when asked to "make a new release" or bump the version.

**Version locations** (all must be in sync):

- `Cargo.toml` root `[workspace.package]` — single source of truth, inherited by all 5 crates via `version.workspace = true`
- `console/index.html` line 10 (`window.ARCA_CONSOLE_VERSION`) — must be bumped manually (standalone HTML, no build tooling)
- `documentation/docs/roadmap.md` Phase Summary table — historical per-phase version tags, update when completing a phase
- `CHANGELOG.md` — [Keep a Changelog](https://keepachangelog.com) format. The docs site symlinks to this file (`documentation/docs/changelog.md` → `../../CHANGELOG.md`).
