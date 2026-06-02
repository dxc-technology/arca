# Arca Phase 29 — High Availability (symmetric self-configuring cluster)

## Context

Arca today is single-node: blobs on the local filesystem (UUID files + `.meta` JSON sidecars) and metadata on local SQLite or PostgreSQL. If the server dies, the service stops. Goal: **when a node dies, the service keeps being served** (reads and writes), with no shared filesystem, no shared DB, no external coordination services (no Zookeeper/etcd/Consul, no FUSE).

Key product constraint (Pietro): **total symmetry**. The TOML configuration must be **byte-identical on every node** (no per-node `node_id` or peer list), so that any subset of nodes can be restarted (e.g. nodes 2 and 3 without node 1) and the cluster re-forms. Nodes **discover each other automatically** on the LAN.

Starting point: Arca already has 90% of the replication infrastructure (Phase 28). We reuse `OutboundClient` (SigV4) as the transport baseline, the `x-amz-arca-replication-source` header for loop-prevention, the credentials in `server_config`, and `/admin/health` (200 ok / 503 draining) as the LB health check. (NOTE: the initial plan also called for reusing `ReplicationStore`+`claim_batch`/backoff for hinted handoff; hinted handoff was later DROPPED — see decision 9. Recovery uses anti-entropy + read-repair.)

Erasure coding is NOT in this phase (full-copy replication; RS remains a future option, patent-free, Apache 2.0 crates available, without copying MinIO AGPL code).

## Approved decisions (Pietro, 2026-05-30)

1. **Total symmetry**: identical TOML config on every node. `node_id` **auto-generated and persisted** by the node, never in config.
2. **Auto-discovery**: nodes find each other on their own. **mDNS** (`mdns-sd`, Apache-2.0, pure Rust, no daemon) as the LAN default; **fallback to an identical seed list / headless DNS** for Docker/Kubernetes where multicast does not pass.
3. **3-node full-replica topology** (every node holds everything). Tolerates 1 failure while keeping full service.
4. **No r/w/n**: one single understandable number, `cluster_size` (expected size), from which the majority is derived.
5. **Configurable consistency model** via `mode` (one option in `[cluster]`), same replication engine underneath:
   - `mode = "quorum"` (default, CP): writes succeed only with a majority of nodes; a minority node becomes read-only. No divergence possible.
   - `mode = "available"` (AP): always writable (even a single node); partition conflicts resolved last-write-wins.
6. **Client failover**: external LB/VIP (HAProxy + keepalived), health on `/admin/health`. Zero routing inside Arca.
7. **Erasure coding**: deferred to a future phase.

## Architectural updates decided during M4 (2026-06-02)

Decisions taken while implementing M4, which UPDATE/CORRECT the original plan. Documented here because they are the architectural heart of the resilience story.

8. **Tombstones for hard deletes (CORRECTION of a plan bug).** Anti-entropy makes nodes converge by *union* of rows (LWW), but a hard delete is the *absence* of a row: a node that missed the delete ships the still-alive row back via manifest → **resurrection** of the deleted object. This affects hard deletes on unversioned buckets, explicit version deletes and lifecycle expiration (delete markers on versioned buckets are rows, hence already safe). **Solution**: in cluster mode a hard delete leaves a *tombstone* row (`is_tombstone=1`, blob cleared, fresh `seq`, `last_modified`=delete instant) instead of removing it; single-node keeps removing as it always did (gated by `set_cluster_mode`). Key insight: the **already-existing LWW on `last_modified` converges tombstones with no special cases** (the tombstone sorts after the deleted row and before any later PUT). Reads exclude tombstones (`recompute_is_latest` skips them, so every `is_latest=1` query is free). GC after `tombstone_grace_days` (> max expected downtime, default 7).

9. **NO hinted handoff (DEVIATION from the M4 plan).** The plan called for hinted handoff + anti-entropy. With tombstones + anti-entropy + read-repair the correctness of deletes is already covered, so hinted handoff remains only a latency optimization. It is standard in BIG clusters (Cassandra) because there reconciliation is expensive and rare; in Arca (N=3 full replica) the changed-since manifest makes anti-entropy incremental and cheap → run it often and get the same fast recovery without the post-it machinery (accumulation during long absences, dedup, expiry). Chosen for "simple and beautiful, not over-engineered".

10. **Changed-since cursor = node-local monotonic `seq` (not `updated_at`/`write_id`).** Every node has a `seq` counter on `objects` (dedicated `object_seq` table on sqlite — immune to the deletion of the highest-seq row, unlike `MAX(seq)+1`; a `SEQUENCE` on pg). It is NOT a field of `ObjectRecord`: it is stamped on every local write including `apply_remote_object`, so reconciliation propagates A→B→C. `last_modified` is NOT a valid cursor (it is the replicated logical mtime, not the node's write order); wall-clock suffers from clock skew; `seq` does not.

11. **No `write_id` (CORRECTION of the M2 plan).** The plan proposed a `write_id` column to disambiguate LWW on null versions. Decided NOT to add it: the tiebreak uses the already-existing `blob_id`, ordering `(last_modified, version_id, blob_id) DESC`. No `ObjectRecord` migration, no new field.

12. **Control-plane reconcile = generic tombstones + full-snapshot LWW (not an op-log).** Same trap as object deletes: a full-set comparison alone resurrects deleted entities (e.g. a revoked credential = a security hole). Chosen: full control-plane snapshot exchanged periodically + LWW merge by `updated_at`, with a generic `control_tombstones(entity_type, entity_key, deleted_at)` table to propagate deletes without resurrection. NOT an op-log: it grows forever, cannot bootstrap a new/long-absent node beyond the GC horizon, and incremental efficiency is pointless for a tiny control plane (over-engineering). The snapshot IS the bootstrap; reads stay unchanged (the entity is truly deleted, the tombstone only serves reconciliation); works in both modes.

## Configuration (identical on ALL nodes)

```toml
[cluster]
enabled = true
cluster_id = "arca-prod"     # logical name: only nodes with the same id form a cluster (discovery filter)
secret = "..."               # shared secret authenticating inter-node requests (identical everywhere)
mode = "quorum"              # "quorum" (CP, default) | "available" (AP)
cluster_size = 3             # expected size; used ONLY in mode="quorum" for the majority
discovery = "mdns"           # "mdns" (LAN, default) | "static" (seed list) | "dns" (k8s headless name)
# advertise_port = 9000      # OPTIONAL, default = [server].port. Port PEERS reach me on;
#                            #   only needed when different from the bind port (container with external mapping/NAT).
# advertise_addr = "host"    # OPTIONAL, default = auto-detected. Host/IP announced to peers (when bind = 0.0.0.0).
# seeds = ["arca-2:9000", "arca-3:9000"]   # ONLY discovery="static"; identical list, the node ignores itself
# dns_name = "arca-cluster.default.svc.cluster.local"  # ONLY discovery="dns"
```

- **No `node_id`, no own address, no per-node peer list.** Everything is auto-derived or discovered.
- **The port is not repeated**: nodes listen on `[server].port`. `advertise_port`/`advertise_addr` are OPTIONAL overrides for the sole cases where peers reach the node on a port/address different from the bind ones (container port mapping, NAT). Without overrides, the endpoint announced in discovery uses `[server].port` and the interface IP.
- `cluster_size` is mandatory when `mode="quorum"` (startup error if missing); ignored in `mode="available"`.
- Documented constraint: in `mode="quorum"` `cluster_size` must reflect the real number of nodes, otherwise the majority is mis-computed (the known limit of fixed-size quorums, acceptable for a small symmetric cluster).

## Node identity (auto-derived)

`crates/arca-server/src/cluster/identity.rs` (new), modeled on `credential::ensure_root_credential`:
- At startup `ensure_node_id(server_config)`: reads the `node_id` key from `server_config`; if absent, generates a `uuid::v4` and persists it (upsert). Reused across restarts and through migrations.
- This `node_id` replaces/unifies `replication_source_id` (today defaulting to `"arca"` in config): it becomes the identity for loop-prevention AND for the cluster, without living in the config file. Solves symmetry.
- Hook point: in `main.rs` right after `open_stores`, before building `AppState` (where `replication_source_id` is taken from config today).

## Discovery and membership

`crates/arca-server/src/cluster/membership.rs` (new) — the `MembershipManager`:
- **mDNS** (`mdns-sd`): announces `_arca._tcp.local` with TXT `cluster_id`, `node_id`; browses continuously and registers peers with the same `cluster_id`. Pure Rust, no system daemon.
- **`static`/`dns` fallback**: periodically resolves the seed list or the headless DNS name (k8s); the node ignores itself by comparing its own `node_id` (obtained via a `GET /cluster/v1/health` returning the remote node_id).
- **Health check**: periodic ping (default 5s) to each known peer's `GET /cluster/v1/health`; marks alive/dead with a few attempts of tolerance. Exposes `live_peers()`, `all_peers()` with state and last contact.
- Honest documented limit: mDNS multicast stays within a single L2 subnet and does not cross routers; Docker bridge and many k8s CNIs block it → in those environments use `static`/`dns` (config still identical). For N=3, gossip (chitchat) is overkill; mDNS + health ping suffice. Chitchat remains a future option beyond ~5 nodes.

Membership feeds both the **quorum** (how many live nodes I see) and the **dashboard** (topology).

## Architectural principles (the backbone, unchanged)

- **Dedicated internal transport `/cluster/v1/*`, NOT the public S3 path.** An S3 PUT towards a peer would regenerate `blob_id` and `version_id`, making object identities diverge. The internal path transfers `blob_id`, metadata row and sidecar **verbatim**.
- **Blobs = the easy problem**: immutable (UUID), we replicate the **physical bytes** (already compressed+encrypted) → byte-identical copies on every node. Requires raw access (below) and a **cluster-shared master key**.
- **Metadata = the hard problem**, confined to the `objects` table. Row `(bucket,key,version_id)` immutable; `is_latest` is a derived view recomputed locally. Verbatim replication with idempotent upsert; LWW conflicts on `(last_modified, version_id, blob_id)` — tiebreak on `blob_id`, NOT `write_id` (decision 11). Hard deletes become *tombstone* rows so deletion converges without resurrection (decision 8). Convergence without consensus.
- **Transparent decorators over the traits**: the cluster layer implements `BlobStore`/`MetadataStore`; handlers, `blob_for_write()` and `AppState` stay unchanged.

## Consistency model (one engine, two policies)

Both modes share: discovery, verbatim replication, anti-entropy + read-repair (NO hinted handoff, decision 9), LWW resolution. They differ ONLY in the write-admission gate, in `ClusterMetadataStore::put_object` (the linearization point):

- **`mode="quorum"` (CP, default)**: quorum = `floor(cluster_size/2)+1`. A write is acknowledged only once `quorum` copies (including the local one) are durable. If live nodes `< quorum` → `503 ServiceUnavailable` on writes (the cluster stays read-only). With `cluster_size=3`: 2 live nodes = writable, 1 node = read-only. No divergence possible (only one side of a partition can hold the majority).
- **`mode="available"` (AP)**: the write is acknowledged after the local copy; fan-out to the other live nodes in parallel (best-effort, completed via anti-entropy). Always writable. In a partition both sides write; at heal LWW wins (a conflicting write on the same object is silently lost).

**Reads** are local in both modes (simple), with **read-repair on miss** (if the node lacks the requested blob/version, it fetches them from peers). Strong cross-node read-after-write guaranteed in practice with a sticky LB (hash on credential/IP); an optional read-quorum remains future hardening, not in the default.

## What gets replicated (control plane + data plane)

For real HA, when a client fails over to another node, the same users, credentials, permissions and settings must be there: otherwise auth and authorization would fail. So we replicate **all managed state**, not just objects, as MinIO does with site replication (objects + IAM + bucket metadata). `SqliteStore`/`PgStore` implement all traits on a single DB, but replication is **logical per-entity**, not at the database-file level.

- **Data plane (replicated)**: physical blobs + `objects`/`parts`/`multipart_uploads`/`object_tags` rows (verbatim bytes + LWW rows, described above).
- **Control plane (replicated)** — via idempotent `/cluster/v1/op` operations:
  - `credentials` (access keys) — critical for auth on every node
  - `users`, `teams`+`team_members`, `grants`+`user_grants`+`team_grants` (identity and RBAC)
  - `server_config` (region, retention, log_level, replication credentials, etc.)
  - `buckets`, `bucket_config` (versioning, encryption, compression, lifecycle, replication rules, **notification config**), `bucket_tags`
- **Per-node observability (NOT replicated)** — each node has its own, possibly aggregated externally:
  - `audit_log` and `metrics_snapshot` (the node's local logs and metrics)
  - `notification_events` (event DELIVERY log: the notification CONFIG yes, the LOG no)
  - `presigned_urls` (console tracking; presigned URLs verify by signature, no shared state)
  - **`replication_journal`** (Phase 28 only; the cluster does NOT use hints — decision 9): it is each node's LOCAL delivery state towards peers; replicating it would be wrong. Each node has its own.

The control plane changes rarely and is small: mutations propagate in real time like objects, but anti-entropy reconciles it with a **full snapshot + LWW merge by `updated_at` plus generic tombstones** (decision 12; few rows) instead of the objects' incremental changed-since. Control-plane mutations follow the same `mode` policy (in `quorum` they require the majority; in `available` always).

## Component architecture

Composition stack (inner → outer) in `main.rs`:
```
ClusterMetadataStore( CachingMetadataStore( Sqlite|Pg ) )
ClusterBlobStore( CompressingBlobStore( Encrypting|Fs ) , raw: Arc<FsBlobStore> )
```
The cluster layer sits on top of everything: it replicates already-encoded bytes and already-canonical rows.

## Implementation milestones

### M1 — Identity, discovery, membership (foundations of symmetry) ✅ DONE
- `cluster/identity.rs`: `ensure_node_id(server_config)` (generates+persists a UUID), wiring in `main.rs`.
- `config.rs`: symmetric `[cluster]` section (above). Validation: `cluster_size` present when `mode="quorum"`; non-empty `secret`.
- `cluster/membership.rs`: `MembershipManager` (mDNS via `mdns-sd` + static/dns fallback + health ping). `AppState.cluster: Option<Arc<ClusterContext>>` (node_id, mode, cluster_size, membership, secret-derived credential, `ClusterClient`, raw `Arc<FsBlobStore>`).
- `GET /cluster/v1/health` (returns node_id, state) — public like `/admin/health`.
- CLI `arca cluster status`: prints the local node_id, the node list (alive/dead, last contact), quorum reached yes/no, lag.

*Outcome: nodes discover and monitor each other; no replication yet.*

### M2 — Transport and verbatim reception ✅ DONE (+ full control plane: bucket/credentials/users/grants/teams/server_config/tags/multipart)
- `cluster/client.rs`: `ClusterClient` (sibling of `OutboundClient`), **real streaming** of the body (no `collect_stream` in RAM). Refactor the SigV4 helpers from `replicator/client.rs` into `sigv4_http.rs`. Auth via a credential derived from the shared `secret` (fixed access_key `arca-cluster` + secret), identical everywhere.
- Raw blob access in `blob.rs` + `fs/blob.rs`: `read_raw`/`write_raw` (verbatim bytes, bypassing wrappers) + `exists`. Delegating default impls for non-FS stores.
- Verbatim metadata methods in `metadata.rs` (+ sqlite/pg impls): `apply_remote_object` (upsert by `(bucket,key,version_id)` + LWW guard + `recompute_is_latest`), `apply_remote_version_delete` (today it TOMBSTONES instead of removing — decision 8), `list_rows_changed_since` (in fact added in M4). Default `Unsupported`.
- `recompute_is_latest`: shared pure function (one single place), used by single-node `put_object`, `apply_remote_object`, and the promotion in `delete_object_version`. Tiebreak `(last_modified DESC, version_id DESC, blob_id DESC)`, excluding tombstones (decisions 8, 11).
- **Control plane — idempotent apply**: per-store `apply_remote_*` methods on `CredentialStore`/`UserStore`/`TeamStore`/`GrantStore`/`ServerConfigStore` + bucket (`buckets`/`bucket_config`/`bucket_tags`), applying verbatim upsert/delete by primary key, idempotent. (NOTE: real-time replication remains last-applied-wins verbatim upsert; the true LWW for catch-up is the control-plane reconcile, ✅ DONE in M4 item 5, decision 12.)
- Migration (REVISED, decisions 10–11; in fact done in M4, not M2): (a) on `objects` ONLY the indexed `seq` column (changed-since cursor; sqlite v18 with the dedicated `object_seq` counter, pg 0005 with a `SEQUENCE`) — NO `write_id`, NO `updated_at` on objects (the LWW tiebreak is `blob_id`); `is_tombstone` added in sqlite v19 / pg 0006. (b) `updated_at` on the control-plane tables (`credentials`/`users`/`teams`) for the reconcile LWW → ✅ DONE (sqlite v20 / pg 0007, backfill = created_at, maintained in put/update/apply_remote), sub-chunk (a) of the control-plane reconcile.
- Endpoints + `cluster_auth` middleware + `/cluster/v1/*` routes in `router.rs` (gated on `state.cluster.is_some()`): `PUT /blob/{id}`, `PUT /blob/{id}/sidecar`, `POST /object`, `POST /object/delete`, `POST /op` (control plane: credential/user/team/grant/server_config/bucket), `GET /manifest`, `GET /blob/{id}`.

*Outcome: nodes can receive verbatim replicas; nobody sends them yet.*

### M3 — Write path + consistency policy ✅ DONE
- `cluster/cluster_blob.rs`: `ClusterBlobStore` (impl `BlobStore`). `write_sidecar` (the point where bytes+sidecar both exist): writes locally via inner → reads the raw bytes → fans out to live peers; best-effort, unreachable peers are reconciled by anti-entropy (NOT by hints — decision 9). `delete`: LOCAL only (orphan-blob GC on peers is anti-entropy's job). `get`: local, with fetch-from-peer fallback + repair (read-repair).
- `cluster/cluster_meta.rs`: `ClusterMetadataStore` (impl `MetadataStore`). `put_object`: calls inner (versioning, canonical version_id) → applies the `mode` policy (quorum gate vs available) → fans out the resulting row → ACK. Also replicates delete/delete-marker/tags/lock/bucket-config/bucket create-delete (via idempotent `/cluster/v1/op`).
- `cluster/cluster_control.rs`: decorators for the control-plane stores (`CredentialStore`/`UserStore`/`TeamStore`/`GrantStore`/`ServerConfigStore`) intercepting mutations and replicating them via `/cluster/v1/op` with the common `fan_out_op` helper (same `mode` policy, best-effort fan-out; recovery via anti-entropy, NOT hints — decision 9). Admin handlers unchanged; wrapped in `main.rs` when the cluster is active.
- `mode` gate: `quorum` (majority from `cluster_size` + live nodes from membership; 503 under the threshold) | `available` (always, best-effort fan-out). Applies to data plane and control plane.
- Multipart: `CompleteMultipartUpload` produces a composite blob referencing the parts' `blob_id`s → replicate all parts before/together with the composite sidecar.
- `main.rs` wiring: when `[cluster].enabled`, wrap blob/plain_blob in `ClusterBlobStore` and metadata in `ClusterMetadataStore`, above caching/compression/encryption. Invalidate the local cache on `apply_remote_object`.

*Outcome: working write HA, with the chosen policy.*

### M4 — Resilience and catch-up (revised per decisions 8–12; see status below)

No hinted handoff (decision 9). Self-healing rests on: frequent anti-entropy (changed-since manifest) + read-repair + tombstones.

1. **Changed-since manifest** ✅ DONE (commit `073aeb3`): node-local `seq` (decision 10), `MetadataStore::list_rows_changed_since`, `POST /cluster/v1/manifest` → `ClusterManifest{entries:[{seq,record}],cursor}`, `ClusterClient::fetch_manifest`. (POST, not GET-with-query: the body stays `UNSIGNED-PAYLOAD`, no query-string signing.)
2. **Tombstones** ✅ DONE (commit `f06ffe0`): decision 8. `is_tombstone` on objects (sqlite v19 / pg 0006), hard delete → tombstone in cluster mode, `apply_remote_version_delete` tombstones (idempotent), `purge_tombstones(before)` GC.
3. **Objects anti-entropy worker + tombstone GC** ✅ DONE (commit `56cdca1`): `cluster/anti_entropy.rs`, for each live peer pulls the manifest from `seq=hwm` (in-memory, per-peer) and applies via `apply_remote_object` (idempotent LWW, tombstones included); tombstone GC every tick (`tombstone_grace_days`, default 7). Worker spawned in `main.rs` when `[cluster].enabled`.
4. **`/admin/health?verbose=1`** ✅ DONE (commit `14768d1`): node_id, peers (alive/dead/last contact), quorum, live node count. The default (non-verbose) form is unchanged for the LB.
5. **Control-plane reconcile** ✅ DONE (decision 12): generic tombstones + full-snapshot LWW. Sub-chunks: (a) ✅ `updated_at` as a DB COLUMN on credentials/users/teams (sqlite v20 / pg 0007, backfill = created_at) maintained on writes — NOT in the structs (avoided ~78 literals). (b) ✅ generic `control_tombstones(entity_type, entity_key, deleted_at)` table (sqlite v21 / pg 0008) + `ControlTombstoneStore` trait (record/apply/list/delete/purge, idempotent) on sqlite+pg + tombstone registration on deletes of the **5 primary entities** (credential/user/team/grant/bucket) in the decorators, with symmetric cleanup on local re-creation. (c) ✅ wire types `ControlSnapshot` + `Timestamped{Credential,User,Team}` + `ClusterClient::fetch_control_snapshot` + endpoint `GET /cluster/v1/control-snapshot` (cluster_auth) + `ControlSnapshotStore` trait (build_control_snapshot + apply_credential/user/team_at + apply_control_merge) on sqlite+pg. (d) ✅ pure function `plan_control_merge(local, remote) -> ControlMergePlan` in arca-core (LWW alive-vs-tombstone per key, tested) + per-peer reconcile pass in the anti-entropy worker (identities+tombstones via ControlSnapshotStore, buckets via the cache-aware metadata path) + control-tombstone GC via `purge_control_tombstones`. **SCOPE (my decision, made coherent):** reconcile limited to the 5 tombstoned entities (credential/user/team/grant/bucket); memberships/join-tables, bucket_config/bucket_tags and server_config replicate in real time but are NOT reconciled → documented limitation (catch-up as a follow-up).
6. **Blob repair + GC** ⏳ TO DO: proactive repair of missing blobs (durability; today only read-repair on GET) + GC of orphaned blobs (hygiene). CAUTION: the GC must be aware of multipart composite blobs (a part blob is referenced by the composite sidecar, not by an object row — data-loss risk, TD-014 territory). Slower cadence than the object reconcile (full O(n) scans).

*Outcome: a node that comes back realigns on its own (objects: already; control plane + blobs: with items 5–6).*

### M5 — Console (topology), deploy, docs, tests ⏳ TO DO
- **Topology dashboard** (`console/index.html` ~line 622, `console/js/views/dashboard.js`): make the "Topology" field dynamic (today hardcoded "Single node") → e.g. "Cluster: 3 nodes (2 healthy)". Add a **dedicated bento box** with the node list (node_id, endpoint, alive/dead, last contact, local/remote) and the quorum state. Go through the `frontend-design` skill, replicate the existing views' patterns EXACTLY.
- `GET /admin/cluster`: local node_id, mode, cluster_size, quorum ok, peer list with state and lag. Feeds the dashboard and `arca cluster status`. (An optional `#/cluster` view for the detail, modelled on `replication`.)
- Local deployment: `docker/docker-compose.cluster.yml` (3 services `arca-1/2/3`, separate volumes, **same master key**, same `secret`, `discovery="static"` with the 3 service names as seeds — reliable in Docker where multicast does not pass) + `haproxy` with a check on `/admin/health`. `config/fragments/cluster.toml`. `bin/lib/compose.sh`: `enable_cluster()`. `bin/arca`: `--cluster` flag (mutually exclusive with `--replication`). mDNS tested/documented for bare-metal Linux LANs.
- Production deploy in `deploy/`: HAProxy+keepalived (VIP, `option httpchk GET /admin/health`) and a k8s StatefulSet + headless Service (`discovery="dns"`).
- Documentation (source `documentation/docs/` AND output `docs/`): new `guide/ha.md` (symmetry, discovery, quorum/available model, LB deploy, prerequisites, honest consistency), roadmap (Phase 29 done), configuration page, `CHANGELOG.md`, HTML report. `bin/docs-build`.
- Tests: unit (`recompute_is_latest` determinism; LWW; quorum computation). 3-node compose integration: killing a node mid-write → it carries on; restart of 2-of-3 (1 lost) → writable in `quorum`; 1-of-3 → read-only in `quorum`, writable in `available`; catch-up via anti-entropy; read-after-write on failover. Update the Test Coverage table in `README.md`.

## Deployment prerequisites (to document)

- **Same master key / same KMS** on all nodes (byte-identity of encrypted blobs). SSE-C works (opaque bytes, nonce in the sidecar).
- **Synchronized clocks (NTP)**: LWW compares wall-clock; severe skew can pick the wrong winner on null-version rows. Mitigated by the `blob_id` tiebreak (decision 11); HLC is future hardening.
- **Connectivity** between all nodes on the cluster port; same `secret`. mDNS requires the same L2 subnet (otherwise `static`/`dns`).
- Consistent with "configuration migration without data migration": enabling the cluster on a node with existing data works without migration; anti-entropy populates the peers.

## Out of scope (future phases)

- **Erasure coding** (1.5x storage efficiency) for cold data (`reed-solomon-novelpoly`, Apache 2.0).
- **Sharding / consistent hashing** to scale capacity beyond full replication.
- **Gossip (chitchat)** beyond ~5 nodes; **read-quorum** for strong read-after-write without a sticky LB; **HLC** instead of NTP+LWW.

## End-to-end verification

```bash
# 3-node cluster + HAProxy (static discovery in Docker), dev image, shared master key
bin/arca start -d --build --dev --cluster --encryption

aws s3 cp ./file.bin s3://test/file.bin --endpoint-url http://localhost:9000
bin/arca cluster status        # 3 healthy nodes, quorum ok, every node has blob+row (same blob_id/version_id)

docker stop arca-1             # lose 1 node
aws s3 cp s3://test/file.bin ./out.bin --endpoint-url http://localhost:9000   # read OK
aws s3 cp ./f2.bin s3://test/f2.bin --endpoint-url http://localhost:9000      # mode=quorum, 2 alive = majority → write OK

docker stop arca-2             # 1 node left
# mode="quorum": writes return 503 (read-only, safe); reads OK
# mode="available": writes continue

docker start arca-1 arca-2     # nodes come back and realign via anti-entropy
bin/arca cluster status        # lag → 0

bin/test unit -p arca-storage  # recompute_is_latest, LWW, quorum
bin/test cluster               # HA integration (new target)
```

## Risk notes (where to expect bugs)

1. **`recompute_is_latest` determinism**: a tiebreak disagreement = nodes diverging on "which version is current". One single pure function, used identically everywhere.
2. **Unversioned overwrite + clock skew** (`mode="available"` especially): the only real data-loss surface on concurrent writes. `blob_id` tiebreak (NOT `write_id`, see decision 11) + NTP; recommend versioning for sensitive buckets.
2-bis. **Delete resurrection** (SOLVED, decision 8): without tombstones, anti-entropy resurrects hard deletes. Tombstones prevent it for objects; for the control plane the `control_tombstones` prevent it (decision 12). The residual risk is a node absent LONGER than the grace period: it comes back after the tombstone was GC'd → it could resurrect. Mitigation: ample `*_grace` values (> max expected downtime), documented.
3. **Quorum math vs membership**: the gate must use the LIVE node count from membership, not the configured one; health-check false positives/negatives shift the threshold. Tolerance in the health detection.
4. **Blob/metadata ordering and orphans**: a blob without a row (orphan, to GC) or a row without a blob (read-repair fetch-from-peer). Conservative GC with grace. **Blob GC MUST be aware of multipart composites** (a part blob is referenced by the composite sidecar, not by an object row): a naïve GC would delete live parts (data loss, TD-014 territory).
5. **Streaming of large blobs in the fan-out** without buffering N copies in RAM (replace Phase 28's `collect_stream`).
6. **mDNS in Docker/macOS**: multicast often does not work → the test compose uses `discovery="static"`; mDNS validated on a Linux LAN. Do not promise mDNS where multicast is blocked.
