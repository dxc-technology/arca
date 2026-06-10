# Phase 29 Review — High Availability (v0.24.0 → v0.25.1)

- **Review author**: Claudia (Claude Fable 5), 2026-06-10
- **Subject**: design and implementation of the HA cluster built with Opus 4.8 (Phase 29, ~8,000 lines of new Rust, 9 cluster modules, 7 DB migrations, inter-node SigV4 transport, anti-entropy, console, deploy, 11 integration tests)
- **Method**: static review of the `v0.24.0..v0.25.1` diff across 4 areas (transport/security, anti-entropy/GC, write path/quorum, tests/deploy/docs) carried out by review agents in parallel; **every critical finding was verified by me directly on the code** before entering this document. No chaos tests or load tests were run: the race findings come from code analysis, not from reproduction.

---

## 1. Overall judgment

**The design is excellent; the implementation is good but promises more than it delivers on one central point.**

The plan's architectural decisions 8–12 (tombstones for hard deletes, no hinted handoff, node-local `seq` cursor, `blob_id` tiebreak with no new columns, control-plane reconcile via snapshot + generic tombstones) are all correct and well motivated for an N=3 full-replica cluster. The total-symmetry constraint (byte-identical config, auto-generated `node_id`) is honored with elegance. The separation into testable pure functions (`recompute_is_latest`, `plan_blob_gc`, `plan_control_merge`) is the most significant technical strength: all the convergence logic is deterministic and covered by unit tests.

The main problem is **semantic, not structural**: the `quorum` mode declared as CP is actually an *admission gate* — the write is acknowledged to the client even if the fan-out fails on every peer. The plan's promise ("a write is acknowledged only once quorum copies are durable", "no divergence possible") does not match the code. It is not a fatal defect for the use case (anti-entropy converges anyway), but it must be either fixed or honestly re-documented, because today the guarantee perceived by the user is stronger than the real one.

Summary assessment:

| Aspect | Score | Notes |
|---|---|---|
| Design / architectural decisions | 9/10 | Decisions 8–12 are textbook; scope control right (no hinted handoff, no gossip at N=3) |
| Convergence correctness (LWW, tombstones, GC) | 8/10 | Solid pure functions; two residual resurrection windows (§3.2, §3.3) |
| Declared vs real consistency semantics | 5/10 | Quorum = admission, not durability; the "no divergence" claim is false inside the failure-detection window |
| Inter-node security | 7/10 | Good credential isolation and signing; missing anti-replay window and body limits |
| Integration with the rest of the system | 6/10 | Background workers not cluster-aware (lifecycle, external replication); SSE-C not replicated |
| Tests | 7/10 | Unit tests of the pure functions are excellent; phased integration well done but with gaps (available mode, control-plane catch-up) |
| Operability / docs | 7/10 | `ha.md` honest; missing runbooks (node replacement, resize, backup) and consistency in the HAProxy tunings |

---

## 2. Critical findings (verified directly on the code)

### 2.1 The quorum is an admission gate, not a write quorum — plan/implementation divergence

`cluster_meta.rs:227-238`: `put_object` calls `check_write_quorum()` (a count of the peers *alive according to membership* at admission time), writes locally, then `fan_out_object(...)` which **swallows every error** (`tracing::warn!` and nothing else). The client receives `200 OK` even if zero peers received the copy.

Consequences:

1. **"Ghost" write**: a PUT admitted with 2 live nodes whose fan-out fails exists on a single machine. If that node dies before the next anti-entropy round, the write acknowledged to the client is lost. This contradicts the module comment ("no divergence") and the plan ("acknowledged only once quorum copies are durable").
2. **Divergence inside the failure-detection window even in quorum mode**: right after an A | B,C partition, node A still believes B and C are alive (health ping every 3s with tolerance), so it admits writes that remain local-only; the B,C side writes the same key; at heal LWW wins and one of the two acknowledged writes is silently lost. The "no divergence possible" claim only holds *outside* this window.

**Recommendation (P0)** — two options, to be chosen explicitly:

- *Option A (true quorum)*: count the fan-out ACKs and, if `1 + ack < quorum`, return an error to the client (the local copy remains and anti-entropy propagates it: it must be documented that an error does not imply rollback, as in any quorum system without distributed transactions). This is the fix coherent with the plan and with the "CP" label.
- *Option B (documentation honesty)*: keep the current behavior but rename the guarantee in docs and comments ("admission-gated, best-effort replication, convergence via anti-entropy") and remove the "no divergence" claims. Near-zero cost, but it weakens the value of quorum mode.

Option A is preferable: the fan-out is already synchronous (awaited before the ACK), so the incremental cost is just counting the results instead of discarding them.

### 2.2 `seq` cursor race on PostgreSQL: rows lost by the incremental sync

`pg/metadata.rs` uses `nextval('objects_seq')` inside the write transaction and `list_rows_changed_since` reads `WHERE seq > $1 ORDER BY seq`. PostgreSQL sequences are **not transactional**: if the transaction with `seq=100` is still in flight when another with `seq=101` commits, a peer reading the manifest sees 101, advances its HWM to 101, and row 100 (committed later) **will never be delivered again** by the incremental sync — until the peer restarts (the in-memory HWM restarting from 0, a choice that here turns out to be a fortuitous but costly mitigation).

On SQLite the problem does not exist: the write connection is serialized and the `object_seq` counter lives in the same transaction as the row. The defect affects only the PG backend under concurrent writes.

**Recommendation (P0)**: introduce a safety margin in the PG manifest, e.g. read only up to a `pg_snapshot_xmin`-safe watermark, or `WHERE seq > $1 AND seq <= (max committed seq − guard lag)`; alternatively a commit-order-based cursor (LSN or `committed_at` with grace). Even a simple "do not return the last N rows / the last T milliseconds" closes almost the whole window at minimal cost.

### 2.3 `apply_control_merge`: deletes before tombstones, in separate transactions

`control_merge.rs:47-66`: the loop applies the `delete_credentials/users/teams/grants` and **only afterwards** the `adopt_tombstones`, each call in its own transaction. A crash (or a concurrent snapshot) between the delete and the tombstone adoption leaves the node without the row and without the tombstone: on the next round a peer that still has the live row re-creates it. For a **revoked credential** this is a security hole, exactly what decision 12 wanted to avoid.

**Recommendation (P0, trivial fix)**: invert the order — adopt the tombstones *before* executing the deletes (or wrap delete+tombstone of the same entity in a single transaction). With tombstone-first, a mid-way crash leaves a safe state (tombstone present, row still alive → LWW converges to the tombstone on the next round).

### 2.4 Sequential fan-out, not parallel as planned

All the fan-outs (`cluster_meta.rs:99,114,148`, `cluster_blob.rs:77,107`, `cluster_control.rs`) are `for endpoint in self.live_peers() { ....await }`: peers are contacted **one at a time**. The plan called for "fan-out to the other live nodes in parallel". With 2 peers, every PUT pays 2 sequential round-trips (blob + row, so 4 in total); a slow peer slows everything down, and the cluster client timeout adds up per peer in the worst case.

**Recommendation (P1)**: `futures::future::join_all` (or `FuturesUnordered`) over the peers. It is also the natural prerequisite of the ACK counting in §2.1's option A.

---

## 3. High-severity findings

### 3.1 No anti-replay window in the inter-node auth

`cluster_auth.rs:138-146` extracts `x-amz-date` but never compares it with the clock: a captured inter-node request (e.g. a `PUT /cluster/v1/blob`) is **replayable forever**. On the public S3 surface the omission is debatable but tolerated; on an internal protocol the server can and must enforce the standard ±15 minutes window. Combined with TD-015 (inter-node TLS accepting invalid certs), a MITM on the cluster network can record and replay writes. Fix: about ten lines in the middleware. **(P1, to be considered together with TD-015)**

### 3.2 Tombstone GC blind to peer liveness

`anti_entropy.rs:119-134`: `purge_tombstones`/`purge_control_tombstones` run on every tick on a wall-clock basis, even if a peer has been down longer than `tombstone_grace_days`. Scenario: node down for 8 days with a 7-day grace → on re-entry its manifest resurrects the deleted objects. The risk is documented in the plan (note 2-bis), but the code offers neither a guard nor an alarm. Fix: before purging, verify that every configured peer has been seen within the grace window; otherwise skip the GC and log a highly visible warning. It turns a silent policy violation into an operational event. **(P1)**

### 3.3 Background workers not cluster-aware: duplicated work on every node

- **Lifecycle worker** (`worker.rs`): runs on every node with the *cluster-decorated* stores → every node scans, deletes and fans out the same expirations. Idempotent thanks to the tombstones, but delete traffic ×N², double local `blob.delete`s and races between concurrent "deleters".
- **Replication worker (Phase 28, external S3 destinations)**: every node has its own journal and tries to replicate the same objects to the same destination → **duplicate deliveries to the external bucket**.

Suggested fix: a trivial, symmetric leader gate (only the live node with the lowest `node_id` runs the worker), consistent with the "simple at N=3" philosophy. To be treated as an extension of TD-016 or a new TD. **(P1)**

### 3.4 Cluster endpoints without a dedicated body limit

`handlers/cluster.rs`: `receive_object` and `receive_op` extract `Bytes` under the global limit (5 GB, the S3 limit). The legitimate payload is a few KB: a compromised peer (or anyone holding the secret) can make the node buffer gigabytes of RAM per request. Fix: a `DefaultBodyLimit` of ~1 MiB on the `/cluster/v1/{object,op,...}` sub-router. **(P1)**

### 3.5 Unauthenticated `/cluster/v1/health` exposes disk stats and config fingerprint

The `node_id` is needed for discovery, but `disk_total`, `disk_available` and `config_fingerprint` are useful information for an unauthenticated attacker (residual capacity for targeted DoS, confirmation of config drift). Move them to an authenticated response (the authenticated manifest/health already exists) leaving only the `node_id` public. **(P2)**

### 3.6 SSE-C not replicated

`main.rs:166`: `ssec_blob` is not wrapped by `ClusterBlobStore` → SSE-C objects are written locally only and do not survive the loss of the node, silently. If SSE-C + cluster is a supported scenario it must be fixed; otherwise it must be **documented in `ha.md` and tracked as a TD** (today it is written nowhere). **(P1 as tracking, P2 as fix)**

---

## 4. Medium-severity findings

| # | Where | Problem | Suggested fix |
|---|---|---|---|
| M1 | `anti_entropy.rs:361-371` | A manifest row that fails *persistently* in `apply_remote_object` blocks that peer's HWM forever (liveness, not data-loss; it only unblocks on restart) | After K consecutive failures on the same `seq`, skip with a warning and evidence in `/admin/cluster` |
| M2 | `anti_entropy.rs:154-206` | `repair_blobs` materializes *all* referenced blob_ids in RAM and repairs sequentially with no budget: on a large node it blocks the worker for hours | Per-tick budget (e.g. 100 blobs) + resume cursor |
| M3 | `membership.rs:84-161` | The `known` map grows without eviction: with dynamic mDNS/DNS (changing IPs) it accumulates dead endpoints probed on every tick | Pruning after N consecutive unreachable ticks |
| M4 | `cluster_meta.rs:88-93` | The 503 from missing quorum carries no `Retry-After` (the rate limiter does) | Add `Retry-After: 5` |
| M5 | `config.rs:800` | `secret` accepted even at 1 character | Startup error below 16 characters |
| M6 | `handlers/cluster.rs:79-115` | Path `blob_id` not validated as a UUID before `write_raw` (defense in depth, the path is signed) | `Uuid::parse_str` at ingress |
| M7 | sqlite `apply_remote_object` | Re-delivery of an identical row (idempotent, correct) still stamps a fresh `seq` → manifest churn between peers | No-op when the incoming row is byte-equal to the existing one |
| M8 | `fs/blob.rs:703-708` | Unreadable mtime → silent fallback to `now()`: the safe direction for the GC, but on a filesystem without mtime the GC will never collect anything, with no log at all | `tracing::warn!` on the fallback |

---

## 5. Tests, deploy and documentation

**Coverage gaps (in order of importance):**

1. **No end-to-end test of `available` mode** (the whole suite runs in `quorum`): the "1/3 nodes and still writable" branch and the LWW convergence of concurrent writes are never exercised. It was already marked optional in the plan; for me it is the first test to add.
2. **No control-plane catch-up test**: phase D only verifies objects. It is enough to create a bucket and a credential via the LB while arca-3 is down and verify their presence on arca-3 after re-entry.
3. **Latent flakiness**: `bin/test:315` waits for one node's *membership* convergence and sleeps 2s, but HAProxy with `fall 2 inter 2s` takes up to ~4s to remove a node from rotation; phase E has a poll condition (`disk_available_bytes` cluster-min) that can trigger before arca-3's state is incorporated. Fix: sleep ≥ 5s (or poll the LB) and an extra check that arca-3 shows alive in the topology before proceeding.
4. Not tested (acceptable to defer, but to be listed in a TD/test-debt): tombstone GC, blob repair, bootstrap of a replaced node with an empty disk, clock skew.

**Deploy:**

- Three different `fall/rise` tunings across `docker/cluster/haproxy.cfg` (2/1), `deploy/haproxy/haproxy.cfg` (3/2) and the snippet in `ha.md`: standardize or motivate the difference.
- K8s: readiness probe without explicit `failureThreshold`/`periodSeconds` → up to 30s of traffic towards a crashed pod. Suggested `periodSeconds: 5, failureThreshold: 2`.

**Documentation (`ha.md` is honest overall, three things are missing):**

1. An explicit sentence about `available` mode: *concurrent writes to the same key are both acknowledged but only the LWW winner survives; the loser is discarded with no error to the client*. "They diverge and then converge" is technically true but understates the consequence.
2. An **operational runbook**: replacing a dead node (empty disk: safe, resurrects nothing — but the operator does not know that), growing the cluster (changing `cluster_size` 3→4 changes the quorum and is a delicate operation, with drift detection firing during the rollout), backup strategy (safe sequence with SQLite WAL).
3. The SSE-C limitation (§3.6) and the real quorum semantics (whatever the outcome of §2.1).

---

## 6. What is done very well

It must be said with the same clarity as the defects:

- **Decisions 8–12 are the heart of the value** and they are all right: tombstones as the correction of the original plan's resurrection bug, dropping hinted handoff (correctly motivated: at N=3 with an incremental manifest it is redundant), the node-local `seq` with a dedicated counter (on SQLite it is *truly* race-free, complete with a comment explaining why `MAX(seq)+1` would be wrong), the `blob_id` tiebreak avoiding a migration, the snapshot+tombstones for the control plane instead of an op-log.
- **Pure functions for all the dangerous logic**: `recompute_is_latest` (one single implementation shared by the three call sites, as per the plan's risk note 1), `plan_blob_gc`, `plan_control_merge` — all with unit tests covering the LWW cases and the composite case.
- **Fail-safe, composite-aware blob GC**: an enumeration error = the whole round skipped (never delete on a partial set); the parts of a referenced composite are protected; dedicated test.
- **Correct security basics**: the cluster credential never in the `CredentialStore` (no escalation towards S3/admin), constant-time signature comparison, the sidecar (carrying the wrapped DEK) included in the signed headers, the loop-prevention header signed, the secret used directly as the SigV4 key (correct: SigV4's HMAC chain is already a KDF).
- **Symmetry is maintained for real**: no `node_id` in config, identity persisted in `server_config`, fingerprint drift detection with logs only on transitions.
- **Phased integration tests** with shell-driven node stop/start orchestration and `--no-deps` to avoid resurrections via `depends_on`: a non-trivial pattern, solved well.
- **Cache coherence**: `CachingMetadataStore` implements all four `apply_remote_*` with invalidation; the merge's buckets go through the cache-aware path, with a comment explaining why.
- **Documentation honesty**: mDNS limits, the NTP constraint, the `cluster_size` constraint, TD-014/015/016 tracked instead of hidden.

---

## 7. Recommended action plan

| Priority | Action | Estimated effort |
|---|---|---|
| **P0** | §2.3 — Tombstone-first in `apply_control_merge` (loop reordering) | Minutes |
| **P0** | §2.1 — Decide true quorum (count ACKs, error below threshold) vs re-documentation; I recommend true quorum | 1 day with tests |
| **P0** | §2.2 — Safety margin in the PG manifest (or a commit-safe cursor) | Half a day |
| **P1** | §2.4 — Parallel fan-out (`join_all`), prerequisite of the ACK counting | Hours |
| **P1** | §3.1 — ±15 min anti-replay window in `cluster_auth` (evaluate together with TD-015) | Hours |
| **P1** | §3.2 — Liveness guard on the tombstone GC + warning | Hours |
| **P1** | §3.3 — Leader gate for the lifecycle and replication workers (live node with the lowest `node_id`) | 1 day |
| **P1** | §3.4 — 1 MiB body limit on the `/cluster/v1/*` JSON endpoints | Minutes |
| **P1** | §3.6 — Track the SSE-C non-replication as a TD + note in `ha.md` | Minutes |
| **P2** | §5 — `available` mode test + control-plane catch-up; fix phase E / `wait_live` flakiness | 1 day |
| **P2** | §5 — Operational runbook in `ha.md` (node replacement, resize, backup) + available LWW note | Half a day |
| **P2** | M1–M8, §3.5, HAProxy coherence, k8s probes | Scattered, all small |

---

## 8. Conclusion

Phase 29 is high-level work: the distributed design is sober and correctly sized for the use case (N=3, full replication, no external coordination), the classic traps (delete resurrection, composite GC, tiebreak determinism) were identified *during* the implementation and solved with clean solutions, and the dangerous logic is confined to tested pure functions. The real defects concentrate in three places: **quorum semantics weaker than promised** (the most important gap, because it concerns the guarantee the user buys by choosing `mode="quorum"`), **two residual resurrection windows** (delete/tombstone ordering in the control merge, GC blind to liveness) and **incomplete integration with the pre-existing subsystems** (duplicated background workers, SSE-C not replicated). All have proportionate fixes and none requires rethinking the architecture: with the three P0s closed, the quorum mode's "CP" label becomes defensible and the system honestly is what it declares to be.
