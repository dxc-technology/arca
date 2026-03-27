# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Phase 22: S3 API Completeness** — `ListParts` (`GET /{bucket}/{key}?uploadId=X`) with pagination, `GetObjectAttributes` (`GET /{bucket}/{key}?attributes`) with ETag/Checksum/ObjectParts/StorageClass/ObjectSize. Checksum algorithms: store and return client-provided `x-amz-checksum-sha256/crc32/crc32c/crc64nvme` on PutObject, GetObject, HeadObject. Storage classes: `storage_class` field on ObjectRecord, accept `x-amz-storage-class` header. Schema migration v13 adds storage_class, checksum_algorithm, checksum_value to objects; checksum_value, last_modified to parts; checksum_algorithm to multipart_uploads. Resolves TD-002 (storage class) and TD-008 (content-type source)

## [0.13.0] — 2026-03-26

### Added

- **Phase 21: Object Lock (WORM Compliance)** — 6 new S3 operations: `PutObjectLockConfiguration`, `GetObjectLockConfiguration`, `PutObjectRetention`, `GetObjectRetention`, `PutObjectLegalHold`, `GetObjectLegalHold`. GOVERNANCE mode (bypassable with permission) and COMPLIANCE mode (absolute protection). Per-object retention with retain-until-date, legal hold (ON/OFF). Default retention applied from bucket config on PutObject. Enforcement blocks hard-deletion of locked versions. Delete markers always allowed. Auto-enables versioning, prevents suspension. Schema migration v12 adds `retention_mode`, `retain_until_date`, `legal_hold_status` columns. 7 new S3 policy actions including `s3:BypassGovernanceRetention`. Lifecycle worker respects Object Lock
- **Console: Object Lock** — Enable Object Lock with retention mode/period in bucket settings, lock badge in bucket list and breadcrumbs, versioning suspend disabled when locked
- **Phase 20: Lifecycle Rules** — S3-compatible lifecycle management with 3 new operations: `PutBucketLifecycleConfiguration`, `GetBucketLifecycleConfiguration`, `DeleteBucketLifecycleConfiguration`. Rules support object expiration after N days, noncurrent version expiration, and abort of incomplete multipart uploads. Filter by prefix, tag, or combined (And filter). Rules stored as JSON in `bucket_config` table (no schema migration required)
- **Lifecycle background worker** — periodic evaluation of lifecycle rules across all buckets. Configurable interval via `lifecycle_evaluation_interval` admin setting (default: 1 hour). Batch processing (100 objects per rule per cycle). Audit logging for all lifecycle actions (`Lifecycle::ExpireObject`, `Lifecycle::ExpireNoncurrentVersion`, `Lifecycle::AbortMultipartUpload`)
- **Console: lifecycle rules editor** — manage lifecycle rules in bucket settings: add/remove rules with prefix filter, expiration days, noncurrent version days, and abort upload days. Immediate persistence on add/remove/toggle
- **Admin setting: `lifecycle_evaluation_interval`** — configurable lifecycle evaluation interval (60-86400 seconds) via `GET/PUT/DELETE /admin/settings/lifecycle_evaluation_interval`

### Fixed

- **Tagging: tag validation on PutObject** — invalid `x-amz-tagging` headers (excess tags, key/value too long) now rejected with 400 before writing the blob, instead of being silently ignored
- **Tagging: `x-amz-tagging-count` header** — GET and HEAD object responses now include `x-amz-tagging-count` header when the object has tags
- **Tagging: multipart upload tags** — `CreateMultipartUpload` now captures `x-amz-tagging` header and applies tags to the final object at `CompleteMultipartUpload` time
- **Versioning: delete marker detection** — GET/HEAD on a key whose current version is a delete marker now returns 404 with `x-amz-delete-marker: true` and `x-amz-version-id` headers, instead of plain NoSuchKey
- **Versioning: CompleteMultipartUpload `x-amz-version-id`** — response now includes the version ID when the target bucket has versioning enabled
- **Versioning: UploadPartCopy with versioned source** — now uses the `?versionId=` from the copy source header to fetch the correct version, instead of always fetching the latest
- **Versioning: conditional DELETE with delete markers** — `If-Match`, `x-amz-if-match-last-modified-time`, and `x-amz-if-match-size` conditional headers on DELETE and batch DELETE now correctly evaluate against the latest version including delete markers
- **S3 compatibility: +18 Ceph s3-tests passing** — 338/829 (40.8%), up from 320/829 (38.6%)
- **Encryption: key mismatch returns 403 instead of 500** — downloading an object encrypted with a different master key now returns `403 AccessDenied` with a clear message instead of `500 InternalError`

## [0.12.0] — 2026-03-24

### Added

- **Phase 19: Object Tagging** — S3-compatible object and bucket tagging with 6 new operations: `GetBucketTagging`, `PutBucketTagging`, `DeleteBucketTagging`, `GetObjectTagging`, `PutObjectTagging`, `DeleteObjectTagging`. Inline tags on `PutObject` via `x-amz-tagging` header and `CopyObject` with `x-amz-tagging-directive`. Max 10 tags per object/bucket, key max 128 chars, value max 256 chars. Version-aware: tags tied to specific object versions. Cascade deletes on object/bucket removal. New `object_tags` and `bucket_tags` tables (migration v11)
- **Console: tag editor** — view, add, and remove object tags in the detail side panel
- **Roadmap restructured** — Phase 19 split into Object Tagging (19) and Lifecycle Rules (20), all subsequent phases renumbered (20-27 became 21-28)

## [0.11.0] — 2026-03-24

### Added

- **Console: search and filter** — real-time debounced search bar on all list views (Buckets, Users, Teams, Grants, Credentials, Bucket Detail). Client-side filtering with 300ms debounce, magnifying glass icon, clear button, and "no results" empty state. Bucket Detail search filters both files and folders, updates treemap and select-all in sync
- **Console: audit operation filter** — replaced text-search autocomplete with smart presets (S3 Read, S3 Write, All S3, All Admin, Data Changes) and category chips with drill-down. One-click presets for common scenarios, category-level toggling, and individual operation checkboxes. Active preset auto-detected from selection
- **Console: responsive mobile layout** — full mobile support down to 375px (iPhone SE). Collapsible sidebar with hamburger menu, responsive dashboard grid, touch-friendly controls (no hover required), horizontal-scrolling audit table, viewport-safe popovers, full-width side panels on mobile, icon-only bucket detail buttons, vertically stacked shuttles, and centered toast notifications

### Fixed

- Removed unused Rust imports (`delete` in router, `MetadataStore` and `admin_settings` in worker) to eliminate compiler warnings

## [0.10.0] — 2026-03-23

### Added

- **Console: object preview** — collapsible preview panel in the object detail sidebar with fullscreen modal (expand button). Supports images (JPEG, PNG, GIF, WebP, SVG, AVIF), video (MP4, WebM, MOV, MKV, OGV, AVI with native controls, 100 MB limit), text/code with syntax highlighting via highlight.js (~40 extensions), Markdown rendered via marked.js with dark theme, HTML in sandboxed iframe, and PDF via browser viewer. JSON auto pretty-printed. Size limits: 100 MB video, 10 MB images/PDF/HTML, 1 MB text
- **Environment variable overrides** — `ARCA_SERVER_BIND`, `ARCA_SERVER_PORT`, and `ARCA_STORAGE_DATA_DIR` environment variables can now override the corresponding config file values, useful for Docker/Kubernetes deployments

## [0.9.1] — 2026-03-22

### Added

- **Console: inline column header filters** — Time (date range popover), Bucket (value list with counts), Key (text search), User (value list with counts), Status (value list with color-coded codes), and Operation (tag-based include/exclude popover) filters are built into the table column headers
- **Console: clear audit log** — "Clear All" button with confirmation modal requiring explicit `CLEAR AUDIT LOG` text input. `DELETE /admin/audit` backend endpoint
- **Console: date range filter** — From/To datetime pickers for the Time column, server-side filtered
- **Console: status column filter** — filterable by HTTP status code via dropdown with checkboxes and counts
- **Console: sticky filters** — all audit log filters (column headers, operation tags, page size) persist across navigation via sessionStorage

### Changed

- **Console: audit log layout** — filter bar removed, all filters moved into column headers with consistent UX (funnel icon on hover, cyan badge when active, X to clear)
- **Console: fixed sidebar** — sidebar no longer scrolls with page content

### Fixed

- **Console: multi-file upload** — fixed regression where only the first file was uploaded (live FileList invalidated during async iteration)
- **Audit log feedback loop** — read-only monitoring operations (Health, Metrics, ListAudit, etc.) are no longer logged to prevent audit entries from generating more audit entries
- **Audit log missing identity** — access key and user ID now correctly extracted from the Authorization header before auth middleware consumes the request

## [0.9.0] — 2026-03-21

### Added

- **Phase 18: Monitoring, Metrics, and Audit**
- **Prometheus metrics endpoint**: `GET /admin/metrics` (unauthenticated) with request counters by operation/status, latency histograms (10 buckets), and gauges (active connections, buckets, objects, storage size)
- **Audit logging**: every S3 and admin operation recorded in `audit_log` table with timestamp, operation, identity, status, duration. `GET /admin/audit` with filters (bucket, operation, user, time range) and pagination. Tag-based operation filter with include/exclude modes in console
- **Instance-wide settings**: `GET/PUT/DELETE /admin/settings/{key}` for runtime-configurable settings (region, retention days). TOML config takes precedence when set (read-only in console)
- **Configurable region**: `region` in `[server]` TOML section or via Admin API. Per-bucket region via `bucket_config`. Resolves TD-004
- **Metrics history**: periodic gauge snapshots stored in `metrics_snapshot` table. `GET /admin/metrics/history` with time range filter. SVG sparkline charts with labeled axes
- **Retention management**: configurable audit log and metrics retention (days). Background worker purges old records hourly
- **Background worker framework**: reusable `BackgroundWorker::spawn_periodic` for periodic tasks (metrics snapshots, retention purge)
- **Console: Settings page** — instance-wide settings with lock indicators for TOML-set values, auto-save for editable fields
- **Console: Audit Log page** — filterable table with tag-based operation filter, pagination with first/last buttons, page size selector, slide-in detail panel, auto-refresh
- **Console: Monitoring page** — SVG sparkline charts for objects, storage, buckets, connections with time range selector and labeled axes
- **Console: fixed sidebar** — sidebar no longer scrolls with content, stays pinned to viewport
- **Database migration v10**: `server_config`, `audit_log`, and `metrics_snapshot` tables
- **Integration tests**: 27 new tests for Prometheus, audit, metrics history, settings, and region

### Fixed

- **Multi-file upload**: fixed regression where uploading multiple files or folders in the console only uploaded the first file (FileList reference invalidated by input reset during async iteration)

## [0.8.1] — 2026-03-21

### Fixed

- **DeleteObjects with VersionId**: batch delete (`POST /{bucket}?delete`) now supports `<VersionId>` per object, enabling hard-deletion of specific versions and delete markers. Previously, VersionId was ignored and versioned deletes only created more delete markers, making buckets impossible to empty.
- **Console**: object list now refreshes after deleting a specific version from the version history panel.
- **Ceph s3-tests**: 50 additional tests now pass (270 → 320), primarily in Versioning and Bucket categories.

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

[Unreleased]: https://github.com/dxc-technology/arca/compare/v0.12.0...HEAD
[Unreleased]: https://github.com/dxc-technology/arca/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/dxc-technology/arca/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/dxc-technology/arca/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/dxc-technology/arca/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/dxc-technology/arca/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/dxc-technology/arca/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/dxc-technology/arca/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/dxc-technology/arca/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/dxc-technology/arca/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/dxc-technology/arca/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/dxc-technology/arca/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/dxc-technology/arca/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/dxc-technology/arca/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/dxc-technology/arca/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/dxc-technology/arca/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/dxc-technology/arca/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dxc-technology/arca/commits/v0.1.0
