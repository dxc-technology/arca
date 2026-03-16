# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] — 2026-03-16

### Added

- **Phase 15: Presigned URLs, Query-String Auth, and SSE-C**
- **Query-string SigV4 authentication**: presigned URLs with `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, `X-Amz-Signature` query parameters
- **Presigned URL generation**: `generate_presigned_url()` in `arca-auth` for server-side URL creation
- **Admin presign endpoint** (`POST /admin/presign`): generate presigned GET/PUT URLs with configurable expiry, optional endpoint override for correct public URL, percent-encoded paths
- **SSE-C** (Server-Side Encryption with Customer-provided keys): AES-256-GCM encryption where the customer provides the key on each request. Supported for PutObject, GetObject, HeadObject, and CopyObject (including cross-mode: SSE-C to plain, plain to SSE-C, SSE-C to SSE-C with different keys)
- **Console: Share button** on objects with presigned URL generation, expiry selector (1h/6h/1d/7d), and copy-to-clipboard
- SSE-C CORS headers for cross-origin browser access
- 17 presigned URL integration tests (GET, PUT, HEAD, DELETE, security, admin presign, special chars, endpoint override)
- 17 SSE-C integration tests (put/get roundtrip, error handling, head, copy, range reads, delete, validation, multipart rejection)
- 18 new unit tests (12 auth, 6 storage) for presigned URL parsing/signing and SSE-C encryption

## [0.5.0] — 2026-03-16

### Added

- **Streaming archive endpoint** (`POST /admin/archive`): download multiple objects as a tar.gz archive streamed on-the-fly, with no temporary files on the server
- **Console: directory upload** via file picker and drag-and-drop, preserving directory structure
- **Console: multi-select** with checkboxes, select-all, and floating action bar
- **Console: batch delete** with recursive directory support and confirmation dialog (warnings for multiple items and recursive directory wipes)
- **Console: batch download** as streaming tar.gz archive
- `CHANGELOG.md` with full release history
- `RELEASING.md` with release procedure checklist

## [0.4.1] — 2026-03-16

### Added

- **Plugin system**: composable features via `--flags` (`--tls`, `--encryption`, `--kms`), replacing combinatorial config/compose files
- **Binary extraction**: `bin/build --binary [--arch amd64|arm64]` extracts a standalone Linux binary to `build/`
- **Console: disk usage** shown in dashboard stats
- **Console: documentation link** in sidebar
- `--help` flag on all `bin/` scripts
- Auto-create master key in Vault/OpenBAO at startup if missing

### Changed

- Centralized all builds in `bin/build` with `--dev`, `--console`, `--binary` flags
- Consolidated version in workspace `Cargo.toml` (single source of truth, inherited by all crates)
- Use `debian:stable-slim` instead of `bookworm-slim` for dev image

### Fixed

- Foreground mode (`bin/arca start`) not stopping associated services on Ctrl+C
- Treemap button not rendering on click in console

## [0.4.0] — 2026-03-15

### Added

- **Phase 14: SSE-KMS with Vault/OpenBAO** — fetch master encryption key from Vault/OpenBAO KV v2 at startup, `[encryption.kms]` config section, `reqwest` with `rustls-tls` backend
- **Per-bucket encryption** with bucket settings console page
- Database schema diagram in architecture documentation (SVG for dark mode)

### Changed

- Adopted semver `0.x.x` pre-release versioning (from `1.2.0` to `0.3.0`)
- Console uses standard ports (80/443) instead of 3000
- Replaced `RUST_LOG` with `ARCA_LOG` for log level configuration
- Replaced ICO favicon with transparent PNG for Safari/Chrome compatibility

## [0.3.0] — 2026-03-13

### Added

- **Phase 13: Server-Side Encryption (SSE-S3)** — AES-256-GCM via `ring`, chunk-based streaming (64 KiB), envelope encryption (random DEK per object wrapped by master KEK), ETag computed on plaintext, mixed-mode coexistence, `bucket_config` table for per-bucket settings
- `--config (-c)` option for custom config files in `bin/arca`
- Combined TLS + encryption config for development
- Version number in web console sidebar

## [0.2.0] — 2026-03-13

### Added

- **Phase 12: Native TLS/SSL** — `tokio-rustls` + `hyper_util`, `ring` crypto backend, single port for all traffic including health checks, ArcSwap for cert hot-reload on SIGHUP
- Console HTTPS support
- Full post-MVP product roadmap (Phases 12–26) with dependency diagram

### Fixed

- HTTP/2 SigV4 host header (`:authority` pseudo-header handling)

## [0.1.0] — 2026-03-10

### Added

- **Phases 0–11: complete MVP** — 15 S3 operations, single-node server
- **S3 API**: CreateBucket, HeadBucket, ListBuckets, DeleteBucket, PutObject, GetObject, HeadObject, DeleteObject, DeleteObjects, CopyObject, ListObjectsV2, CreateMultipartUpload, UploadPart, CompleteMultipartUpload, AbortMultipartUpload
- **Five-crate Cargo workspace**: arca-core, arca-auth, arca-proto, arca-storage, arca-server
- **Streaming-first architecture**: never buffers full objects in memory
- **AWS SigV4 authentication** with credential CRUD via Admin API
- **Disaster recovery**: sidecar `.meta` files, `arca recover` for full DB rebuild, `arca fsck` for integrity checking
- **Web console**: Alpine.js + Tailwind CSS dashboard, bucket browser, credential management, role-aware UI
- **Admin API**: health, info, stats, credential CRUD under `/admin/*`
- **S3 compatibility hardening**: 270/829 Ceph s3-tests passing (32.6%), MinIO Client support
- **Documentation site**: MkDocs with Material theme, architecture docs, user guides
- Scratch-based production Docker image (8.6 MB)

[Unreleased]: https://github.com/dxc-technology/arca/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/dxc-technology/arca/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/dxc-technology/arca/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/dxc-technology/arca/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/dxc-technology/arca/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/dxc-technology/arca/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dxc-technology/arca/commits/v0.1.0
