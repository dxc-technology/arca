# AGENTS.md

Guidance for coding agents (Claude Code and any other AGENTS.md-aware tool) working in this repository. This file holds the rules that always apply; task-specific procedures live elsewhere and **must be read before starting the task**:

| Before you... | Read |
|---|---|
| work on a tech debt item, or touch code carrying a `TECHDEBT(TD-XXX)` marker | that item in `TECH_DEBT.md` |
| work on the next roadmap step, or complete a phase | `documentation/docs/roadmap.md` |
| add or upgrade a dependency | `DEVELOPMENT.md` → Dependency licence audit |
| add a test mode, a suite needing a mode's own services, or a port in a compose overlay | `DEVELOPMENT.md` → Self-contained test modes |
| bump a base image or a package pin in a Dockerfile | `DEVELOPMENT.md` → Base Image Pinning and Upgrades |
| touch image tags, labels or the publish pipeline | `DEVELOPMENT.md` → Published Container Images |
| add or change a console feature | `DEVELOPMENT.md` → Console documentation and screenshots |
| make a release or bump the version | `RELEASING.md` |
| change architecture or HA behaviour | `documentation/docs/reference/architecture.md`, `ha-design-decisions.md` |
| run or analyse performance tests | `documentation/docs/operations/performance.md` |

## Project

Arca is an open source S3-compatible object storage server in Rust, a drop-in replacement for AWS S3, MinIO and other S3-compatible services. Single-node or symmetric HA cluster, SQLite or PostgreSQL metadata. AGPL-3.0-or-later.

Architecture: `documentation/docs/reference/architecture.md`; HA decisions cited in code as "decision H<n>": `documentation/docs/reference/ha-design-decisions.md`.

## Dependency Licensing Policy (HARD CONSTRAINT)

A copyleft dependency anywhere in the transitive tree would stack extra obligations, beyond AGPL's own, onto every user of Arca.

- **Only permissive licences**, transitively down to the leaves: MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, BSL-1.0, Unicode-3.0, CC0-1.0, 0BSD, MIT-0, CDLA-Permissive-2.0, Unlicense, and equivalents.
- **Copyleft is forbidden** (GPL, AGPL, LGPL, MPL, SSPL, CDDL, EPL, OSL), direct or transitive. An `OR` multi-licence is fine **only if** it always offers a permissive option.
- **Before adding or upgrading any dependency**, run the transitive audit, and after any dependency change regenerate `THIRD-PARTY-NOTICES.md` (commands in `DEVELOPMENT.md`).
- If a capability is only available under copyleft, **STOP and ask Pietro**.

## Language

**Everything committed must be in English**: code, comments, docs, commit messages, plans, reviews. Conversations with Pietro may be in Italian; no Italian in tracked files.

## Build & Test

All development happens inside Docker containers, never install libraries on the host. `bin/` scripts fully abstract Docker Compose; all builds go through `bin/build`.

**Plugin system**: features (TLS, encryption, KMS) compose via `--flags`. Each has a config fragment in `config/fragments/` and an optional overlay in `docker/`; `bin/lib/compose.sh` concatenates the TOML and builds the compose command. New feature = 1 fragment + a few lines in the library.

```bash
bin/build [--dev|--console]      # production / development (has shell) / console image
bin/build --binary [--arch amd64]  # extract (cross-compiled) Linux binary to build/arca-<arch>
bin/build --push                 # publish the release at HEAD to ghcr.io (fallback; CI does it)
bin/release vX.Y.Z               # create the GitHub release

bin/arca start -d [--build --dev] [--tls] [--encryption] [--kms] [--config <path>]
                                 # --tls: certs in ./certs/, else a local CA in certs/local
bin/arca start -d --release latest  # run the published image: pull, never build
                                 # (also bin/console start --release, bin/cluster up --release)
bin/arca stop | status | logs -f
bin/console start -d [--build] | stop   # inherits features from running Arca via .arca-env

bin/test                 # unit + integration
bin/test full            # unit, integration, notifications, replication, cluster
bin/test unit [-p CRATE] # extra args go to cargo test
bin/test integration     # against the running server as started; .arca-env enables encryption/KMS suites
bin/test tls | tls-permissions | encryption | per-bucket-encryption | virtual-host | kms
bin/test cluster | replication | notifications     # self-contained
bin/test scripts | s3-report | perf-report         # no server needed

bin/perf-test [suite [--profile quick --targets arca --variants plain]]
bin/perf-test report perf-results/<run> --baseline perf-results/<earlier>
                         # perf-results/ is git-ignored; see documentation/docs/operations/performance.md

bin/docs-build | docs-serve | docs-publish
aws s3 ls --endpoint-url http://localhost:9000   # manual check
```

Self-contained test modes run in their own compose project and never touch the developer's stack. **Before adding a test mode, a suite that needs a mode's own services, or a port in a compose overlay**, read "Self-contained test modes" in `DEVELOPMENT.md`.

### Build constraints

- The builder's Rust version is whatever its pinned base image carries: read `docker/Dockerfile`. Minimum **1.85** (`getrandom 0.4` needs edition 2024).
- `ring` needs `perl` in Alpine; `rdkafka` needs `cmake make g++ curl-dev linux-headers zlib-dev zlib-static`.
- `time` pinned to 0.3.47: 0.3.48+ clashes with `rcgen` (TD-017).
- `Cargo.lock*` glob in the Dockerfile allows building with or without a lockfile.
- `bin/build` does NOT rebuild `unit-test` / `test`: after code changes run `docker compose -f docker/docker-compose.yml build unit-test test`, or tests run stale code.

### Base images

Deployed images pin their base to an exact tag; **never write a pinned version anywhere but the `FROM` line** (`CHANGELOG.md` excepted). Test and tooling images stay unpinned on purpose, except the docs image (bumped only on request, in its own commit). A bump is never a drive-by edit: follow "Base Image Pinning and Upgrades" in `DEVELOPMENT.md`. All four images must scan clean with `trivy`.

## Documentation

MkDocs Material, built in Docker. Sources in `documentation/docs/`, built output in `docs/` (GitHub Pages): update and commit BOTH. After a phase or significant feature, update docs BEFORE committing code (roadmap checkboxes, configuration page, installation/Quick Start), then `bin/docs-build`. When the console gains features, follow "Console documentation and screenshots" in `DEVELOPMENT.md`.

## Architecture

Five-crate workspace, no cycles:

- **arca-core**: shared types, traits (`BlobStore`, `MetadataStore`), errors. No I/O.
- **arca-auth**: SigV4 verification. No I/O, no Arca dependency, tested against AWS vectors.
- **arca-proto**: S3 + admin HTTP adapter (Axum 0.8 + Tower). Handlers hold the request logic and call the storage traits directly (no use-case layer); XML ser/de; middleware (auth, virtual-host rewrite, CORS, rate limiting, audit).
- **arca-storage**: `FsBlobStore` (UUID + sidecar) and decorators (`EncryptingBlobStore`, `CompressingBlobStore`, `SsecBlobStore`), `SqliteStore` (tokio-rusqlite, WAL, one writer + read pool), `PgStore`, `CachingMetadataStore`.
- **arca-server**: binary. Wiring, config (TOML + env), CLI (`crates/arca-server/src/cli.rs`, `documentation/docs/guide/cli.md`), background workers, HA cluster decorators (`src/cluster/`).

`arca-server` -> all; `arca-proto` -> `arca-core`, `arca-auth`; `arca-storage` -> `arca-core`. `arca-auth` and `arca-core` are leaves.

## Key Design Constraints

**No `s3s` crate**: our own S3 adapter on Axum (avoid pre-1.0 dependency risk).

**S3 query-parameter routing**: same method+path, different query params; handlers dispatch internally (e.g. `PUT /:bucket/*key` -> PutObject vs UploadPart on `?partNumber=&uploadId=`).

**Streaming-first**: never buffer full objects in memory.

**Storage write order**: blob -> sidecar `.meta` -> metadata row, so `arca recover` can rebuild the DB from `blobs/` (gaps: TD-014, TD-032). Durability rules:
- Use the helpers in `arca-storage/src/fs/blob.rs` (`rename_durable`, `write_file_atomic`, `create_dir_all_durable`), never plain `fs::write`/`rename`. Temp files are opened with `create_new`, then renamed.
- **A blob is durable once its sidecar is written**: blob renames skip their own directory fsync because the sidecar's covers both. So every blob writer must write the sidecar, and fail the request (discarding the blob) if that fails, before committing the row (`handlers/sidecar.rs`: `write_sidecar_or_discard`); a writer with no sidecar must use `rename_durable`.
- `[storage] fsync = false` makes `FsBlobStore::with_fsync` / `SqliteStore::open_with_fsync` skip fsyncs (temp + rename kept). Only the live server reads it: offline tools use `FsBlobStore::new` / `SqliteStore::open`, always durable, and must stay so.
- A partial unique index on `(bucket, key) WHERE is_latest = 1` prevents two current versions.

**`put_object` returns the old record**: `(Option<ObjectRecord>, Option<String>)`, the overwritten object (caller deletes its blob) and the new version id. It delegates to `put_object_if`, which checks conditional-write preconditions in the same transaction.

**Multipart composite ETag**: `hex(MD5(binary_MD5(part1) || ...))-{count}`, concatenating binary MD5s, not hex strings.

**Body integrity** (SigV4 covers headers + URI, not the body; `UNSIGNED-PAYLOAD` and `STREAMING-*` accepted):
- PutObject/UploadPart check `Content-MD5`, `x-amz-checksum-*` (header or trailer) and hex `x-amz-content-sha256` after the blob is written, before sidecar and row, deleting the blob on mismatch (`handlers/integrity.rs`).
- Signed aws-chunked bodies are verified while streaming (`auth_middleware` -> `ChunkVerifier` extension -> `verified_body_stream`, `handlers/body.rs`). The lenient `body_to_byte_stream` is for inter-node bodies only.
- Multipart checksums are computed at Complete from part checksums (`handlers/multipart_checksum.rs`, `plan_complete` validates first). The `objects.parts` column is read only by `get_object_parts`: S3 read and write queries must NOT select it, only `apply_remote_object` and the replication manifest do (`OBJECT_COLUMNS` vs `OBJECT_COLUMNS_FULL` in `sqlite/metadata.rs`, `pg/metadata.rs`).

**Configuration migration without data migration** (hard constraint): any config change (backend, encryption, topology, ...) must have an in-place path, offline (`arca migrate-*`) or live, on the existing data directory. Never require a fresh instance.

**TLS**: `tokio-rustls` + `hyper_util` (not `axum-server`), `ring` (not `aws-lc-rs`), single port including health checks, ArcSwap cert hot-reload on SIGHUP.

**SSE-S3**: AES-256-GCM (`ring`), `EncryptingBlobStore` over `FsBlobStore`, 64 KiB chunks, random DEK per object wrapped by master KEK, ETag on plaintext, mixed-mode coexistence, per-bucket settings in `bucket_config`.

**SSE-KMS**: master key fetched from Vault/OpenBAO KV v2 at startup and cached; `[encryption.kms]`, mutually exclusive with `master_key`; `reqwest` with `rustls-tls`.

**Console**: separate app in `console/`, just another API client (small binary, independent releases, split deployment, smaller attack surface). **Admin API** under `/admin/*` on port 9000, SigV4 auth, JSON responses.

## Known Gotchas

- **Axum `Router::layer()` runs AFTER routing**: to rewrite the URI before routing, wrap the Router with an external tower Layer.
- **mc sends `?uploads=`** (with `=`): match both forms.
- **HTTP/2 has no `host` header** (only `:authority`) and hyper does not synthesize it: auth must derive it from the URI.
- **Multipart + encryption**: parts MUST get sidecars after `put()`, so Complete can detect and decrypt them.
- **Console Alpine.js scopes**: each view is its own `x-data`, no shared state; load data in each view's `load()`.
- **TLS auto-detect** fails with several key files in the cert directory; tests use the `tls-explicit` fragment.

## Planning Large Changes

Plans and reviews stay out of the repository (or are deleted when done). First move anything durable (decisions and rationale, invariants, known limits) into `documentation/docs/` or `TECH_DEBT.md`, and make sure no code comment cites an ID defined only in the plan.

For features spanning many subsystems: design the failure path of every guarantee, include an operations lifecycle (deploy, upgrade, resize, recover), and audit every existing subsystem for assumptions the feature changes (e.g. N nodes instead of one).

## Technical Debt

For a workaround or shortcut: add a `TECHDEBT(TD-XXX)` comment at the site, an entry with the same ID in `TECH_DEBT.md` (description, files, proposed fix), and update the tech debt section of `documentation/docs/roadmap.md`. When fixing one: remove the markers, mark it resolved, update the roadmap.

## Test Coverage Table

Update the "Test Coverage" table in `README.md` whenever test counts change. After `bin/s3-tests`, read `s3-tests/summary.json` and update the Ceph row with its numbers (the badge SVG updates itself).

## Versioning and Releases

Semantic versioning. Read `RELEASING.md` when asked to make a release or bump the version: step 4 lists every version location that must stay in sync (`Cargo.toml` `[workspace.package]` is the source of truth). `CHANGELOG.md` follows Keep a Changelog; the docs site symlinks it.
