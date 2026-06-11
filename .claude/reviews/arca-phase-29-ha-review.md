# Phase 29 Review — High Availability (v0.24.0 → v0.25.1)

- **Review author**: Claudia (Claude Fable 5), 2026-06-10
- **Subject**: design and implementation of the HA cluster built with Opus 4.8 (Phase 29, ~8,000 lines of new Rust, 9 cluster modules, 7 DB migrations, inter-node SigV4 transport, anti-entropy, console, deploy, 11 integration tests)
- **Method**: static review of the `v0.24.0..v0.25.1` diff across 4 areas (transport/security, anti-entropy/GC, write path/quorum, tests/deploy/docs) carried out by review agents in parallel; **every critical finding was verified by me directly on the code** before entering this document. No chaos tests or load tests were run: the race findings come from code analysis, not from reproduction.
- **Update 2026-06-11**: second pass dedicated to the plan itself as a design document (new §6, findings D1–D12), with every claim verified on the code or the artifacts; the following sections have been renumbered (7–9).

---

## 1. Overall judgment

**The design is excellent; the implementation is good but promises more than it delivers on one central point.**

The plan's architectural decisions 8–12 (tombstones for hard deletes, no hinted handoff, node-local `seq` cursor, `blob_id` tiebreak with no new columns, control-plane reconcile via snapshot + generic tombstones) are all correct and well motivated for an N=3 full-replica cluster. The total-symmetry constraint (byte-identical config, auto-generated `node_id`) is honored with elegance. The separation into testable pure functions (`recompute_is_latest`, `plan_blob_gc`, `plan_control_merge`) is the most significant technical strength: all the convergence logic is deterministic and covered by unit tests.

The main problem is **semantic, not structural**: the `quorum` mode declared as CP is actually an *admission gate* — the write is acknowledged to the client even if the fan-out fails on every peer. The plan's promise ("a write is acknowledged only once quorum copies are durable", "no divergence possible") does not match the code. It is not a fatal defect for the use case (anti-entropy converges anyway), but it must be either fixed or honestly re-documented, because today the guarantee perceived by the user is stronger than the real one.

**Update 2026-06-11.** A second pass dedicated to the plan as a design document (§6, findings D1–D12) refines the judgment: the decisions that were taken remain excellent, but the plan errs by omission. It does not design the failure paths of the guarantees it declares (the root of §2.1), it does not cover the operational lifecycle of the cluster (resize, secret rotation, restore from backup), it does not audit the pre-existing subsystems against N-node execution (the root of §3.3 and §3.6), and it never verifies a real partition. The dedicated assessment row is in the table.

Summary assessment:

| Aspect | Score | Notes |
|---|---|---|
| Design / architectural decisions | 9/10 | Decisions 8–12 are textbook; scope control right (no hinted handoff, no gossip at N=3) |
| Completeness of the plan as a design document | 6/10 | Failure paths, operational lifecycle and subsystem audit missing (§6, D1–D12) |
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

## 6. Design issues of the plan (pass of 2026-06-11)

This section evaluates the plan itself as a design document, complementary to the plan/implementation divergences of §2–§3. Every finding was verified on the code or the artifacts before entering here (the lines cited as "plan, line N" refer to `.claude/plans/arca-phase-29-ha.md`). The general theme: what the plan designs is right (decisions 8–12 remain textbook), but the plan systematically errs by omission, in four directions:

1. **It declares guarantees without designing their failure paths.** "A write is acknowledged only once quorum copies are durable" (line 89) is never accompanied by the mechanism (ACK counting? error semantics towards the client? what remains on the local node?): §2.1 is born exactly in this void, which the implementation filled with the weaker admission gate.
2. **It does not cover the operational lifecycle of the cluster** (node re-entry, resize, secret rotation, restore from backup): D2, D3.
3. **It does not audit the pre-existing subsystems against N-node execution**: the plan lists what NOT to replicate, but never what must not run N times (the root of §3.3), and it declares "SSE-C works" without designing its wiring (the root of §3.6).
4. **The verification strategy never exercises a real partition** (D8): all the phases are crash-stop.

At the end of the section, for honesty, the suspicions that were verified and disproved: in some places the implementation does better than the plan.

### D1. The quorum counts an unauthenticated liveness and ignores drift ("ghost quorum")

The liveness feeding the quorum gate is an **unauthenticated** `GET /cluster/v1/health` (`cluster_public` router, `router.rs:223`; ping in `membership.rs:103`); `has_write_quorum` counts only the `alive` peers (`cluster.rs:775-799`) and the `config_ok` flag is recorded (`membership.rs:135`) but **never used in the gate**: config drift is purely informational. mDNS adds candidates with a mere string comparison on `cluster_id`, with no proof of possession of the secret (`membership.rs:217-224`).

Consequence: a peer with the wrong secret or a diverged config answers the ping, **counts toward the quorum**, but rejects 100% of the replications: writes admitted "with a majority" that exist on a single node, **persistently**, not just inside §2.1's failure-detection window. The scenario already exists in the suite (phase F: arca-3 with a different secret) but the test only checks the `config_aligned` flag. On the LAN, any process exposing that path with the same `cluster_id` inflates the count.

Side note (a plan divergence not caught in §2–3): the plan promised "a few attempts" of tolerance in the health check (line 73); the implementation marks dead at the **first** error (`membership.rs:104`). A conservative direction for the quorum, but in an already-degraded state (one node really down) a single lost ping towards the remaining peer produces up to ~5 s of spurious read-only.

**Recommendation (P1)**: liveness based on an authenticated exchange (signed ping or an endpoint under `cluster_auth`) and `config_ok = false` excluding from the quorum count; leave the public `/cluster/v1/health` with only the `node_id` needed for discovery (combines with §3.5). The ACK counting of §2.1-A covers the write side almost for free: a peer that does not authenticate never ACKs.

### D2. No synchronization state: a realigning node serves 404s and partial listings

The plan never defined a "syncing" state: the read-repair on miss (line 92) is blob-level only. In the code: `get_object` delegates to the inner store with no peer fallback (`cluster_meta.rs:243-249`), read-repair lives only in `ClusterBlobStore::get` (`cluster_blob.rs:149-164`); `/admin/health` answers 200 from the very first instant without reflecting either quorum or sync state (`admin.rs:144-165`) and it is exactly the path of the k8s probes (`arca-cluster.yaml:161-172`); the anti-entropy worker tracks no "first pass completed".

Scenario: a node replaced with an empty disk (or returning from a long downtime) enters the LB rotation and the quorum **immediately**. Until the metadata pull completes, a GET of a key whose row has not arrived yet answers **404 NoSuchKey** and LISTs return **partial listings**, on ~1/N of the LB-routed traffic. Rows arrive relatively fast (manifest paginated at 500 to exhaustion within the tick, `anti_entropy.rs:360-371`) and blobs follow on-demand via read-repair, so the window ranges from minutes to tens of minutes on large stores: but it is a window in which the node gives wrong answers with full confidence, and nothing signals it.

**Recommendation (P1)**: a readiness gate: the health used by LB/k8s answers non-200 until the first reconcile pass towards every live peer has completed after startup; expose the per-peer lag in `/admin/cluster`. Minimal mitigation: document the re-entry procedure (warm-up with the node out of the LB).

### D3. The cluster lifecycle is not designed (resize, secret rotation, restore from backup)

The plan covers the birth and the failure of a node, not the operational transitions. It is in tension with the project's architectural constraint ("configuration migration without data migration": every configuration change must have a migration-free path), which is not honored for the `[cluster]` section:

- **(a) Resize and wrong `cluster_size`.** The quorum derives from the config and no guard compares the observed nodes with `cluster_size` (`check_write_quorum` only compares alive vs threshold, `cluster.rs:775-799`). With 4 nodes and `cluster_size = 3` the config is identical everywhere, so the **drift detection does not fire** (same fingerprint): two partitioned pairs both hold quorum 2, i.e. split-brain precisely in the mode that promises to exclude it. A rolling resize 3→5 produces mixed thresholds by construction.
- **(b) Secret rotation.** No dual-secret window: changing the secret requires stopping the whole cluster (downtime in an HA product). During a rolling restart, nodes with different secrets count each other toward the quorum (D1) while being unable to replicate anything.
- **(c) Restore from backup.** The `seq` rewinds but the `node_id` stays the same (it is in the restored DB), and the peers keep the HWM **in memory, keyed by `node_id`** (`anti_entropy.rs:74,91`): the restored node's post-restore writes stay invisible to the incremental sync until the peers are restarted. No documented rule (for example: after a restore, regenerate the `node_id` or restart the peers).

**Recommendation**: P1 for guard (a) (a strong alarm or a refusal when the known nodes exceed `cluster_size`: a few lines); P2 for the dual-secret (accept the old and the new secret inbound during the rotation) and for the restore/resize runbook (extends the one already requested in §5).

### D4. Multipart: real-time replication is there (beyond the plan), but with no reconciliation and local-only assembly

Here the implementation exceeds the plan (which only had one line about ordering at Complete): create, part row and abort replicate synchronously under the quorum gate via `ControlOp::{MultipartCreate, PartUpsert, MultipartDelete}` (`cluster_meta.rs:340-385`) and the part blob fans out at `write_sidecar` (`handlers/multipart.rs:201` → `cluster_blob.rs:171-177`). Two design holes remain, though:

1. **No reconciliation for `multipart_uploads`/`parts`**: the manifest covers only `objects`, the control-plane snapshot only the 5 entities (TECHDEBT in `anti_entropy.rs:312-316`). A node down during a multipart, on re-entry, answers `NoSuchUpload` to the UploadPart/ListParts/Complete the round-robin LB routes to it (an intermittent error until the upload closes elsewhere); a **missed abort** leaves orphaned rows and part blobs on that peer forever if no abort-incomplete lifecycle rule is configured, and the GC protects them precisely because they are referenced by in-progress rows.
2. **`concat` is explicitly local, with no part read-repair** (`cluster_blob.rs:179-186`): a Complete served by a node that missed a part's fan-out (errors swallowed, §2.1) fails until the proactive blob repair (every 10 ticks, ~5 minutes) fixes it.

**Recommendation (P2)**: include `multipart_uploads`/`parts` in the reconcile scope (a natural extension of TD-016) and give `concat` a fetch-from-peer fallback for missing parts (the client already exists).

### D5. The LB does not distinguish "down" from "not writable": client-visible 503s silently accepted

Both reference `haproxy.cfg`s declare the intended behavior in their comments: a node that lost the quorum stays in rotation and "writes return 503 from the node itself — the intended behaviour" (`docker/cluster/haproxy.cfg:3-5`, `deploy/haproxy/haproxy.cfg:4-7`), `balance roundrobin`, no retry on 5xx responses, and the quorum 503 carries no `Retry-After` (M4). Keeping the read-only node in rotation to serve reads is defensible, but the cost is declared nowhere: in an asymmetric partition a ~1/N fraction of the writes fails at the client. In practice the AWS SDKs mask it (retry on 5xx by default, and the retried, re-balanced request hits a writable node); clients without retries do not.

**Recommendation (P2)**: document behavior and mitigation in `ha.md` (SDK retries; a sticky/cookie option commented in the reference cfgs); evaluate a write-aware health check variant for those who want a separate write pool.

### D6. Per-node observability behind a round-robin LB: incoherent console views

The plan keeps audit log, metrics and event log per-node (the right call, lines 104-108) and in the same M5 points the console at the LB (`docker-compose.cluster.yml`: `ARCA_ENDPOINT=http://arca-lb:9000`). With `balance roundrobin` every console request can land on a different node: the audit/metrics/events views show **a different node at every refresh**, with no indication of which one. The plan never asked itself how an operator consults per-node data through a balanced entry point.

**Recommendation (P2)**: a node selector in the console for the per-node views (the endpoints are already known via `/admin/cluster`), or aggregation on the admin API side; minimal mitigation: show the source `node_id` in the view and document it.

### D7. The read story in quorum mode is weaker than the CP label (doc)

Reads are always local (R = 1) by design (line 92). With W = majority and R = 1 a read does not necessarily intersect the write quorum: quorum mode (once §2.1 is fixed) prevents divergent writes, it does **not** guarantee up-to-date reads. `ha.md:91` says a read through the LB "may briefly hit a node that has not yet received the write": true in normal operation (synchronous fan-out), false in a partition, where a minority node stays in rotation (D5) and serves stale reads for **the whole duration of the partition**, even in quorum mode. The plan recommended a sticky LB for read-your-writes (line 92), but all three shipped artifacts (the two `haproxy.cfg`s and the `ha.md` snippet) are round-robin.

**Recommendation (P2, doc)**: clarify in `ha.md` the real window (partition, not just fan-out lag) and that CP here means "no conflicting writes", not read linearizability; promote the sticky example from a note to a commented reference configuration in the cfgs.

### D8. The verification plan never exercises a real partition

All 6 phases of `bin/test cluster` use container stop/start (crash-stop). A **network partition** (both sides alive) is the case the two modes exist for, and it is never exercised: no available-mode split-brain with LWW merge at heal, no minority side in quorum mode with both processes alive, no clock skew, no concurrent writes to the same key. For a system whose declared value is partition behavior, it is the most important validation gap (more than the ones already listed in §5).

**Recommendation (P2)**: a phase with `docker network disconnect`/`connect`: in available, writes to the same key from both sides, heal, verify that LWW wins and the loser disappears with no errors (it also covers the doc sentence requested in §5); in quorum, verify the 503 on the minority side with both sides alive.

### D9. TD-016 is underestimated: for the RBAC join tables it is a security problem, for `bucket_config` an integrity one

The reconcile scope (plan line 161, TD-016) excludes the join tables and `bucket_config` with the rationale "they replicate in real time, catch-up as a follow-up". The consequence analysis is missing, though, and it is severe: a **revocation** of a `user_grant`/`team_member` that happened while a node was down is **never reconciled**: the user keeps the privilege on that node forever, and the LB routes ~1/N of their requests there (a revoked privilege that intermittently keeps working is also very hard to diagnose). A diverged `bucket_config` makes the same bucket behave differently per node: the node believing the bucket is unversioned hard-deletes where the others create delete markers.

**Recommendation (P1)**: raise the priority of TD-016 at least for `user_grants`/`team_members`/`team_grants` and `bucket_config` (the entities with security and integrity consequences); credentials, users and teams are already covered by the current reconcile.

### D10. Object Lock / WORM in the cluster: trust model not discussed (P3, doc)

Remote applies are verbatim and do not re-run the lock enforcement (correct between trusted peers: the origin already enforced it). It means, however, that compliance-mode immutability now rests on the shared secret and on the integrity of **every** node: whoever holds the secret can tombstone a locked object across the whole cluster via `/cluster/v1`. Neither the plan nor `ha.md` mention it; TD-015 (inter-node TLS accepting invalid certs) worsens the picture. To be added to the guide's threat model.

### D11. Durability in available mode not declared (P3, doc)

In available mode the ACK arrives with only the local copy durable (line 90): with all peers down, single-copy acknowledged writes pile up, and losing that node's disk loses them permanently (RPO > 0). Plan and guide describe the "LWW conflict" risk but never the single-copy durability window. One line in `ha.md`.

### D12. Minor nits

- The receive side of the `server_config` ops does not filter node-local keys (`handlers/cluster.rs:275-279`): the denylist lives only in the sender. Zero-cost defense in depth, same spirit as M6.
- The admin export includes the whole `server_config`, `node_id` included (`admin_export.rs:138-145`; `list_server_config` filters nothing), and the import rewrites it **locally** at runtime (`admin_import.rs:99`: the decorator's denylist only skips the fan-out, not the local write, `cluster_control.rs:640-641`). Importing an export onto a cluster node duplicates another node's `node_id` (discovery self-skip, colliding HWM and loop-prevention). Exclude `node_id` from export/import.
- Single-failure health detector (`membership.rs:104`) against the "a few attempts" of tolerance promised by the plan (line 73): one lost ping = up to ~5 s of spurious read-only in quorum mode. Being conservative is fine, but 2-3 attempts were the declared design.
- Forming a cluster from **two pre-existing non-empty single-nodes** is not covered: today it is an implicit union+LWW merge of all state (with master keys that must already match for encrypted blobs). To be documented or explicitly discouraged.

### Suspicions verified and disproved

For completeness, the design suspicions that were verified and **disproved** (the implementation does better than the plan):

- **Real-time multipart**: the plan only mentioned ordering at Complete; the implementation replicates create/parts/abort synchronously under the gate (see D4).
- **`node_id` in the replicated `server_config` table**: I feared the non-replication depended only on wiring order (`ensure_node_id` in `main.rs:93`, decoration in `main.rs:419-423`); there is instead an explicit `is_node_local_key` denylist in the decorator (`cluster_control.rs:604-606,640-641,658-659`).
- **Drift detection**: the fingerprint includes the master key id besides `cluster_id`/`mode`/quorum/secret (`cluster.rs:56-78`), also covering the "same master key" prerequisite.
- **Manifest paginated within the tick** (500 per request to exhaustion, `anti_entropy.rs:360-371`): catch-up is network-bound, not worker-cadence-bound.

---

## 7. What is done very well

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

## 8. Recommended action plan

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
| **P1** | D1 — Authenticated liveness + `config_ok` in the quorum count | Hours |
| **P1** | D2 — "Syncing" state in health/readiness + per-peer lag in `/admin/cluster` | 1 day |
| **P1** | D3a — "Observed nodes > cluster_size" guard (alarm or refusal) | Hours |
| **P1** | D9 — Extend the reconcile to user_grants/team_members/bucket_config (TD-016 reprioritization) | 1–2 days |
| **P2** | §5 — `available` mode test + control-plane catch-up; fix phase E / `wait_live` flakiness | 1 day |
| **P2** | §5 — Operational runbook in `ha.md` (node replacement, resize, backup) + available LWW note | Half a day |
| **P2** | M1–M8, §3.5, HAProxy coherence, k8s probes | Scattered, all small |
| **P2** | D4 — Multipart reconcile + `concat` with fetch-from-peer of missing parts | 1 day |
| **P2** | D3b/D3c — Dual-secret rotation; restore-from-backup and resize runbook | 1 day + doc |
| **P2** | D5/D7 — LB doc (SDK retries, reference sticky) + real read semantics in quorum | Half a day |
| **P2** | D6 — Node selector in the console for audit/metrics/events | 1 day |
| **P2** | D8 — Test phase with a real network partition (available split-brain, quorum minority) | 1 day |
| **P3** | D10, D11, D12 — doc notes (WORM, available RPO) and nits (receive-side filter, node_id export/import, health tolerance) | Scattered, small |

---

## 9. Conclusion

Phase 29 is high-level work: the distributed design is sober and correctly sized for the use case (N=3, full replication, no external coordination), the classic traps (delete resurrection, composite GC, tiebreak determinism) were identified *during* the implementation and solved with clean solutions, and the dangerous logic is confined to tested pure functions. The real defects concentrate in three places: **quorum semantics weaker than promised** (the most important gap, because it concerns the guarantee the user buys by choosing `mode="quorum"`), **two residual resurrection windows** (delete/tombstone ordering in the control merge, GC blind to liveness) and **incomplete integration with the pre-existing subsystems** (duplicated background workers, SSE-C not replicated). All have proportionate fixes and none requires rethinking the architecture: with the three P0s closed, the quorum mode's "CP" label becomes defensible and the system honestly is what it declares to be.

The 2026-06-11 pass over the plan (§6) completes the picture without overturning it: the plan's defects are **omissions**, not wrong choices, and two of the most serious implementation gaps are born exactly there (§2.1 from a guarantee declared without a failure path; §3.3/§3.6 from the absence of an audit of the pre-existing subsystems). The most relevant new themes that emerged by looking at the plan: the ghost quorum on unauthenticated liveness (D1), the absence of a synchronization state for a re-entering node (D2), the undesigned cluster lifecycle (D3) and the underestimation of TD-016's security consequences (D9). Lesson for the next phase of this magnitude: for every declared guarantee, the plan must specify the behavior under partial failure; and it must include an operations section (resize, secret rotation, restore) and an integration-with-the-existing section, because that is where this review found almost all the holes.
