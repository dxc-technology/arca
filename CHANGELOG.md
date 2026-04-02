# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **(Console)**: Settings page reorganized: "Monitoring" section renamed to "Data Retention" with all retention policies grouped together (Audit Log, Notification Events, Metrics Snapshots), ordered by sidebar position
- **Notification event retention** is now a server setting (`notification_retention_days`) manageable from the console, following the same TOML > DB > default precedence as audit and metrics retention (default: 7 days)

## [0.17.0] — 2026-04-02

### Added

- **Phase 25: Notifications and Event System**
- **`PutBucketNotificationConfiguration`** / **`GetBucketNotificationConfiguration`**: S3-compatible bucket notification configuration via `?notification` query parameter. Accepts all three S3 destination types (`TopicConfiguration`, `QueueConfiguration`, `CloudFunctionConfiguration`), treating all as webhook destinations
- **Event emission**: `s3:ObjectCreated:Put`, `s3:ObjectCreated:Copy`, `s3:ObjectCreated:CompleteMultipartUpload`, `s3:ObjectRemoved:Delete`, `s3:ObjectRemoved:DeleteMarkerCreated` events emitted from handlers via non-blocking mpsc channel
- **Webhook delivery worker**: background worker consumes events, matches per-bucket notification rules (event type + prefix/suffix filters), delivers HTTP POST to configured webhook URLs with exponential-backoff retry (configurable max retries, base delay, timeout)
- **Notification event persistence**: `notification_events` table (SQLite migration v14, PostgreSQL schema update) stores delivery records with status tracking (`pending`/`delivered`/`failed`)
- **Per-webhook Enabled/Disabled toggle**: Arca extension persisted via `<Enabled>` XML element, allowing temporary webhook suspension without removal
- **`NotificationStore` trait**: CRUD + purge for notification event records, implemented for both SQLite and PostgreSQL backends
- **Admin API**: `GET /admin/notifications/events` (list with filters), `GET /admin/notifications/events/count`, `DELETE /admin/notifications/events` (clear with confirmation), `POST /admin/notifications/test-webhook` (connectivity test)
- **`[notifications]` config section** (optional): tuning parameters for channel size, retry behavior, webhook timeout, and event retention days (default: 7)
- **Event retention**: notification events automatically purged by the retention worker based on `event_retention_days`
- **Notification config caching**: in-memory cache with 60s TTL in the delivery worker to avoid per-event DB reads
- **(Console)**: Notification event log viewer with audit-style inline column header filters (Time, Event, Bucket, Key, Destination, Status), pagination, side panel detail, clear-all with confirmation
- **(Console)**: Per-bucket notification rules editor in bucket settings (lifecycle-style inline forms, Enabled/Disabled toggle, prefix/suffix filters, test webhook button)
- **(Console)**: Sidebar navigation link for Notifications (between Audit Log and Monitoring)
- Docker webhook receiver service (`docker/webhook-receiver/`) for integration testing
- `bin/test notifications` mode with webhook receiver compose overlay
- 35 unit tests (XML parsing/roundtrip, event matching, filter matching, validation, NotificationStore CRUD)
- ~18 integration tests (configuration CRUD, webhook delivery, event format, filters, batch delete, admin API)

## [0.16.1] — 2026-04-01

### Fixed

- **PostgreSQL backend not activating**: when using `--postgres` with other features (e.g. `--encryption`), the `metadata_backend` key from the postgres config fragment landed under the wrong TOML section due to naive fragment concatenation, silently falling back to SQLite
- Auto-detect `metadata_backend = "postgres"` from presence of `[storage.postgres]` config section, removing the need for an explicit key in the fragment

## [0.16.0] — 2026-03-31

### Added

- **Configurable preview size limits**: 3 new server-side settings (`preview_max_size_mb`, `preview_max_text_mb`, `preview_max_video_mb`) to control the maximum file size for object previews in the console
- **(Console)**: New "Console" section in the Settings page to manage preview thresholds per file type (images/PDF/HTML, text/Markdown, video), with 0 = unlimited
- **(Console)**: Preview logic reads limits from server settings instead of using hardcoded values
- 2 new integration tests for preview settings CRUD and validation

## [0.15.0] — 2026-03-31

### Added

- **Phase 24: PostgreSQL Backend**
- **`PgStore`**: PostgreSQL metadata backend implementing all 8 store traits (MetadataStore, CredentialStore, UserStore, TeamStore, GrantStore, AuditStore, MetricsStore, ServerConfigStore) via `sqlx-core`/`sqlx-postgres`
- **Config switch**: `[storage] metadata_backend = "sqlite" | "postgres"` with `[storage.postgres]` section for connection string and pool settings
- **Docker overlay**: `docker-compose.postgres.yml` with PostgreSQL 17 Alpine, `--postgres` flag for `bin/arca start` and `bin/test postgres`
- **Migration runner**: consolidated initial schema (equivalent to SQLite v1-v13) applied automatically at startup
- **`/admin/info`**: returns `metadata_backend` field ("sqlite" or "postgres")
- **(Console)**: Database indicator in server info panel showing current backend type
- 20 PostgreSQL-specific integration tests covering buckets, objects, multipart, versioning, tags, lifecycle, copy, range reads

## [0.14.0] — 2026-03-31

### Added

- **Phase 22: S3 API Completeness**
- **`ListParts`**: `GET /{bucket}/{key}?uploadId=X` with pagination
- **`GetObjectAttributes`**: `GET /{bucket}/{key}?attributes` with ETag, Checksum, ObjectParts, StorageClass, ObjectSize
- **Checksum algorithms**: store and return client-provided `x-amz-checksum-sha256/crc32/crc32c/crc64nvme` on PutObject, GetObject, HeadObject
- **Storage classes**: `storage_class` field on ObjectRecord, accept `x-amz-storage-class` header
- **SQLite migration v13**: `storage_class`, `checksum_algorithm`, `checksum_value` on objects; `checksum_value`, `last_modified` on parts; `checksum_algorithm` on multipart_uploads
- Resolves TD-002 (storage class) and TD-008 (content-type source)
- **Phase 23: Performance and Hardening**
- **Request size limits**: `[server.limits]` TOML section, streaming `LimitedByteStream`, Content-Length fast-reject, `EntityTooLarge` error (default 5 GB)
- **Rate limiting**: per-IP and per-credential GCRA via `governor` crate, `SlowDown` 503 with `Retry-After` (disabled by default)
- **Metadata cache**: in-memory LRU via `moka` crate, `CachingMetadataStore` for bucket existence and object HEAD, configurable size/TTL in `[server.cache]`, write-through invalidation
- **Graceful shutdown**: drain mode via `tokio::sync::watch`, health endpoint returns 503 during configurable drain window
- **Request validation middleware**: header count limit, null byte rejection, user metadata size limit
- **Performance benchmarks**: HEAD/DELETE benchmarks with `--json` output and `--baseline` comparison

## [0.13.0] — 2026-03-26

### Added

- **Phase 21: Object Lock (WORM Compliance)**
- **6 new S3 operations**: `PutObjectLockConfiguration`, `GetObjectLockConfiguration`, `PutObjectRetention`, `GetObjectRetention`, `PutObjectLegalHold`, `GetObjectLegalHold`
- **Retention modes**: GOVERNANCE (bypassable with permission) and COMPLIANCE (absolute protection) with retain-until-date
- **Legal hold**: per-object ON/OFF flag, independent of retention
- **Default retention**: bucket-level config applied automatically on PutObject
- **Enforcement**: blocks hard-deletion of locked versions, delete markers always allowed, auto-enables versioning (prevents suspension)
- **SQLite migration v12**: adds `retention_mode`, `retain_until_date`, `legal_hold_status` columns
- 7 new S3 policy actions including `s3:BypassGovernanceRetention`
- Lifecycle worker respects Object Lock
- **Console: Object Lock** — lock badge in bucket list and breadcrumbs, retention mode/period in bucket settings, versioning suspend disabled when locked
- **Phase 20: Lifecycle Rules**
- **3 new S3 operations**: `PutBucketLifecycleConfiguration`, `GetBucketLifecycleConfiguration`, `DeleteBucketLifecycleConfiguration`
- **Rule features**: object expiration after N days, noncurrent version expiration, abort incomplete multipart uploads
- **Filters**: by prefix, tag, or combined (And filter). Rules stored as JSON in `bucket_config` table (no schema migration)
- **Background worker**: periodic evaluation across all buckets, configurable interval (default 1 hour), batch processing (100 objects per rule per cycle), audit logging for all lifecycle actions
- **Console: lifecycle rules editor** — add/remove rules with prefix filter, expiration days, noncurrent version days, abort upload days
- **Admin setting: `lifecycle_evaluation_interval`** — configurable evaluation interval (60–86400 seconds) via `GET/PUT/DELETE /admin/settings/lifecycle_evaluation_interval`

### Fixed

- **Tagging: tag validation on PutObject** — invalid `x-amz-tagging` headers now rejected with 400 before writing the blob
- **Tagging: `x-amz-tagging-count` header** — GET and HEAD responses now include tag count when the object has tags
- **Tagging: multipart upload tags** — `CreateMultipartUpload` now captures `x-amz-tagging` and applies tags at `CompleteMultipartUpload`
- **Versioning: delete marker detection** — GET/HEAD on a delete marker now returns 404 with `x-amz-delete-marker: true` and `x-amz-version-id`
- **Versioning: CompleteMultipartUpload `x-amz-version-id`** — response now includes version ID when versioning is enabled
- **Versioning: UploadPartCopy with versioned source** — now uses `?versionId=` from copy source header instead of always fetching latest
- **Versioning: conditional DELETE with delete markers** — conditional headers on DELETE now correctly evaluate against latest version including delete markers
- **S3 compatibility**: +18 Ceph s3-tests passing, 338/829 (40.8%), up from 320/829 (38.6%)
- **Encryption: key mismatch returns 403** — wrong master key now returns `AccessDenied` instead of `InternalError`

## [0.12.0] — 2026-03-24

### Added

- **Phase 19: Object Tagging**
- **6 new S3 operations**: `GetBucketTagging`, `PutBucketTagging`, `DeleteBucketTagging`, `GetObjectTagging`, `PutObjectTagging`, `DeleteObjectTagging`
- **Inline tags**: `x-amz-tagging` header on PutObject, `x-amz-tagging-directive` on CopyObject
- **Limits**: max 10 tags per object/bucket, key max 128 chars, value max 256 chars
- **Version-aware**: tags tied to specific object versions, cascade deletes on object/bucket removal
- **SQLite migration v11**: new `object_tags` and `bucket_tags` tables
- **Console: tag editor** — view, add, and remove object tags in the detail side panel
- **Roadmap restructured** — Phase 19 split into Object Tagging (19) and Lifecycle Rules (20), subsequent phases renumbered

## [0.11.0] — 2026-03-24

### Added

- **Console: search and filter** — real-time debounced search bar on all list views (Buckets, Users, Teams, Grants, Credentials, Bucket Detail), filters files and folders, updates treemap and select-all in sync
- **Console: audit operation filter** — smart presets (S3 Read, S3 Write, All S3, All Admin, Data Changes) with category chips, drill-down, and individual operation checkboxes
- **Console: responsive mobile layout** — full support down to 375px, collapsible sidebar, responsive grid, touch-friendly controls, horizontal-scrolling tables, viewport-safe popovers

### Fixed

- Removed unused Rust imports (`delete` in router, `MetadataStore` and `admin_settings` in worker) to eliminate compiler warnings

## [0.10.0] — 2026-03-23

### Added

- **Console: object preview** — collapsible preview panel with fullscreen modal. Supports images (JPEG, PNG, GIF, WebP, SVG, AVIF), video (MP4, WebM, MOV with native controls), text/code with syntax highlighting, Markdown, HTML in sandboxed iframe, and PDF
- **Environment variable overrides** — `ARCA_SERVER_BIND`, `ARCA_SERVER_PORT`, `ARCA_STORAGE_DATA_DIR` override config file values

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

[Unreleased]: https://github.com/dxc-technology/arca/compare/v0.17.0...HEAD
[0.17.0]: https://github.com/dxc-technology/arca/compare/v0.16.1...v0.17.0
[0.16.1]: https://github.com/dxc-technology/arca/compare/v0.16.0...v0.16.1
[0.16.0]: https://github.com/dxc-technology/arca/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/dxc-technology/arca/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/dxc-technology/arca/compare/v0.13.0...v0.14.0
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
