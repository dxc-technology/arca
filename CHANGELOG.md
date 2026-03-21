# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **DeleteObjects with VersionId**: batch delete (`POST /{bucket}?delete`) now supports `<VersionId>` per object, enabling hard-deletion of specific versions and delete markers. Previously, VersionId was ignored and versioned deletes only created more delete markers, making buckets impossible to empty.

### Changed

- **`bin/s3-tests`**: report generation no longer skipped when pytest exits non-zero (test failures). The script now correctly captures the exit code and proceeds to generate the HTML report, SVG badge, passlist, and summary.json.

## [0.8.0] — 2026-03-20

### Added

- **Phase 17: Object Versioning**
- **Bucket versioning config**: `PutBucketVersioning` / `GetBucketVersioning` with three states (Disabled / Enabled / Suspended)
- **Version IDs**: every `PutObject` on a versioned bucket generates a UUID version ID, returned via `x-amz-version-id` header
- **Delete markers**: `DeleteObject` on versioned buckets creates a delete marker instead of removing the object; `x-amz-delete-marker: true` header
- **Version-specific operations**: `GetObject?versionId=X`, `HeadObject?versionId=X`, `DeleteObject?versionId=X` for accessing or permanently deleting specific versions
- **ListObjectVersions**: rewritten with real version data, delete markers, and `Owner` elements (replaces TD-003 fake implementation)
- **CopyObject with source versionId**: `x-amz-copy-source: bucket/key?versionId=X` copies a specific version
- **Suspended versioning**: new writes get null version ID, existing real versions preserved
- **Batch DeleteObjects**: safe on versioned buckets (creates delete markers, no panic on empty blob_id)
- **SQLite migration v9**: objects table recreated with `version_id`, `is_latest`, `is_delete_marker` columns, partial unique index
- **Error codes**: `NoSuchVersion` (404), `MethodNotAllowed` (405) for GET on delete markers
- **`arca recover`**: sorts sidecars by `last_modified` to recover newest version; `SidecarMeta` gains `version_id` field
- **`arca fsck`**: handles all object versions and skips blob checks for delete markers
- **Web console**: bucket versioning toggle (Enable/Suspend) matching encryption switch style
- **Web console**: versioning indicators (clock icon) in dashboard, bucket list, and bucket detail breadcrumbs (blue=Enabled, amber=Suspended)
- **Web console**: version history panel in object detail with download/delete per version, Latest badge, delete marker indicators
- **Web console**: "Show deleted" toggle in bucket browser header for versioned buckets, showing deleted files and directories with strikethrough + red badge
- **Web console**: scrollable breadcrumbs for deep directory paths with auto-scroll to deepest level
- **Web console**: centralized SVG icons (`icons.encryptionShield`, `icons.versioningClock`, `icons.versioningSuspended`) in `app.js`
- 13 new unit tests for versioning (put/get/delete versioned, delete markers, suspended mode, stats)
- 20 versioning integration tests (config, PUT, GET, HEAD, DELETE, batch delete, ListVersions, CopyObject, backward compat)
- Resolves: TD-003

### Fixed

- File re-upload after delete now works (reset file input value after upload)
- Batch `DeleteObjects` no longer panics on versioned buckets (skip blob deletion for delete markers)

## [0.7.0] — 2026-03-18

### Added

- **Phase 16: Access Control and Bucket Policies**
- **RBAC foundation**: users, teams, grants (policy documents) with full CRUD via Admin API
- **Policy evaluation engine**: AWS IAM-style policy documents with Effect/Action/Resource matching, wildcard support, deny-overrides evaluation
- **S3 authorization middleware**: non-root users are authorized against effective policies for every S3 operation (action + resource ARN matching)
- **Admin authorization**: non-root users need `arca:*` grants to access admin endpoints (e.g., `AdministratorAccess` grant)
- **Identity resolution**: credentials linked to users, users belong to teams, grants attach to users or teams. Effective policies = direct grants + team-inherited grants
- **Built-in grants**: AdministratorAccess, S3FullAccess, S3ReadOnlyAccess created during migration
- **Owner model**: buckets and objects track their creator's username (resolves TD-001)
- **Admin API endpoints**: `/admin/me`, `/admin/users`, `/admin/teams`, `/admin/grants` with full CRUD, membership management, grant attachments, effective grant queries
- **CLI `arca user` subcommand**: `create`, `list`, `delete` for offline user management
- **CLI `arca credential add --user`**: associate new credentials with specific users
- **Web console**: Users, Teams, Grants management views with dual-list shuttle components for membership and grant assignment
- **Web console**: inline editing of names and descriptions for users, teams, grants, and credentials (save-on-change)
- **Web console**: credential activate/deactivate toggle on credential cards
- **Admin API**: `PUT /admin/credentials/{id}` for updating credential active status and description
- **Admin API**: `PUT /admin/users/{id}` now accepts optional `username` field for renaming
- **Admin API**: `PUT /admin/teams/{id}` now accepts optional `name` field for renaming
- **Documentation**: Access Control guide page with diagrams explaining the identity model, auth flow, and effective grants
- **Documentation**: database ER diagram (Mermaid) in architecture page showing all 12 tables and relationships
- **Documentation**: full Admin API reference (37 endpoints), updated CLI reference, console manual with 19 screenshots
- **Ceph s3-tests**: machine-readable `summary.json` output after test runs
- **SQLite migration v8**: users, teams, grants, team_members, user_grants, team_grants tables; owner fields on buckets/objects; user_id on credentials
- 41 RBAC integration tests (user/team/grant CRUD, attachments, effective grants, E2E access control)
- 90 new unit tests (47 policy evaluator, 37 RBAC store implementations, 6 credential/user/team update)

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

[Unreleased]: https://github.com/dxc-technology/arca/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/dxc-technology/arca/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/dxc-technology/arca/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/dxc-technology/arca/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/dxc-technology/arca/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/dxc-technology/arca/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/dxc-technology/arca/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/dxc-technology/arca/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/dxc-technology/arca/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dxc-technology/arca/commits/v0.1.0
