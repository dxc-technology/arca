# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`license` field in the Cargo manifests.** The workspace declared no licence at all in its package metadata, so `cargo metadata`, `cargo package` and any downstream tooling reading the manifests saw Arca as unlicensed even though the repository has always carried the licence file and the notices in `README.md`. `[workspace.package]` now sets `license = "AGPL-3.0-or-later"` and all five crates inherit it with `license.workspace = true`. Metadata only: no code, dependency or behaviour change.

## [0.29.0] — 2026-07-28

### Security

- **Anti-replay window on header-signed S3 and admin requests.** The `Authorization`-header SigV4 paths (S3 API and `/admin/*`) never validated `x-amz-date` against the server clock, so a captured signed request — sniffed off a plain-HTTP hop, or lifted from an access log or proxy trace — stayed replayable forever. Both paths now reject a request whose `x-amz-date` lies more than 15 minutes from the server clock (the AWS SigV4 convention, and the same window `/cluster/v1/*` already enforced since 0.26.0), returning `403 AccessDenied`. The signature covers `x-amz-date`, so an attacker cannot re-date a captured request without the secret key. Presigned (query-string) requests were already bounded by their own `X-Amz-Expires`. Clients whose clock drifts beyond ±15 minutes from the server's will now be refused — run NTP.
- **Auth fails closed when a credential's user no longer exists.** A valid credential whose `user_id` did not resolve to a `users` row was silently promoted to a **synthetic root identity** (a leftover backward-compatibility accommodation for pre-users-table credentials), which meant a dangling credential — one left behind after its user was deleted — authenticated as root and bypassed all policy evaluation. Both the S3 and admin auth paths now deny such a request (`403 AccessDenied`), and a genuine metadata-backend failure is a `500 InternalError` instead of being collapsed into the same root fallback. To remove the source of dangling credentials, `UserStore::delete_user` now deletes the user's credentials in the same transaction on both backends (SQLite and PostgreSQL), so a credential can no longer outlive its user. This is not merely defensive: `DELETE /admin/users/{id}` refuses with `409 Conflict` while credentials exist (unchanged), but two paths reach the store without that check and could mint a dangling credential that authenticated as **root** — the `arca user delete` CLI, which validated only "not root", and the cluster control-plane apply of a `UserDelete` op, where the deleting node's check cannot see a credential another node created concurrently. In the cluster the cascade is also what converges the two nodes on "user gone, its keys gone" instead of leaving one peer with a permanently denied key. `arca user delete` now prints each access key it revokes rather than dropping them silently. Credentials predating the users table are unaffected: the migration that added `credentials.user_id` backfilled them to the root user, which exists.

### Fixed

- **Admin access follows the user, not the credential.** A credential carried its own `admin` boolean, and the `/admin/*` gate required BOTH a root user AND `credential.admin = true`. Two paths minted credentials that could never satisfy it: `POST /admin/users/{id}/credentials` (the console's "create credential" flow) hardcoded `admin = false`, so a second credential created on the **root** user signed in as a plain user with no admin reach at all, while `POST /admin/credentials` honoured the request's `admin` field — the same identity therefore got different privileges depending on which endpoint created its key. Privileges are a property of the identity, so the flag is gone: the gate now allows a request when the credential's user `is_root`, or when the user's effective grants allow the `arca:*` action mapped from the admin path (which also, for the first time, lets a non-root user reach the specific admin endpoints its grants cover). The `credentials.admin` column is dropped (SQLite migration v26 / PostgreSQL `0014`), `admin` is removed from the Admin API request and response bodies (replaced by `user_id`, the owning user), from `arca credential add --admin`, and from the console's create-credential form and card badge (now the owner's username, linking to that user). An incoming `admin` field is silently ignored rather than rejected, so an older cluster peer can still replicate credentials to an upgraded node. The reverse direction is *not* supported: a 0.29 node replicates credentials without the field, and a pre-0.29 peer requires it, so a cluster must not be left with mixed versions across this release. The lockout guards on delete/deactivate now key on "last active credential belonging to a root user".

### Removed

- **`admin` flag on credentials.** Dropped from the core type, the database (SQLite migration v26 / PostgreSQL `0014`), the Admin API request/response bodies, the `arca credential add` CLI and the console. See the entry above for the reasoning; incoming `admin` fields are ignored rather than rejected, so older clients and export files keep working, but a cluster must not be left running mixed versions across this release.

## [0.28.0] — 2026-07-24

### Added

- **Guided topology transition `migrate-topology` (Phase 30, M4).** Scripts the two in-place single-node ⇄ HA-cluster topology changes so the operator does not hand-assemble the config and the small DB chores. The cluster is fully replicated (not sharded), so neither direction redistributes data. `arca migrate-topology --to-cluster [--output <file>]` turns a standalone instance into the first node of a new cluster: it refuses if already clustered, reconciles this node's `object_seq` write counter to `MAX(seq)` (so the first clustered write cannot skip a pre-cluster object), and prints a ready-to-paste `[cluster]` stanza with a generated `cluster_id`, a strong random `secret` (32 bytes hex, reusing the `ring`-backed RNG), `mode = "quorum"`, `cluster_size = 3`, `discovery = "mdns"`, and commented templates for static/DNS discovery and `[cluster.tls]` (pointing the operator at `arca tls generate-cluster` for the inter-node CA). Restarting with the stanza enables cluster (tombstone) mode automatically; empty joiners then converge via anti-entropy — no data copy. `arca migrate-topology --to-single --force` collapses a cluster back to one node on the surviving authoritative node: it purges the cluster-only state (object tombstones + control-plane tombstones) and `VACUUM`s SQLite (PostgreSQL autovacuum needs none), then the operator strips `[cluster]` and restarts standalone; `--force` is the explicit confirmation that every peer is synced and stopped (collapsing while a peer is behind loses its un-replicated writes). CLI-only by design (no console job): both directions are inherently operator+restart actions an online job cannot perform. New runbook in the HA guide.
- **Metadata-backend migration `migrate-db` (Phase 30, M3).** Copy ALL metadata from one backend to the other (SQLite ⇄ PostgreSQL) in place, without re-uploading data: blob files are filesystem-resident and untouched, only the metadata database moves. After a run the operator switches `metadata_backend` (and `[storage.postgres]`) in the config and restarts onto the new backend. Available two ways: an offline CLI escape hatch `arca migrate-db --to <sqlite|postgres> [--force]` (server stopped, no writes race the copy), and an online maintenance-mode job (`type: "migrate-db"`, params `{ "target", "force" }`) that drains the S3 API on its node while it runs. The copier is a single generic, type-aware column-walker driven by a static description of all 24 data tables (name + ordered columns + each column's logical kind) rather than per-table structs; it reads/writes every backend's native representation (SQLite TEXT timestamps ⇄ PG `TIMESTAMPTZ`, SQLite INTEGER booleans ⇄ PG `BOOLEAN`, SQLite TEXT JSON ⇄ PG `JSONB`), copies tables in foreign-key-safe order, and reconciles source/destination row counts per table (a mismatch aborts naming the table). Refuses a destination that already holds operator data unless `--force` (which fully replaces it); the always-present schema baseline (root user, built-in grants, seq counter) is ignored. The `object_seq` write counter and the audit/metrics surrogate sequences are migrated/reseeded so writes on the new backend don't collide.
- **In-place re-encryption jobs (Phase 30, M2).** Encrypt existing plaintext objects to SSE-S3 (AES-256), or decrypt them back, without re-uploading and without migrating to a new instance. Runs as a maintenance job (console or admin API, `type: "encrypt"` / `"decrypt"`) in either **live** mode (copy-on-write + compare-and-swap + an optional byte-rate throttle, zero downtime) or **maintenance** mode (S3 drained on the node, full speed). Copy-on-write is used in both modes: a new encrypted blob is written and the object row is atomically swapped to it, so concurrent readers always see a consistent blob and the cluster's blob-id immutability is preserved — the new blob reaches peers via read-repair / anti-entropy and the orphaned old blob is reclaimed by GC. The ETag and Last-Modified are preserved (no lifecycle-clock impact); cluster convergence rides a dedicated `content_updated_at` LWW dimension (separate from Object Lock's `lock_updated_at`) so a re-encrypted row is adopted deterministically without ever clobbering a concurrent lock change, and vice versa. SSE-C objects (customer-held keys) and multipart/composite objects are skipped (TD-014). An offline CLI escape hatch (`arca encrypt-existing` / `arca decrypt-existing`) performs the same transform in place for disaster recovery when the server is stopped. Console **Maintenance** page gains the encrypt/decrypt launchers (bucket/prefix filter, throttle).
- **Maintenance jobs subsystem (Phase 30, M1).** Long-running, operator-launched maintenance operations are tracked as `maintenance_jobs` rows (SQLite migration v24 / PostgreSQL `0012`) so progress survives restarts and is observable from the console. A single background worker processes one job at a time, committing progress per item so pause / cancel / restart are always safe; in a cluster the worker is leader-gated (only the worker-leader node runs it). A job in `maintenance` mode drains the S3 API on its node for its lifetime — every S3 request is refused with `503 ServiceUnavailable` (+ `Retry-After`) AND the health endpoint reports `draining` so the load balancer stops routing — while the admin API and worker stay live (the graceful-shutdown drain stays non-blocking, by design). New JSON admin API under `/admin/maintenance/jobs` (create / list / get-with-logs / pause / resume / cancel / clear-history, single-job lock, state-guarded transitions) and a console **Maintenance** page (launch panel, live active-job card with progress / ETA / rate, log panel, paginated job history with a clear-history action). The re-encryption (M2) and migration (M3/M4) job types build on this foundation.

### Fixed

- **Re-encryption no longer clobbers Object Lock state during cluster convergence (Phase 30).** In-place re-encryption changes an object's blob/algorithm/key WITHOUT bumping `last_modified` — exactly like a retention / legal-hold change. Both used to reuse the single `lock_updated_at` LWW dimension, so during anti-entropy a re-encryption could revert a newer lock state (or a lock change revert a newer re-encryption) whenever the two raced on different nodes. Re-encryption now stamps its own `content_updated_at` dimension (SQLite migration v25 / PostgreSQL `0013`), and `apply_remote_object` merges the content column group (blob/algorithm/key, by `content_updated_at`) and the lock column group (retention/legal-hold, by `lock_updated_at`) independently, so the two converge without ever overwriting each other regardless of apply order.
- **`migrate-db` maintenance job requires maintenance mode.** The online `migrate-db` job copies every metadata table across separate read transactions, so a concurrent write mid-copy would land in some already-dumped tables but not others, producing a torn destination that per-table row-count reconciliation cannot detect. A `live`-mode request is now rejected with a clear message pointing at maintenance mode (or the offline CLI); previously it was accepted and could silently produce an incomplete copy.

## [0.27.1] — 2026-07-07

### Added

- **Per-run blob GC log summary.** Each blob GC pass (the single-node background worker and the cluster anti-entropy worker) now emits a concise INFO summary — `blob GC pass started` and `blob GC pass complete scanned=… candidates=… reclaimed=… failed=… elapsed_ms=…` — so a scheduled run is visible even when it reclaims nothing (previously only a non-zero reclaim was logged). A pass skipped by the fail-safe (an enumeration error) is logged at WARN. `arca gc` likewise prints the scanned/orphan counts.

## [0.27.0] — 2026-07-06

### Added

- **`arca gc` — reclaim orphaned blob files on a single node.** Orphan blobs (on disk, referenced by no live object row, in-progress part, or non-orphan composite sidecar) accumulate from interrupted uploads, overwrites, crashes between the metadata and blob delete, and swallowed blob-delete failures. Until now the only reclamation path was the cluster anti-entropy worker, which is spawned only when clustering is enabled — a single-node deployment never reclaimed them and they grew unbounded (`arca fsck` could report them but not remove them). The new offline command reuses the same composite-aware, fail-safe selection as the cluster worker: it previews by default (dry run) and deletes only with `--reclaim`. `--grace-seconds` (default 86400) protects freshly-written blobs whose object row may not be committed yet; drop it to `0` when the server is stopped. Schedule it from cron or run it on demand. The shared selection logic now lives in one place (`blob_gc::collect_reclaimable_blobs`), used by both the CLI and the anti-entropy worker.
- **Opt-in single-node blob GC worker.** For hands-off operation, a background worker can reclaim orphans on a schedule without an external cron job. Off by default; enable with `[storage] blob_gc_enabled = true` (plus `blob_gc_interval_seconds`, default 3600, and `blob_gc_grace_seconds`, default 86400). It is skipped under clustering (the anti-entropy worker already reclaims orphans) and exposes the `arca_blobs_reclaimed_total` metric. On very large stores prefer cron-ing `arca gc` so the full-store scan runs out of the serving process.
- **`arca_blob_delete_failures_total` metric.** Counts blob-delete failures during object deletion. When a blob delete fails after its metadata row is already gone, the object is genuinely deleted (a subsequent GET returns 404), so `DeleteObjects`/`DeleteObject` still report success per S3 semantics — but an orphan blob is left on disk. This counter makes the orphan-creation rate observable (previously visible only in `warn!` logs), so operators can see whether `arca gc` needs to run.

### Fixed

- **Console: shared bucket links now land on the bucket after login.** Opening a deep-link URL (e.g. a bucket link sent by another user) and then logging in previously always dropped you on the dashboard (admin) or the buckets list (non-admin), discarding the link. The console now preserves the deep link and routes to it after login, only defaulting to home when no destination was given. If the target bucket is inaccessible or missing, the user is bounced home with an explanatory toast — `403` → "You don't have access to bucket …", `404` → "Bucket … does not exist". Because S3 returns no error for a non-existent *prefix*, a missing sub-folder (a non-empty prefix whose listing is entirely empty, with no folder marker) is inferred client-side and returns the user to the bucket root with a "Folder … does not exist" toast; genuinely empty folders still open normally.

## [0.26.1] — 2026-06-12

### Security

- **`time` 0.3.41 → 0.3.47 — CVE-2026-25727 fix picked up (resolves TD-011).** The Docker builder moved from `rust:1.85-alpine` to `rust:alpine` (currently Rust 1.96), unblocking the `time` versions that contain the stack-exhaustion fix; the MSRV pins (`home`, `serde_with`, `darling`) became unnecessary and were removed from the Dockerfile. `time` 0.3.48 remains excluded for a non-security reason (a coherence clash with `rcgen` — tracked as TD-017).
- **`rustls-pemfile` retired (resolves TD-012, RUSTSEC-2025-0134).** PEM parsing migrated to `rustls-pki-types` (`PemObject`), which is maintained as part of the rustls project; the unmaintained dependency is gone from the workspace.

### Changed

- **Full dependency refresh — every crate brought to its latest version.** The lockfile had never been systematically refreshed since the project started; this pass updates all ~540 locked packages and bumps 23 declared majors/minors, with the code migrated where APIs changed:
    - `sqlx` 0.8 → 0.9 (PostgreSQL/MySQL): dynamically composed queries now go through the new `AssertSqlSafe` audit opt-in (all of Arca's dynamic SQL is fixed fragments + bind parameters, audited by construction).
    - `redis` 0.27 → 1.2: connection-info overrides moved to the new builder setters.
    - `lapin` 2.5 → 4.10 (AMQP): native tokio integration — the `tokio-executor-trait`/`tokio-reactor-trait` shims are gone from the workspace.
    - `tonic`/`prost` 0.12/0.13 → 0.14 (gRPC): prost codegen moved to the new `tonic-prost`/`tonic-prost-build` crates; TLS features renamed (`tls-ring`).
    - `quick-xml` 0.36 → 0.40: entity references now arrive as separate reader events — the manual `DeleteObjects` parser accumulates text fragments and resolves character/predefined entities itself.
    - RustCrypto digest 0.11 ecosystem (`sha2` 0.11, `md-5` 0.11, `hmac` 0.13), `rand` 0.10, `rcgen` 0.14 (issuer-based signing API), `reqwest` 0.13 (TLS roots via the platform verifier), `brotli` 8, `lz4_flex` 0.13, `governor` 0.10, `async-nats` 0.49, `rumqttc` 0.25, `rdkafka` 0.39, `mongodb` 3.7, `thiserror` 2, `toml` 1.
    - Known constraints handled: `rusqlite` stays at 0.37 (the latest `tokio-rusqlite` requires `^0.37`); `time` pinned at 0.3.47 (TD-017, see Security).
    - The integration-test image moved to `python:3-slim`; its Python dependencies and the console's CDN libraries already float to latest by construction.
- **Process-default crypto provider hardened.** The refreshed graph compiles more than one rustls crypto provider (lapin's TLS stack brings `aws-lc-rs` alongside Arca's `ring`), so every constructor that builds TLS machinery (cluster client, membership prober, Vault client, replicator, webhook/Elasticsearch connectors, the rustls config loader) now installs the `ring` process default idempotently — previously only `main()` did, which left non-`main` entry points (and unit tests) able to hit the multi-provider ambiguity panic.

### Fixed

- **Generated certificates now pass modern strict TLS verification.** OpenSSL in strict mode — the default for Python 3.13+ clients — refuses CA-issued certificates without an Authority Key Identifier and CA certificates without the KeyUsage (keyCertSign) extension. `arca tls generate` and `arca tls generate-cluster` now emit both, so freshly minted material works with current AWS SDKs, curl and browsers out of the box. (Surfaced by the refreshed integration-test image; previously generated certificates keep working with clients that do not enforce strict mode — regenerate them to be future-proof.)
- **Cold Docker builds produced empty workspace crates.** The builder's dependency-caching layer compiles dummy crates whose cargo fingerprints are newer than the COPY'd real sources' mtimes, so on a cold build (fresh cache, first build, right after `docker builder prune`) cargo considered the real sources unchanged and linked the empty dummies — hundreds of `unresolved import` errors. The real sources are now touched after the COPY; warm builds are unaffected (unchanged context = layer-cache hits all the way). Reproduced on both the old and new toolchain — a latent defect exposed by the cache prune, not by the dependency refresh.
- **Integration tests aligned with current S3/SDK semantics** (no server changes — the server was right): checksum assertions now request `ChecksumMode=ENABLED` on HEAD/GET as S3 requires; suspending versioning on an Object-Lock bucket expects AWS's real `409 InvalidBucketState` (not 400); the notification event-log test signs with `S3SigV4Auth` like every other admin test (the generic `SigV4Auth` signs the payload hash over http but never sends `x-amz-content-sha256`, which the server rightly rejects).

## [0.26.0] — 2026-06-12

### Security

- **Verified inter-node mutual TLS with a cluster CA (Phase 29.1 R4, review §3.7(C), decision H12 — resolves TD-015).** Inter-node TLS no longer accepts invalid certificates: the `danger_accept_invalid_certs` accommodation is gone. A new `[cluster.tls]` section distributes an operator-owned cluster CA plus this node's CA-signed cert/key (mint everything with the new `arca tls generate-cluster`): outbound cluster clients verify peer certificates against the CA and present the node cert as their client identity, while the listener requests client certificates — optional at the TLS layer, because S3 clients share the single port — and `/cluster/v1/*` refuses any request whose connection did not present one signed by the cluster CA. This is the independent second factor on top of the R3 challenge-response (an attacker now needs the secret AND a CA-signed key), and it closes the passive-sniffing / active-MITM exposure on the cluster network. **Breaking**: a cluster running over HTTPS now refuses to start without `[cluster.tls]` (fail closed — there is no insecure fallback); plain-HTTP clusters are unaffected (the R3 peer authentication remains their baseline; use TLS on any network you do not fully trust).
- **Anti-replay window on cluster requests (Phase 29.1 R4, review §3.1/§3.7(C)).** `/cluster/v1/*` now rejects signed requests whose `x-amz-date` lies more than 15 minutes from the node's clock (the SigV4 convention). Previously a captured signed request — e.g. a blob `GET` sniffed off a plain-HTTP segment — remained replayable forever; the signature covers the timestamp, so it cannot be refreshed without the secret. Cluster nodes already require NTP.
- **Cluster secret strength enforced (Phase 29.1 R4, review M5/§3.7(B)).** Startup now refuses a `[cluster].secret` shorter than 16 characters and the placeholders shipped in the reference configs (`dev-cluster-secret-change-me`, `CHANGEME-CLUSTER-SECRET`), with a warning when the secret looks low-entropy — generate one with `openssl rand -hex 32`. The secret keys all inter-node authentication (SigV4 + the peer challenge), so a guessable value hands an attacker the whole cluster. The dev/test cluster configs now ship a floor-compliant dev secret; deployments still on a placeholder fail at startup with a clear message until it is replaced.
- **Cluster endpoint defense-in-depth (Phase 29.1 R4, review §3.4, M6, D12.1).** The JSON cluster endpoints (`object`, `object/delete`, `op`, `manifest`, `control-snapshot`, `ping`) now cap request bodies at an explicit 2 MiB (blob transfer streams to disk and stays uncapped); the `blob_id` path parameter must parse as a UUID before it reaches the blob layer, where it becomes a file name; and the replication receive side drops node-local `server_config` keys (the node identity), so not even a buggy or malicious peer can rewrite who a node is — the filter previously existed only sender-side.
- **Cluster peers are now authenticated, not just cluster requests (Phase 29.1 R3, review §3.7(A)/D1, decision H12).** Inbound `/cluster/v1/*` requests were always SigV4-verified, but the *outbound* direction trusted any endpoint that answered a public health probe: a rogue process on the cluster network registering itself over mDNS with the right `cluster_id` would be counted alive, *receive the replication fan-out of every new write* with no knowledge of the cluster secret, and sustain a ghost write-quorum. Membership now probes peers with a signed `GET /cluster/v1/ping` carrying a fresh nonce, and a peer is **authenticated** only if its response proves possession of the cluster secret (`nonce_mac = HMAC(secret, nonce)` — answering 200 proves nothing, a recorded MAC is useless against a fresh nonce). Replication fan-out (objects, blobs, control plane), anti-entropy pulls (manifest, control snapshot, blob repair — also a data-poisoning vector), the write-quorum count and the cluster-minimum capacity now all consider **eligible peers only**: alive AND authenticated AND config-aligned. Legacy pre-upgrade peers (no ping route) stay visible as alive but are excluded until upgraded — complete a rolling upgrade promptly. On plain-HTTP clusters this closes the rogue-peer attack; passive sniffing / active MITM still require the verified inter-node TLS arriving with R4.
- **Public cluster health minimized (Phase 29.1 R3, review §3.5/§3.7(B)).** The unauthenticated `GET /cluster/v1/health` now returns only `{status, node_id}`. It previously exposed the cluster config fingerprint — an *offline brute-force oracle* for the cluster secret (truncated SHA-256 over mostly-guessable inputs) — plus disk capacity. That detail moved to the authenticated ping, where only secret holders can read it.

### Added

- **Write-aware health check (Phase 29.1 R9, review D5).** `GET /admin/health?writable=1` answers `503 {"status":"read_only"}` (with `Retry-After`) while the node's cluster write gate is closed — write quorum lost or cluster size exceeded — and `200` otherwise (single-node deployments and `available` mode are always writable). A node that lost the quorum deliberately stays `200` on the default health check (it serves every read); pointing a *separate* load-balancer write pool at the write-aware variant routes write methods only to nodes that would accept them, instead of letting ~1/N of the writes fail with a client-visible 503 while the cluster is degraded. Both shipped HAProxy configs carry the write pool as a commented reference backend.
- **`[cluster] tombstone_grace_seconds` (Phase 29.1 R9).** Advanced seconds-granularity override of `tombstone_grace_days` (≥ 1), for tests and demos that must observe the tombstone-GC liveness guard within seconds; production deployments keep sizing the grace in days.
- **Cluster operational runbooks and honest consistency docs (Phase 29.1 R9, review D3/D5/D7/D10/D11/D12.4, §5-doc).** The HA guide gains an Operational Runbooks section — replacing a dead node (an empty disk is safe by construction), restoring a node from backup (and when not to: a backup older than the tombstone grace), resizing the cluster (cold procedure; why rolling cannot work), rotating the cluster secret, taking coherent backups (filesystem snapshots vs stopped-node copies, SQLite WAL caveat), and forming a cluster from non-empty nodes (a silent union+LWW merge — discouraged, with the safe alternative). The consistency documentation now states plainly what was previously only implied: in `available` mode concurrent writes to the same key are both acknowledged but only the LWW winner survives (the loser is discarded with no error to the client) and durability is single-copy until the fan-out lands (RPO > 0); in `quorum` mode reads are not linearizable (R = 1 by design) and a minority node serves reads that age for a partition's whole duration; the WORM/Object-Lock trust model in a cluster (verbatim remote applies; immutability rests on the secret, the cluster CA and OS access on every node); and the clock-skew test gap is declared instead of implied away.
- **Tombstone-GC and blob-repair integration tests (Phase 29.1 R9, review §5.4).** New cluster test phase on a dedicated overlay: the proactive blob-repair sweep is proven by deleting an object's payload file straight from a node's volume and watching the file reappear (observed on the volume — a client GET would mask the sweep with lazy read-repair) and serve intact bytes; the §3.2 GC liveness guard is proven end-to-end — an object deleted while a node is down beyond the grace blocks tombstone GC on both survivors, the returning node learns the deletion (no resurrection anywhere), and the guard releases.
- **Per-node admin views behind the load balancer (Phase 29.1 R8, review D6, decision H9).** The audit log, metrics history, notification event log and replication journal are strictly node-local (each node records what *it* served), so behind a load balancer their console views showed whichever node luck routed to — with nothing saying which. The four list endpoints (`GET /admin/audit`, `/admin/metrics/history`, `/admin/notifications/events`, `/admin/replication/journal`) now accept `?node=<node_id>`: the serving node proxies the query server-side to that peer over the signed cluster transport (new `POST /cluster/v1/admin/*` receive routes, behind the same peer authentication, mTLS enforcement and body caps as every cluster endpoint — browsers never talk to nodes directly and the cluster credential never leaves the server side). Only *eligible* peers are valid targets (unknown node → 404, dead/drifted → 503, unreachable → 502); every response carries a top-level `node` label, including the default LB path. `?node=all` returns the merged view: a parallel fan-out to every eligible node, rows merged newest-first, each labeled with its source node, plus a per-source result report so a partially-failed fan-out is visible. Pagination of the merged view is per-source-page (each node is asked for the same window and the newest rows across nodes are kept) — an approximation by design, documented instead of hidden behind cross-node cursors. The console gains a node selector in all four views (hidden when clustering is off): default "This node (via LB)" with a badge always naming the node that answered, per-node options, and "All nodes" with per-row source badges; Monitoring draws one chart series per node in the merged view. Clear All and journal Retry are disabled while a node is selected — they operate on the node serving the request, not the viewed one.
- **Syncing readiness gate for re-entering cluster nodes (Phase 29.1 R7, review D2).** A node returning from downtime (or replaced with an empty disk) used to enter the load-balancer rotation immediately and answer `404` / partial listings for data it had not pulled yet — wrong answers given with full confidence, on ~1/N of the routed traffic, with nothing signalling it. A clustered node now tracks the completion of its **first anti-entropy pass toward every eligible peer since startup**, and until then the plain `GET /admin/health` answers `503 {"status":"syncing"}` (with a `Retry-After` hint; a shutdown drain takes precedence), so the LB and the Kubernetes readiness probe keep it out of rotation exactly while it could mislead clients. The verbose health stays inspectable (`200` with `status: "syncing"`); with no eligible peers the node reports `ok` (degraded-but-serving, there is nothing to sync from). New cluster test phase: a node restarted after downtime must hold the 503 until caught up, and the moment it reports ready the data written during its absence must already be readable on it.
- **Per-peer sync state exposed (Phase 29.1 R7, review D2/M1).** `GET /admin/cluster` now reports the node-level `syncing` flag plus, for every peer, this node's pull state toward it: the `hwm` manifest cursor, the `lag` still to pull (against the peer's ping-reported write cursor), `last_reconcile`, `first_pass_done` and `skipped_entries`. The console topology card shows a syncing notice and amber per-node `sync lag` / `skipped` indicators when non-zero.
- **Restore-from-backup rewind detection (Phase 29.1 R7, review D3c).** Each node advertises its object write cursor on the authenticated ping; a peer reporting a cursor *below* what this node already consumed from it was restored from a backup, and its post-restore writes (whose `seq` values re-use the rewound range) would have stayed invisible to the incremental sync until this node restarted. The high-water mark is now reset automatically (one idempotent full re-pull, logged as a warning). A freshness guard — the report must be newer than the last cursor advance — keeps sustained writes from false-alarming.
- **`[cluster] blob_repair_budget` (Phase 29.1 R7, review M2).** The proactive blob-repair sweep now attempts at most this many peer fetches per anti-entropy tick (default 100, ≥ 1) and resumes where it left off on the next tick — previously a node missing many blobs repaired them all in one pass, monopolizing the anti-entropy worker for hours on a large store. Between budget slices, lazy read-repair still covers anything a client actually asks for.
- **Complete control-plane reconcile (Phase 29.1 R5, review D9 — resolves TD-016).** The anti-entropy snapshot merge now covers EVERY control-plane family, not just the original five: grant attachments (`user_grants`, `team_grants`), team memberships, per-bucket config keys (versioning, encryption, …), bucket tag sets, and cluster-wide `server_config` settings. Each family carries a per-row LWW timestamp (migrations sqlite v22 / pg 0010) and deletions leave tombstones, so a node that was down when a grant was revoked, a membership removed or a bucket setting changed now fully self-heals at re-entry — previously those families replicated in real time only and a missed change persisted until the next write touched it. Child families are parent-filtered in the merge: rows whose parent (user, team, grant, bucket) resolved deleted are never adopted, replacing per-child cascade tombstones. Bucket tag sets reconcile as ONE entity per bucket (matching the replace-all semantics of `PutBucketTagging`).
- **Multipart uploads reconcile across the cluster (Phase 29.1 R5, review D4).** In-progress multipart uploads and their part rows are now part of the control snapshot: a node that was down during `CreateMultipartUpload`/`UploadPart` learns them at re-entry, and a `multipart` tombstone recorded at Complete/Abort time stops a closed upload from resurrecting off a stale peer. `CompleteMultipartUpload` on a node that has the part rows but is missing some part *bytes* (their fan-out never reached it) now repairs them from a peer before assembling instead of failing.
- **SSE-C objects get cluster read-repair (Phase 29.1 R5, review §3.6).** SSE-C blobs always replicated (the sidecar write ships the customer-key-encrypted bytes verbatim — peers never see the key), but a GET on a node that had the object row and not yet the bytes failed until the next anti-entropy pass, because the SSE-C read path bypassed the cluster wrapper. SSE-C reads now repair the missing bytes from a peer synchronously, like every other object.
- **`arca tls generate-cluster` (Phase 29.1 R4).** Mints the cluster CA plus one CA-signed certificate per node (`--node name=san1,san2,...`, SANs covering every name/IP peers dial; serverAuth + clientAuth EKUs so the same pair serves as listener certificate and inter-node client identity), and prints the `[server.tls]` / `[cluster.tls]` snippets to paste on each node. Operators with their own PKI can supply equivalent material instead.
- **Zero-downtime cluster secret rotation (Phase 29.1 R4, review D3b, decision H8).** New optional `[cluster] secret_previous`: inbound cluster authentication accepts the current secret first, then the previous one, while outbound signing, the challenge MAC and the config fingerprint always use the current secret (the ping answers with the secret the request verified under, so probes keep verifying across the window). Runbook: set `secret_previous` = old and `secret` = new on every node, rolling restart, then remove `secret_previous`. During the window expect transient drift flags (the fingerprint includes the secret): nodes on the new secret form the writable side once they reach a majority.
- **HA Hardening plan (Phase 29.1).** The remediation plan for the findings of the in-depth Phase 29 HA review is now part of the official roadmap: a new Phase 29.1 section (progress bar, dependency graph, summary table, milestone checklist) plus the full working plan — fixed design decisions, per-milestone detail, and a finding-by-finding traceability table — published as a living page on the documentation site.
- **Cluster-size write guard (Phase 29.1 R3, review D3a, decision H6).** In `quorum` mode, if MORE eligible nodes than the configured `cluster_size` are observed, every node refuses writes with `503` (fail-closed, no config escape hatch) and `/admin/cluster` reports `size_exceeded: true`. The write majority is derived from `cluster_size`, so an over-sized membership (e.g. a 4th node started with the 3-node config) would let two disjoint "majorities" accept conflicting writes — split-brain. Resizing has a documented cold procedure (R9 runbook).
- **Tombstone-GC liveness guard (Phase 29.1 R3, review §3.2).** Anti-entropy now skips the tombstone purge while any known peer has been unreachable beyond the grace window, with a loud warning and a `tombstone_gc_blocked: true` flag on `/admin/cluster` — purging would erase the very tombstones that returning peer needs to learn its missed deletions, resurrecting deleted data. Membership pruning (below) eventually evicts a never-returning peer so GC cannot stay blocked forever.
- **Cluster membership pruning (Phase 29.1 R3, review M3).** Peers unreachable beyond `[cluster] peer_prune_days` (default: `tombstone_grace_days`) are evicted from membership (and from the admin/console topology). A pruned node that later returns is re-discovered and re-synced like a new candidate; returning *beyond* the tombstone grace is the documented residual resurrection risk.
- **`/admin/cluster` topology detail (Phase 29.1 R3).** Per-node `authenticated` flag (did the node prove possession of the cluster secret on its last probe), cluster-level `eligible_node_count` (what the write quorum is measured against, vs. the visibility-only `live_node_count`), `size_exceeded` and `tombstone_gc_blocked`.
- **`[lifecycle]` worker interval (Phase 29.1 R6).** New optional `[lifecycle] interval_seconds` (default: 3600, the previous fixed value) controls how often the background worker evaluates bucket lifecycle rules.

### Changed

- **Cluster deploy references aligned (Phase 29.1 R9, review §5-deploy).** The production HAProxy config, the test/demo one and the HA-guide snippet now agree: production health checks use `fall 3 rise 2` (ride out a transient blip), the test/demo config keeps `fall 2 rise 1` for fast convergence and says so, and both carry commented `balance source` (sticky read-your-writes) and write-aware-pool examples. The Kubernetes readiness probe tightens to `periodSeconds: 5, failureThreshold: 2` (~10 s worst-case window of traffic to a crashed pod) and its comments now describe the syncing semantics introduced in R7.
- **The lifecycle evaluator is now a cluster singleton (Phase 29.1 R6, review §3.3, decision H5).** The object table is fully replicated, so every node used to evaluate the same lifecycle rules and delete the same expired objects — N× the delete fan-out, duplicate audit entries and races between concurrent deleters. Only the **worker leader** now runs the evaluation tick (expirations, noncurrent-version deletes, stale-multipart aborts): the node with the lowest `node_id` among the *eligible* nodes, the same authenticated-and-config-aligned predicate that gates the write quorum — so an unauthenticated rogue cannot steal the role and silence the workers. No election protocol: each node computes the role locally from its membership view, and failover to the next-lowest node is automatic at the following membership tick (a brief double-execution window during membership disagreement is accepted — the work is idempotent). `/admin/cluster` and `/admin/health?verbose=1` report the node's claim as `worker_leader`; in a stable cluster exactly one node says `true`. The per-node workers (metrics snapshot, retention purge, notification delivery) and the Phase 28 bucket-replication worker stay un-gated by design: they operate on strictly node-local state — in particular the replication journal records each client write only on the node that served it, so deliveries to external destinations are already exactly-once cluster-wide (the review's duplicate-delivery premise did not hold; gating that worker would instead strand the entries of non-leader nodes).
- **Configuration export omits the node identity, import refuses it (Phase 29.1 R5, review D12.2).** `GET /admin/export` no longer includes the node-local `node_id` settings key, and `POST /admin/import` skips it even when present in the document (e.g. an export taken before this change) — importing another node's identity would corrupt cluster loop-prevention. Double defense, mirroring the existing replication-side filter.
- **Cluster failure detector tolerates one missed probe (Phase 29.1 R3, review D12.3).** A peer is declared dead after 2 *consecutive* probe failures (and alive again at the first success) instead of flapping on a single missed probe — a routine GC pause or dropped packet no longer bounces a node out of the quorum. Dead peers now also keep their `last_seen` timestamp in `/admin/cluster` (how long a node has been unseen is what the GC guard and pruning reason about).
- **Config drift now excludes a node from the write quorum (Phase 29.1 R3, review D1/H7).** A peer whose cluster-critical config differs (`config_ok: false` — wrong secret, different master key, different mode) stays visible in the topology but no longer counts toward the write quorum and no longer receives replication fan-out: it could not store the replicas correctly anyway. With 1 of 3 nodes drifted the cluster keeps writing (2 eligible ≥ quorum 2); with 2 of 3 drifted the aligned node refuses writes with `503`. Drift detection itself no longer depends on the (removed) public fingerprint: a wrong-secret node is detected by its `403` on the authenticated ping, an aligned-secret/wrong-key node by the fingerprint now exchanged inside the ping.
- **Real-network-partition test infrastructure (Phase 29.1 R2, review D8/§5.1/§5.2).** The 3-node cluster suite grows from 11 to 24 tests and now exercises the scenarios the consistency modes exist for, not just crash-stops. `bin/cluster partition <n>` / `heal <n>` cut a node off the inter-node network while its process stays alive (the cluster compose now uses two networks, so the test runner keeps reaching the isolated node). New phases verify: in `quorum` mode an isolated node refuses writes with `503` *immediately* — inside the window where its membership still believes the peers are alive, the missing fan-out ACKs close it (the R1 true-quorum fix at work) — while reads keep being served, the majority side keeps writing, and the healed node converges and becomes writable again; in `available` mode (new config overlay) a split-brain write to the same key is accepted on both sides and exactly one LWW winner survives everywhere after heal, and a 1-of-3 minority node still accepts writes. The catch-up phase now also covers the control plane: a bucket and a credential created while a node is down appear on it after re-entry.
- **True write quorum in cluster `quorum` mode (Phase 29.1 R1, review §2.1).** A replicated write (PutObject, CompleteMultipartUpload, DeleteObject, delete markers, version deletes) is now acknowledged to the client only when at least `write_quorum` nodes durably hold it *at ACK time*: the local copy plus every peer whose fan-out response certified "row applied AND blob present". Previously the quorum was only an admission gate on membership liveness — a write whose fan-out failed on every peer was still acknowledged and could exist on a single machine. On a quorum shortfall the client now receives `503 ServiceUnavailable` with a `Retry-After` header; the local copy is *not* rolled back (as in any quorum system without distributed transactions, the error means "not acknowledged as replicated", not "undone") and anti-entropy either propagates it or a retry overwrites it. `available` mode is unchanged.
- **Parallel cluster fan-out (Phase 29.1 R1, review §2.4).** Replication to peers (object rows, blobs, version deletes, control-plane ops) is now sent to all live peers concurrently instead of one at a time, so a slow peer no longer adds its latency to every write and the ACK counting above sees all responses in one round-trip.

### Fixed

- **Kubernetes liveness probe could kill a node mid catch-up (Phase 29.1 R9).** Since the R7 syncing gate, `/admin/health` answers 503 while a returning node re-pulls what it missed — which is exactly right for *readiness*, but the shipped manifest pointed the *liveness* probe at the same path: a catch-up outlasting three probe periods would get the pod restarted in a loop precisely while it was healing. The liveness probe now uses `?verbose=1`, which always answers 200 on a live process.
- **Object-Lock state could be silently erased cluster-wide by anti-entropy (Phase 29.1 R7, finding N2).** Retention/legal-hold changes keep `last_modified` unchanged (matching S3), so two replicas of the same object version could differ *only* in lock state while their last-writer-wins key tied — and the tie was resolved in favor of whichever copy happened to be applied last. The losing sequence: a retention is set while one node is down; another node later restarts and re-pulls that peer's full manifest (its sync cursor is in-memory), re-applies the stale lock-free copy over its own newer lock state, and the rewrite's fresh manifest cursor propagates the regression to every node — the retention silently disappears cluster-wide, with no error anywhere. For a WORM feature this is data loss of the protection itself. Object rows now carry a `lock_updated_at` timestamp (sqlite migration v23 / PostgreSQL 0011), stamped by every retention/legal-hold change, and the replication apply guards order equal-timestamp copies by it — a stale copy can no longer clobber a newer lock state, in either direction. Found by the new cluster catch-up test; root-caused via seq-counter forensics on the preserved node databases.
- **A poison manifest entry no longer blocks a peer's incremental sync forever (Phase 29.1 R7, review M1).** A row that persistently failed to apply stopped the anti-entropy cursor toward that peer at the failing entry, freezing the whole incremental sync (everything behind it included) until a restart. After 5 consecutive failed passes on the same entry the node now skips it with a warning and counts it as `skipped_entries` in `/admin/cluster` — operator evidence that the key may not converge locally until it changes again on the peer.
- **Unreadable blob mtimes no longer silently disable blob GC (Phase 29.1 R7, review M8).** When a blob file's mtime cannot be read, the GC walk falls back to "just written" — the safe direction (never reclaim), but previously a silent one: on a filesystem that cannot report mtimes, orphan blobs would simply never be collected with no trace of why. The fallback now logs a warning naming the file.
- **Object-Lock changes were invisible to cluster anti-entropy (Phase 29.1 R5, finding N1).** `PutObjectRetention` / `PutObjectLegalHold` updated the object row without stamping a fresh manifest cursor (`seq`) on either backend, so a peer that was down during a lock change never received it — only the real-time fan-out carried it, and a dead node misses that by definition. For a WORM feature this silently weakened the guarantee on the returning node. Lock updates now stamp a fresh `seq`, so the changed-since manifest re-delivers the row with its lock columns at re-entry.
- **Steady-state anti-entropy churn: identical rows were endlessly redelivered (Phase 29.1 R5, review M7).** `apply_remote_object` rewrote the row (stamping a fresh `seq`) even when the incoming copy was byte-identical to the local one. Two caught-up nodes therefore kept re-delivering their whole object tables to each other on every anti-entropy pass, forever — each apply bumped a `seq` the peer then saw as new. An identical incoming row is now a no-op (the equal-timestamp guard still lets through rows that differ only in lock columns, which N1 above relies on), so a converged cluster reaches a quiet fixed point.
- **PostgreSQL: rows could be skipped by cluster incremental sync (Phase 29.1 R1, review §2.2).** The PG backend assigned the anti-entropy manifest cursor (`seq`) from a PostgreSQL `SEQUENCE`, which is not transactional: a row could commit with a lower `seq` *after* a peer had already advanced its cursor past it, so the incremental sync silently never delivered it. The sequence is replaced by a commit-ordered single-row counter (`object_seq`, pg migration 0009, mirroring the SQLite design): the counter's row lock serializes assignment until commit, so cursor order now equals commit order and no committed row can be skipped.
- **Control-plane merge could resurrect deleted entities on crash (Phase 29.1 R1, review §2.3).** The anti-entropy control-plane merge executed entity deletions *before* adopting the corresponding tombstones, each in its own transaction: a crash between the two left "row deleted, no tombstone", and the next reconcile with a peer still holding the live row re-created it — for a revoked credential, a security hole. Tombstones are now adopted first, so an interrupted merge always converges to the deletion.
- **Cluster test flakiness (Phase 29.1 R2, review §5.3).** Two latent races in `bin/test cluster`: the post-convergence wait was shorter than HAProxy's eviction time (`fall 2 inter 2s` ≈ 4 s), so a test could hit the load balancer while a stopped node was still in rotation; and the 507 phase could read the cluster-minimum free space before the tiny-disk node was alive in the topology (its stats are cleared while it is considered dead). The wait now gives the LB a 5 s margin and the 507 precondition requires all 3 nodes alive.

## [0.25.1] — 2026-06-10

### Changed

- **Performance: per-bucket replication-config cache.** Object writes (PutObject, CompleteMultipartUpload, delete markers, PutObjectTagging) no longer read the bucket's `replication_configuration` from the metadata store on every request. The result is cached per bucket for 30 seconds — mirroring the existing per-bucket encryption cache — so buckets without replication (the common case) skip the database query entirely on the write hot path. The cache is invalidated immediately on `PutBucketReplication`, `DeleteBucketReplication`, and the admin credential-cascade disable, so a configuration change still takes effect at once.
- **S3 compatibility scope.** Ceph s3-tests for RGW-only extensions (`x-rgw-*` headers, `?list-type=unordered`, account usage) — which are not part of the AWS S3 API — are now excluded from the reported compatibility denominator. A new "Intentional divergences from AWS S3" section in the API reference documents Arca's deliberate allowance of enabling Object Lock on an existing *empty* bucket (AWS permits it only at bucket creation).

### Fixed

- **Intermittent `400 Bad Request` on a keep-alive connection reused immediately after a rejected conditional `PutObject`.** When a `PutObject` was rejected before its request body was read (a failed `If-Match` / `If-None-Match` precondition → `412`, or `NoSuchBucket` / `NoSuchKey` / `EntityTooLarge` / invalid tagging), the unconsumed request body could desync the HTTP/1.1 keep-alive connection, so the *next* request parsed on it intermittently failed with a protocol-level `400`. These early-reject responses now send `Connection: close` (matching AWS S3), so the client never reuses a possibly-desynced connection.

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

[Unreleased]: https://github.com/dxc-technology/arca/compare/v0.29.0...HEAD
[0.29.0]: https://github.com/dxc-technology/arca/compare/v0.28.0...v0.29.0
[0.28.0]: https://github.com/dxc-technology/arca/compare/v0.27.1...v0.28.0
[0.27.1]: https://github.com/dxc-technology/arca/compare/v0.27.0...v0.27.1
[0.27.0]: https://github.com/dxc-technology/arca/compare/v0.26.1...v0.27.0
[0.26.1]: https://github.com/dxc-technology/arca/compare/v0.26.0...v0.26.1
[0.26.0]: https://github.com/dxc-technology/arca/compare/v0.25.1...v0.26.0
[0.25.1]: https://github.com/dxc-technology/arca/compare/v0.25.0...v0.25.1
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
