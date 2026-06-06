# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Performance: per-bucket replication-config cache.** Object writes (PutObject, CompleteMultipartUpload, delete markers, PutObjectTagging) no longer read the bucket's `replication_configuration` from the metadata store on every request. The result is cached per bucket for 30 seconds — mirroring the existing per-bucket encryption cache — so buckets without replication (the common case) skip the database query entirely on the write hot path. The cache is invalidated immediately on `PutBucketReplication`, `DeleteBucketReplication`, and the admin credential-cascade disable, so a configuration change still takes effect at once.

## [0.25.0] — 2026-06-03

### Added

- **High-availability clustering (Phase 29).** Arca can now run as a symmetric, self-configuring, fully-replicated multi-node cluster — every node is identical, holds the full dataset, and serves reads and writes. Enable it with a single `[cluster]` config section (or `bin/arca --cluster` for local dev); there is no special "primary" and no separate coordinator.
  - **Real-time replication** of the whole data and control plane: objects (including multipart uploads, object tags, retention and legal hold), buckets and bucket config, and identity/authorization (credentials, users, teams, grants) and server settings. A write to any node fans out to its peers, so a client behind a load balancer can read its write from any node.
  - **Two consistency modes.** `quorum` (default, CP): a write needs a majority of nodes; if a node loses the majority it goes read-only and rejects writes with `503 ServiceUnavailable` rather than risk divergence. `available` (AP): always writable, best-effort replication. Last-writer-wins conflict resolution with a deterministic `blob_id` tiebreak.
  - **Self-healing.** A returning or lagging node catches up automatically: an anti-entropy worker reconciles objects (changed-since manifests) and the control plane (full-snapshot merge), proactively repairs blobs whose bytes a node is missing, and garbage-collects orphaned blobs (composite-multipart-aware). Tombstones ensure a delete is never resurrected by a peer that missed it.
  - **Peer discovery** via mDNS (LAN), static seed list, or DNS (e.g. a Kubernetes headless Service).
  - **Cluster-aware storage capacity.** Because every node holds the full dataset, the cluster's capacity is bounded by its smallest node; the dashboard reports the cluster-minimum free space and a write larger than that is rejected with `507 InsufficientStorage`, even if the node serving it has room.
  - **Config-drift detection.** A node whose cluster-critical config (cluster id / secret / mode / size / master key) differs from its peers is flagged on `/admin/cluster` and in the console, without the cluster refusing to run.
- **`GET /admin/cluster`** — cluster topology, consistency mode, write-quorum status, per-node liveness and config alignment, and cluster-effective disk capacity (`{"enabled": false}` on a single node).
- **`GET /admin/health?verbose=1`** — health plus the cluster snapshot for operators; the default (non-verbose) health shape consumed by load balancers is unchanged.
- **(Console)** Cluster topology on the Dashboard: a dedicated topology card showing the consistency mode, write status, and a live node list (online/offline, "this node", endpoints, last-seen, config-mismatch flags). The card is now shown on single-node deployments too, where it presents the single local node — the dashboard layout is identical whether or not clustering is enabled.
- **Tooling and deployment.** `bin/cluster` manages a local 3-node cluster (compose + HAProxy); `deploy/kubernetes/arca-cluster.yaml` (StatefulSet + headless-Service DNS discovery) and `deploy/haproxy/` (HAProxy + keepalived floating VIP) provide production templates; a [High Availability guide](https://dxc-technology.github.io/arca/guide/ha/) documents the design, trade-offs, and operations.

## [0.24.0] — 2026-05-21

### Added

- **(Console)** Keyboard navigation across the file list, turning the side panel and the fullscreen preview into a slideshow. <kbd>↑</kbd> / <kbd>↓</kbd> walk the list and reload the inline preview in place if it is open; <kbd>←</kbd> / <kbd>→</kbd> step through the fullscreen modal. The fullscreen header gains a `position / total` counter and ‹ › arrows for mouse users, and the next two images are prefetched so navigation feels instant. Editable fields (search, tags) keep their native arrow behaviour.
- **(Console)** The sidebar identity block now shows the authenticated user's **username** next to the Admin / User badge, with the access key on a dedicated muted line underneath instead of a truncated access key. Works for non-admin users too.
- **`GET /admin/me` is now identity-only**: any valid Arca credential can read its own username, with no `arca:*` grant required. Previously the username probe needed `arca:ViewServerInfo`, which forced admins to attach `AdministratorAccess` to every user just to populate the sidebar — and that side-effect also exposed the empty Users / Teams / Grants / Audit Log pages where every call inside was a 403. The grant gate on the rest of the admin API is unchanged.
- **(Console)** "Open in new tab" button in the fullscreen preview header. Useful when the in-iframe renderer is weak — most notably macOS Safari, which displays PDFs as a tiny thumbnail strip in iframes but renders them correctly at top level (where macOS users can also hand off to Preview.app).

### Changed

- **(Console)** Server card on the Dashboard now sits in a stable 3×2 grid with a new **Topology** tile (currently "Single node", will reflect multi-node in Phase 29). Longer labels like `Per-bucket (Vault)` no longer wrap.
- **(Console)** The drag-and-drop zone in the bucket browser now stretches to the bottom of the file area instead of hugging the first few rows. Anywhere under the file list is droppable; the sidebar is not.

### Fixed

- **(Console)** The **Effective Grants** tab on the user-detail page now refreshes automatically after attaching / detaching a direct grant or joining / leaving a team. Previously the tab kept showing the stale state until the user navigated away and back.

## [0.23.1] — 2026-05-07

### Fixed

- **Plain `PutObject` and `UploadPart` no longer block tokio workers on MD5.** The previous pipeline interleaved hashing and disk writes on the same async task, so the CPU-bound MD5 work serialized the runtime that was supposed to be pulling network bytes — leaving plain uploads slower than encrypted ones after v0.23.0 moved AEAD off the runtime (the symptom that triggered this release). MD5 and write now run on a dedicated blocking worker fed by a bounded channel.

  Single-stream (`1 × 1 GiB`, tmpfs): plain steady at ~266 MB/s; encrypted **188 → 235 MB/s (+25%)**. Concurrent (`4 × 256 MiB`): plain ~833 → ~862 MB/s (+3.5%), encrypted **594 → 706 MB/s (+19%)** — the largest win where the runtime was most contended (encrypted, low parallelism), exactly the PBM-backup workload.
- **(Console)** Admin error toasts now show the backend's descriptive `message` (e.g. `Username "root" already exists`) instead of the bare HTTP category (e.g. `Conflict`). The S3 endpoints that were still surfacing `Error ${status}` were converted to use the existing `<Message>` extractor at the same time.

### Added

- **(Console)** Soft duplicate-description warning on Create / inline-edit forms for users, teams, grants and credentials. Advisory only — descriptions remain free-form, the save path is unchanged.

## [0.23.0] — 2026-05-06

### Fixed

- **Encrypted multipart uploads no longer pay the 2× decrypt+re-encrypt tax on `CompleteMultipartUpload`.** The default `concat` implementation used to decrypt every part and re-encrypt the whole final blob, so a Percona PBM MongoDB backup that took 1h plain took 2h with SSE-S3. The encrypted store now produces a **composite sidecar** that points at the still-on-disk encrypted parts (no copy, no extra crypto). Reads only decrypt the chunks that overlap the requested range; deletes cascade to the parts. Sidecar schema is additive, so old sidecars still deserialize.
- **Plain multipart uploads also benefit from the composite sidecar.** Same trick when every part is plain (no encryption, no compression), eliminating the byte-by-byte copy that previously dominated `CompleteMultipartUpload`. Encrypted-or-compressed parts still take the existing copy + MD5 path.
- **HTTP/2 keep-alive no longer panics.** `keep_alive_interval` on the hyper auto-builder requires a Timer; the TLS server now installs one explicitly so internal pings don't abort the runtime.

### Bench (4 concurrent multipart uploads, 256 MiB each, 8 MiB parts, tmpfs)

| Scenario              | Throughput  | `CompleteMultipartUpload` p50 |
|-----------------------|-------------|-------------------------------|
| Plain baseline        | 527.8 MB/s  | 0.71s                         |
| Encrypted baseline    | 207.7 MB/s  | 3.03s                         |
| Plain after fix       | 836.9 MB/s  | 0.003s (≈237× faster)         |
| Encrypted after fix   | 594.9 MB/s  | 0.004s (≈758× faster)         |

### Changed

- **AEAD chunk crypto runs off the tokio async runtime.** The encrypting stream now batches 4 × 64 KiB chunks and hands each batch to a `spawn_blocking` worker; up to 4 batches fly per stream so network reads, disk writes and CPU-bound AEAD overlap instead of serializing on the same worker thread. Tag and ciphertext are produced in place — one allocation per batch instead of two per chunk.
- **Tokio runtime is now built explicitly** instead of via the `#[tokio::main]` macro. New `[server.runtime]` config section: `worker_threads`, `max_blocking_threads` (both default to `num_cpus`).
- **HTTP/2 server tunables exposed via `[server.http]`**: max concurrent streams, keep-alive interval/timeout, initial stream/connection window. The TLS auto-builder is now constructed once and shared across connection tasks.
- **Process-wide `SystemRandom`** for DEK / nonce generation (previously a fresh instance per call).

### Docker

- `docker-compose.yml` now sets `nofile=65535` and `net.core.somaxconn=4096` on the `arca` service so connection fan-out (multipart, parallel PBM workers) doesn't hit a hidden FD ceiling. New `docker-compose.perf.yml` overlay (tmpfs `/data` for reproducible perf measurements) is layered on automatically by `bin/perf-test --tls` / `--encryption`.

### Benchmarks

- `perf_test.py` gained a `parallel-multipart` scenario that issues N concurrent multipart uploads and reports `CompleteMultipartUpload` time separately. Used to detect the encrypted-concat regression and verify the fix.

### Added

- **(Console)** New `2d` timeframe button in the Monitoring view, between `24h` and `7d`.
- **(Console)** Replication credentials are now created and managed from a dedicated modal flow with inline location hints, instead of the old free-form text fallback.

### Documentation

- **(Console manual)** New screenshots covering the replication setup and journal flow.
- **(Project report)** Refreshed with v0.23.0 metrics: ~23 full-time-equivalent days, 285 commits, 72,651 LoC, 2,152 tests, 28/31 phases done.

### Tech debt

- **TD-014** (recorded, not yet fixed): `arca recover` and `arca fsck` do not understand composite blobs and currently flag them as "orphaned sidecar". Runtime S3 reads/writes are unaffected. Proper fix sketched in `TECH_DEBT.md`.

### Console (other)

- Anchor monitoring chart edge x-ticks so the leftmost / rightmost labels can no longer clip out of the SVG viewport.

## [0.22.0] — 2026-04-22

### Added

- **Phase 28 — Replication** (P3). Asynchronous cross-instance replication for disaster recovery and geographic distribution. One-way per-rule with explicit loop prevention, so two-way mirrors converge instead of bouncing forever. Destination is any S3-compatible endpoint (Arca, AWS S3, MinIO, …) and outbound requests are signed with full AWS SigV4. Versioning is required on the source bucket (AWS CRR semantics).
- **Change journal** records every pending / in-flight / failed delivery in a `replication_journal` table (SQLite v17 / Postgres `0004`). Per-event-type dispatch (PUT / delete-marker / tag), last-writer-wins conflict resolution against the destination's `Last-Modified`, exponential backoff capped at one hour, and a `FAILED` state stamped only after the terminal attempt so transient failures don't leak to clients.
- **Loop prevention**: every outbound request carries `x-amz-arca-replication-source: <source_endpoint_id>`. Receiving Arca tags the object `REPLICA` and skips journal emission. Mirror configurations are integration-tested end-to-end: a single PUT on the source reaches the mirror exactly once and never bounces back.
- **`x-amz-replication-status` response header** on the relevant object handlers, with values `PENDING` / `COMPLETED` / `FAILED` / `REPLICA`.
- **Admin API** for managing the journal (list with filters, retry an entry) and destination credentials (list, create, delete, query usage). Deleting a credential cascade-disables every rule that references it so the worker can't loop forever against a vanished reference. `/admin/info` gained a `replication_enabled` flag.
- **`PutBucketReplication` / `GetBucketReplication` / `DeleteBucketReplication`** S3 handlers with Arca-flavoured XML extensions (`<Endpoint>`, `<CredentialRef>`, `<Region>` alongside the standard `<Bucket>`).
- **`[replication]` config section** with poll interval, batch size, retry budget, request timeout, source endpoint id, and journal retention windows (30 days for completed rows, 90 day hard cap so the table stays bounded during an extended outage).
- **Docker replication test mode**: `bin/arca start --replication` boots a second `arca-replica` instance on `:9001`; `bin/test replication` runs the full two-instance integration suite (4 boto3 tests: PUT, delete-marker, tag, mirror no-loop).
- **(Console)** Per-bucket **Replication** card in Bucket Settings with an amber "Versioning required" banner that links to the versioning card, rule rows showing `source ➜ destination` with inline Enabled/Disabled toggles and tag-filter chips, and a modal editor (Prefix + Tag filter, Destination, Credential Ref dropdown with an inline "+ New" flow).
- **(Console)** New admin view at `#/replication` — **Destination Credentials** card above the **Replication Journal**. The journal has inline column-header filters, color-coded status chips with a pulse dot on `in_flight`, a side detail panel, pagination, auto-refresh every 30 s, and per-row / per-panel "Retry delivery now" CTAs on failed rows.
- **(Docs)** New `guide/replication.md` covering setup, the loop-prevention contract, two-way mirrors, destination-credential management, journal and retry workflow.

### Changed

- `ObjectRecord` gained a `replication_status` field; wire format and existing migrations are unchanged.

## [0.21.0] — 2026-04-20

### Added

- **Phase 27 — Transparent at-rest compression** (P2). `CompressingBlobStore` wraps `FsBlobStore`/`EncryptingBlobStore` to compress plaintext before encryption and disk write, while keeping the wire format unchanged (`ETag` is still MD5 of plaintext; `Content-Length` reports plaintext size). Six algorithms ship: `zstd`, `lz4`, `snappy`, `gzip`, `brotli`, `xz`. An `auto` mode picks per-object via a deterministic rule table based on `Content-Type` and size (no sampling or ML). Compression is **per-bucket, console-managed** — the wrapper is always installed and activates only when a bucket has compression configured via the console or the Arca-specific `PUT/GET/DELETE /{bucket}?compression` subresource. Baked-in MIME and size filters skip already-compressed content; skipped reasons are surfaced as Prometheus counters. Chunked frame format with a footer chunk index enables O(1) ranged reads. Mixed-mode: compressed and plain blobs coexist transparently via sidecar metadata.
- **`arca compress-existing` / `arca decompress-existing` CLI**: offline atomic retrofit of an existing data directory (resumable via sidecars, `--dry-run`, `--bucket`, `--algorithm` overrides). `arca fsck` already recognizes `.compressing.tmp` orphans.
- **Prometheus series**: `arca_compression_plaintext_bytes_total{algorithm}`, `arca_compression_compressed_bytes_total{algorithm}`, `arca_compression_skipped_total{reason}`, `arca_storage_compression_ratio`.
- **(Console)** Per-bucket Compression card in bucket settings — matches the Object Lock card pattern: inline algorithm dropdown + level input + single Enable/Disable button, with an "Enabled" status badge and current algorithm/level displayed when active.
- **(Console)** Inline help system: every setting carries a **"?"** trigger next to its label. Hovering shows a short one-line hint tooltip; clicking opens a modal with the full explanation, bullets, rows, notes and code samples. Backed by a single Alpine component (`helpTrigger(topicId)`) and a global modal listening for `arca:open-help` custom events. All 13 starter topics live in `console/js/help.js`; adding help to a new setting is one topic entry plus a six-line snippet next to the label.
- **(Console)** Help coverage pass: bucket-settings cards (encryption, compression, versioning, object-lock, lifecycle, notifications), Settings page (log level, retention windows, S3 region, preview size limits) and the dashboard bucket-size widget (now also shows compression and object-lock badges, previously missing).
- **(Console)** Create-bucket dialog gained an **Enable Object Lock** checkbox. When ticked, the `PUT /{bucket}` request sends `x-amz-bucket-object-lock-enabled: true` so Object Lock and versioning are enabled atomically with the bucket. The API client's `request` helper now forwards caller-supplied headers through SigV4 signing.
- **(Console)** Irreversible bucket-setting toggles (first-time Versioning enable, Object Lock enable) now require typed bucket-name confirmation in a modal before firing. Prevents accidental clicks in production (versioning cannot be disabled, only suspended; Object Lock cannot be disabled at all).
- **(Docs)** New `guide/compression.md`, new Compression note in `guide/configuration.md`, new CLI entries for `compress-existing`/`decompress-existing`. Console user guide updated for the new Object Lock enablement flow, confirmation modals and card intros.

### Changed

- **Object Lock can now be enabled on empty existing buckets**, not only at bucket creation. `PutObjectLockConfiguration` used to reject every call on a bucket whose `object_lock` config was absent with `InvalidBucketState` (strict S3 semantics). The handler now accepts the request when the bucket still has no objects and auto-enables versioning as before; buckets that already contain objects are still rejected, with a clearer message ("can only be enabled on empty buckets or at bucket creation"). Three new integration tests cover the accepted-empty / rejected-non-empty / accepted-at-creation matrix.
- **(Console)** Buckets list cards: creation date moved to its own line above the capability badges, and the badge row now wraps so four or more indicators (encrypted + versioned + locked + compressed) no longer overflow the card.
- **(Console)** Bucket settings page normalized: every card (Encryption, Compression, Versioning, Object Lock, Lifecycle, Event Notifications) now opens with a short plain-English intro paragraph explaining what the setting does and whether it can be turned off. Versioning and Object Lock intros carry the amber "This action cannot be undone." red-thread, matching the Object Lock card that set the pattern.
- **(Console)** Compression and Object Lock inline controls (dropdowns, inputs, buttons) harmonized with the global Settings page baseline (`text-sm px-3 py-1.5`); no more size discrepancy between pages.

## [0.20.0] — 2026-04-18

### Added

- **Kafka notification connector**: produce S3 events to Kafka topics via `rdkafka` (librdkafka). Supports SASL authentication, configurable security protocol, and custom topics
- **AMQP notification connector**: publish S3 events to RabbitMQ via `lapin` (pure Rust AMQP 0-9-1). Supports custom exchanges, routing keys, durable queues, and publisher confirms
- **Elasticsearch notification connector**: index S3 events as documents via REST API (`reqwest`). Supports custom indices and basic auth. Zero new dependencies
- **Syslog (RFC 5424) notification connector**: send S3 events as syslog messages over UDP or TCP. Configurable facility, severity, and app name. Zero new dependencies
- **SMTP notification connector**: deliver S3 events as e-mail via `lettre` over SMTP or SMTPS. Supports STARTTLS, PLAIN authentication, custom subject/sender, and configurable timeout (`smtp_timeout_seconds`, default 15 s). Body is the same S3 event JSON the webhook connector posts
- **gRPC notification connector**: deliver S3 events as a unary `arca.notifications.v1.NotificationService/Notify` RPC via `tonic` + `prost`. Supports h2c and TLS, Bearer token via gRPC metadata, custom CA certificates for self-signed servers, and arbitrary user metadata forwarded through the proto map. Configurable timeout (`grpc_timeout_seconds`, default 10 s)
- **Connector configuration guide** (`guide/connectors.md`): single-page reference for all 13 connectors with destination URL formats, properties, XML examples, and timeout tuning
- **(Console)** Connector-specific form fields for Kafka, AMQP, Elasticsearch, Syslog, SMTP, and gRPC (all now active in the connector picker)
- **(Docs)** Rewrote the Event Notifications section of the console user guide with dedicated Webhook / SMTP / gRPC form examples and three new screenshots

### Fixed

- **Kafka/AMQP connector integration tests**: resolved TD-013. The AMQP tests used to fail because `wait_for_amqp_receiver` ran `docker exec rabbitmq-diagnostics` as root in a tight loop, racing with RabbitMQ's `.erlang.cookie` initialization and crashing the container (EACCES). Switched the AMQP and Kafka readiness helpers to `docker inspect`-based health polling so they never exec into the receiver. Also fixed the Kafka test subscriber (manual partition assignment + `seek_to_end` avoids consumer-group coordination races) and raised the SigV4 admin-API default timeout so the `test-connector` failure path for Kafka's unreachable host does not time out the HTTP client before Arca can respond

### Removed

- **AMQP DNS pre-resolution workaround**: the `resolve_uri` helper that rewrote AMQP URIs to use a tokio-resolved IP was added under the false TD-013 diagnosis. With the real fix in place, `lapin`/`tcp-stream` resolves hostnames correctly from the Alpine/musl `arca` binary, so the workaround is gone and the connector passes the URI straight to `Connection::connect`
- **ONVIF connector**: dropped from Phase 26 before any implementation shipped. The `ConnectorType::Onvif` enum variant, the console picker entry, and the roadmap checklist item have been removed. ONVIF integration was judged too niche to justify its maintenance cost; an external adapter can bridge ONVIF events into Arca via the webhook or gRPC connector when needed
- **"Coming soon" paths in the console**: since every connector is now implemented, removed the dead `active` flag on `CONNECTOR_TYPES`, the `soon` label + disabled-tile styling in the picker, and the conditional templates that were only reachable for unimplemented connectors

## [0.19.0] — 2026-04-15

### Added

- **Configuration export API**: `GET /admin/export` returns full instance configuration as JSON (settings, users, teams, grants, credentials, buckets, bucket configs). Supports `sections` query parameter for selective export and `include_secrets` for secret key visibility (masked by default)
- **Configuration import API**: `POST /admin/import` applies exported JSON to a running instance. Three conflict modes: `skip` (default), `overwrite`, and `dry_run`. Processes sections in dependency order, automatically skips masked credentials
- **(Console)** Export/Import buttons in Settings page with polished modals: section checkboxes with aligned labels, secrets toggle with warning, drag-and-drop file upload, conflict mode selection, per-section result display
- **Configurable log level with runtime reload**: log level can be changed via `PUT /admin/settings/log_level` and takes effect immediately without restart
- **Git commit hash at startup**: compile-time embedded git hash logged on server start for traceability
- **SQLite read connection pool**: separate read-only connection pool for concurrent query execution
- **`BlobStore::concat`**: efficient multipart assembly without intermediate copies
- **Batched audit writes**: dedicated writer task with channel-based batching to reduce per-request overhead
- **Admin & Server Enhancements section** in roadmap for tracking features outside numbered phases

### Changed

- Request logging moved from INFO to DEBUG level to reduce noise

### Fixed

- Unused imports in multipart.rs and validate.rs tests
- Timeline divider label in project report (changed from "MVP" to "NOW")

## [0.18.2] — 2026-04-11

### Changed

- **Performance: release profile optimization** — enable `opt-level = 3`, thin LTO, `codegen-units = 1`, and symbol stripping for significantly faster crypto, hashing, and XML parsing
- **Performance: TLS session resumption** — enable server-side session cache (256 entries) to avoid full handshakes on client reconnections
- **Performance: TCP_NODELAY on TLS connections** — reduce latency on small responses (HEAD, DELETE, errors)
- **Performance: SQLite tuning** — set `synchronous=NORMAL` (safe with WAL), 64 MB page cache, 256 MB mmap, `temp_store=MEMORY`, 5 s busy timeout
- **Performance: 64 KB ReaderStream chunks** — increase from 8 KB default, reducing syscalls by ~8x on large object downloads
- **Performance: eliminate double allocation in decryption** — reuse the in-place decryption buffer instead of copying plaintext
- **Performance: compact sidecar JSON** — drop pretty-printing for smaller sidecar files and faster I/O

### Fixed

- Unused imports in `arca-proto` middleware (`std::net::IpAddr`, `http::StatusCode`)

### Added

- **Perf-test summary table**: all test results in a single table with throughput and latency percentiles
- **Global Performance Index**: weighted geometric mean score for run-to-run comparison
- **Perf-test `--tls` flag**: self-contained TLS testing with self-signed certificates
- **Perf-test `--tls-fqdn` flag**: TLS with existing certificates and FQDN verification
- **Perf-test `--encryption` flag**: self-contained encryption testing
- **Perf-test `-q`/`--quiet` flag**: show only the summary table

## [0.18.1] — 2026-04-10

### Added

- **NATS notification connector**: delivers S3 event notifications by publishing JSON payloads to a NATS subject. Supports custom subject names via `subject` property (default `arca.notifications`), optional token or user/password authentication, and configurable connection timeout (`nats_timeout_seconds`). Includes admin API connectivity test via `POST /admin/notifications/test-connector` with `connector_type=nats`
- **(Console)** NATS connector enabled in notification editor with subject, token, and user/password configuration fields
- **MQTT notification connector**: delivers S3 event notifications by publishing JSON payloads to an MQTT topic. Supports custom topic names via `topic` property (default `arca/notifications`), configurable QoS level (0/1/2, default 1), optional username/password authentication, and configurable connection timeout (`mqtt_timeout_seconds`). Includes admin API connectivity test via `POST /admin/notifications/test-connector` with `connector_type=mqtt`
- **(Console)** MQTT connector enabled in notification editor with topic, username, and password configuration fields
- **Database notification connectors** (PostgreSQL, MySQL, MongoDB): delivers S3 event notifications by inserting rows/documents into a database table or collection. Shared `db_common` module provides DDL generation and event field extraction. Tables/collections are auto-created on first delivery
  - **PostgreSQL connector**: `sqlx-postgres` driver, `TIMESTAMPTZ` columns, supports `table` and `schema` properties (default: `arca_notifications`)
  - **MySQL connector**: `sqlx-mysql` driver, `DATETIME(6)` columns, supports `table` property (default: `arca_notifications`)
  - **MongoDB connector**: `mongodb` driver, BSON documents, supports `database` (default: `arca`) and `collection` (default: `arca_notifications`) properties
- **(Console)** PostgreSQL, MySQL, and MongoDB connectors enabled in notification editor with database-specific configuration fields
- **Deployment files**: Kubernetes manifests (Deployment, Service, PVC, ConfigMap), systemd unit file, and sample configuration file for production deployments
- **Troubleshooting guide**: documentation page with common issues, diagnostic steps, and solutions

### Fixed

- **SIGHUP killing the process when TLS is not enabled**: signal handler crashed on `unwrap()` of the TLS reloader when TLS was not configured
- **(Console)** STORAGE SIZE chart Y-axis showed "undefined" labels when all values were 0 (fractional tick values caused negative index in `formatBytes`)

## [0.18.0] — 2026-04-09

### Added

- **Redis Pub/Sub notification connector**: delivers S3 event notifications by publishing JSON payloads to a Redis Pub/Sub channel. Supports custom channel names via `channel` property, optional password authentication, and configurable connection timeout (`redis_timeout_seconds`). Includes admin API connectivity test via `POST /admin/notifications/test-connector` with `connector_type=redis`
- **Dedicated connector test stream**: `bin/test connectors redis` (and future `bin/test connectors all`) runs connector integration tests against real Docker-based receivers, separate from the normal development test flow
- **Presigned URL tracking**: generated presigned URLs are now tracked in the database (metadata only, not the URL itself, for security). Active shares are visible in the console with a permanent blue share icon next to shared files. Clicking the icon shows a detail modal with method, duration, remaining time, creator, and a button to remove the record. Expired records are purged automatically by the retention worker
- **`GET /admin/presigned-urls?bucket=X`**: lists active (non-expired) presigned URL records for a bucket
- **`DELETE /admin/presigned-urls/:id`**: removes a presigned URL tracking record (the URL itself remains valid until expiry)
- **`POST /admin/presign` response** now includes an `id` field for tracking reference (SQLite migration v16, PostgreSQL migration v3)
- **(Console)**: Monitoring chart time range buttons expanded with 6m and 1y presets, plus a custom date range picker (From/To datetime inputs)
- **`bin/seed-metrics`**: development utility to generate fake metrics data for testing monitoring charts

### Fixed

- **Monitoring chart time ranges**: 24h, 7d, and 30d views only showed ~8 hours of data because the API returned `LIMIT 500` most-recent rows. Fixed with server-side downsampling using `ROW_NUMBER()` CTE to return evenly spaced points across the full range (both SQLite and PostgreSQL)
- **(Console)**: X-axis labels now adapt to actual data span (time-only for short ranges, date+time for days, month+day for weeks, month+year for long ranges) instead of being hardcoded per button
- **Ceph s3-tests**: 18 additional tests now pass (lifecycle, object lock, versioning, encryption)

## [0.17.1] — 2026-04-03

### Added

- **Modular notification connector architecture**: trait-based `NotificationConnector` in `arca-core` with `ConnectorRegistry` for pluggable delivery backends. Webhook is the first implementation; 9 additional connectors (Kafka, AMQP, Redis, NATS, MQTT, PostgreSQL, MySQL, MongoDB, Elasticsearch) planned for Phase 26
- **`ConnectorType` enum**: all 10 connector types defined with `display_name()`, `category()`, `all()` helpers for UI enumeration
- **`WebhookConnector`**: extracted webhook delivery logic into a standalone connector implementing the `NotificationConnector` trait, with Bearer token authentication support via `auth_token` property
- **Webhook auth token**: `DestinationConfig` now supports `properties` map (Arca extension) for connector-specific settings. Webhook connector reads `auth_token` and sends `Authorization: Bearer <token>` header
- **`POST /admin/notifications/test-connector`**: generalized admin endpoint for testing any registered connector type; existing `test-webhook` endpoint preserved as backward-compatible alias with auth_token support
- **(Console)**: Notification editor redesigned as a modal dialog (replacing inline lifecycle-style forms) with connector type selector grid showing all 10 types grouped by category (Functions, Queue, Database), SVG icons, and "coming soon" badges for unimplemented connectors
- **Roadmap Phase 26**: Notification Connectors phase added (9 connectors, modular per-connector Docker test infrastructure)

### Changed

- **(Console)**: Settings page reorganized: "Monitoring" section renamed to "Data Retention" with all retention policies grouped together (Audit Log, Notification Events, Metrics Snapshots), ordered by sidebar position
- **Notification event retention** is now a server setting (`notification_retention_days`) manageable from the console, following the same TOML > DB > default precedence as audit and metrics retention (default: 7 days)
- **Notification worker** refactored to dispatch via `ConnectorRegistry` instead of hardcoded webhook delivery. Retry logic (exponential backoff) is now generic across all connector types
- **`DestinationConfig`** extended with `connector_type` (default: Webhook) and `properties` (HashMap) fields, both Arca extensions preserved in JSON storage and XML serialization
- **`NotificationEventRecord`** extended with `connector_type` field (SQLite migration v15, PostgreSQL migration v2)
- **(Console)**: Section title changed from "Notification Webhooks" to "Event Notifications"; notifications list shows connector type badge and icon
- **Webhook test receiver** (`docker/webhook-receiver/server.py`) now supports `AUTH_TOKEN` env var for Bearer token validation (returns 401 on mismatch)

### Fixed

- **(Console)**: Monitoring chart line clipping at maximum values: Y-axis scale now always extends above the data maximum, preventing the line from being drawn outside the SVG viewBox

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

[Unreleased]: https://github.com/dxc-technology/arca/compare/v0.25.0...HEAD
[0.25.0]: https://github.com/dxc-technology/arca/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/dxc-technology/arca/compare/v0.23.1...v0.24.0
[0.23.1]: https://github.com/dxc-technology/arca/compare/v0.23.0...v0.23.1
[0.23.0]: https://github.com/dxc-technology/arca/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/dxc-technology/arca/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/dxc-technology/arca/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/dxc-technology/arca/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/dxc-technology/arca/compare/v0.18.2...v0.19.0
[0.18.2]: https://github.com/dxc-technology/arca/compare/v0.18.1...v0.18.2
[0.18.1]: https://github.com/dxc-technology/arca/compare/v0.18.0...v0.18.1
[0.18.0]: https://github.com/dxc-technology/arca/compare/v0.17.1...v0.18.0
[0.17.1]: https://github.com/dxc-technology/arca/compare/v0.17.0...v0.17.1
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
