# Arca — HA Hardening (remediation after the Phase 29 review)

## Context

The Phase 29 review ([`arca-phase-29-ha-review.md`](https://github.com/dxc-technology/arca/blob/main/.claude/reviews/arca-phase-29-ha-review.md), passes of 2026-06-10 and 2026-06-11) produced the findings: **3 P0s** (§2.1–§2.3), **§2.4 + 9 P1s** (§3.x, D1, D2, D3a, D9), **a series of P2s** (M1–M8, §5, D3b/c, D4–D8) and **P3/doc** (D10–D12). This plan implements ALL of them, organized in 9 milestones (R1–R9) ordered by priority and technical dependency. It is a living document, published on the documentation site as an annex of the roadmap (Phase 29.1) via a symlink to the canonical file `.claude/plans/arca-phase-29-ha-hardening.md`.

**How to use this document** (process rules, valid for every session):

1. At session start read: `CLAUDE.md`, this plan, the review. The traceability table at the bottom says what is done and what is not.
2. Work TDD; every milestone must leave `bin/test unit` and `bin/test cluster` green. After code changes also rebuild the test images (`docker compose -f docker/docker-compose.yml build unit-test test`), otherwise tests run on stale images.
3. For every completed item: tick the checkbox HERE and in the traceability table, update `CHANGELOG.md` (Unreleased section), update the documentation touched.
4. If a decision changes the design, update the WHOLE document, not just the touched section (same rule as the Phase 29 plan).
5. At the end of each milestone: tick the milestone in the roadmap's Phase 29.1 section (`documentation/docs/roadmap.md`), rebuild the site with `bin/docs-build` (this plan is published there via symlink), then propose to Pietro a commit + possibly a release.
6. The §x.y / Mx / Dx numbers refer to the review; the "plan lines" to `.claude/plans/arca-phase-29-ha.md`.

## Design decisions (fixed before implementation)

| # | Decision | Status |
|---|---|---|
| H1 | **True quorum = option A of review §2.1**: count the fan-out ACKs; if `1 + ack < write_quorum` the write fails with `503 ServiceUnavailable` + `Retry-After`. The local copy is NOT rolled back (anti-entropy propagates it): document that an error does not imply undo, as in every quorum system without distributed transactions. | ✅ approved (review + Pietro's ok on the P0s) |
| H2 | **ACK = row applied + blob present**: the response of `POST /cluster/v1/object` becomes `{applied, has_blob}`; the peer self-certifies (sidecar present for rows referencing a blob; for delete markers/tombstones `applied` suffices). Avoids threading state between `write_sidecar` and `put_object`. For composites `has_blob` = composite sidecar present (the parts already fan out individually; the rest is covered by D4). | ✅ decided in planning |
| H3 | **PG commit-ordered cursor via counter table** (like SQLite): table `object_seq(v BIGINT)` updated with `UPDATE ... RETURNING` in the same transaction as the row. The row lock serializes the assignment until commit → seq order = commit order, no deliverable gaps. Cost: serializes the final stretch of object writes on PG (acceptable; documented). Discarded alternative: a probabilistic guard window. | ✅ decided in planning |
| H4 | **Scope of the ACK counting**: object data-plane mutations (put_object, version-delete/tombstone, delete marker). Control-plane and tag ops remain best-effort fan-out + reconcile (rare mutations, reconciled; extension possible later). | ✅ decided in planning |
| H5 | **Symmetric leader gate for the workers**: only the ALIVE node with the lowest `node_id` (membership + self) runs the worker. Automatic failover at the next tick. A double-execution window during membership disagreement: accepted and documented (idempotent work; for external replication a duplicate delivery is possible inside the window). | ✅ decided in planning |
| H6 | **D3a = gate, not just a warning**: in quorum mode, if the observed live nodes exceed `cluster_size`, writes are refused (503 with a clear message) + flag in `/admin/cluster`. It is a misconfiguration that enables split-brain: the safety mode must fail closed. | ⚠ recommended, confirm with Pietro at the start of R3 |
| H7 | **Drift = out of the quorum**: a peer with `config_ok=false` does not count toward the quorum (it stays visible in `/admin/cluster` with the drift flag). | ✅ decided in planning |
| H8 | **Secret rotation via dual-secret**: optional `[cluster] secret_previous` key; inbound auth accepts both, outbound uses only `secret`. Runbook: set `secret_previous=old, secret=new`, rolling restart, then remove `secret_previous`. | ✅ decided in planning |
| H9 | **D6 console via server-side proxy**: a `?node=<node_id>` parameter on the per-node admin endpoints (audit/metrics/events), internal proxying via `ClusterClient`. Avoids CORS and browser-unreachable endpoints. | ⚠ recommended, confirm with Pietro at the start of R8 |
| H10 | **Rolling upgrade across mixed versions**: every wire change is additive (new JSON fields ignored by old nodes); the probe uses the new authenticated `/cluster/v1/ping` with a fallback to the old `/cluster/v1/health` on 404 (legacy peer, counts as alive with a warning). | ✅ decided in planning |
| H11 | **Versions**: end of R1+R2 → propose `v0.26.0` (MINOR: the quorum changes observable behavior). Subsequent milestones group into releases proposed at milestone end; numbers decided with Pietro at that time. | ⚠ proposal |

---

## R1 — P0 correctness (true quorum, PG cursor, tombstone-first)

The review's three P0s plus the §2.4 prerequisite. No new test infrastructure required (unit + the existing cluster suite); partition-based verification arrives in R2.

- [ ] **§2.4 Parallel fan-out** — replace the sequential `for endpoint in live_peers()` loops with `futures::future::join_all` collecting `Vec<Result>`: `cluster_meta.rs` (`fan_out_object` and the delete/op fan-outs, lines ~99/114/148), `cluster_blob.rs` (`fan_out`, ~77/107), `cluster_control.rs` (`fan_out_op`). Verify `ClusterClient` has a per-request timeout (reqwest); otherwise add one (default ~10 s).
- [ ] **§2.1 True quorum (decisions H1, H2, H4)**:
    - [ ] response of `POST /cluster/v1/object` → JSON `{applied: bool, has_blob: bool}` (handler `handlers/cluster.rs:165-185`); `has_blob` = sidecar present when the row references a blob.
    - [ ] `ClusterMetadataStore::put_object` (`cluster_meta.rs:227-238`): after the local write, parallel fan-out, `acks = 1 + successes_with_blob`; if `acks < write_quorum` in mode=quorum → error mapped to `503 ServiceUnavailable` with `Retry-After: 5`. Same logic for version-delete/tombstone and delete marker.
    - [ ] mode=available: unchanged (ACKs ignored).
    - [ ] pure function `quorum_satisfied(local+acks, write_quorum)` with unit tests; update the module doc-comment (remove the absolute "no divergence", describe the real semantics: durability quorum at ACK time, no rollback, convergence via anti-entropy).
    - [ ] integration: the existing phases B/C must stay green (2 alive → writes OK; 1 alive → 503).
- [ ] **§2.2 PG commit-ordered cursor (decision H3)** — pg migration (next free number): `object_seq` table initialized at `MAX(seq)`; replace `nextval('objects_seq')` with `UPDATE object_seq SET v=v+1 RETURNING v` inside the same transaction in all the write paths (`pg/metadata.rs`: put/apply/delete/tombstone); drop the sequence; a comment explaining the commit-order guarantee (mirroring the SQLite comment on `object_seq`). A concurrency smoke test in the PG suite if feasible.
- [ ] **§2.3 Tombstone-first in `apply_control_merge`** (`control_merge.rs:47-66`): adopt the tombstones BEFORE executing the deletes (for every entity); a unit test verifying the call order (a mock store recording the sequence) and the crash window (tombstone present + row alive = safe state, converges on the next round).
- [ ] CHANGELOG + update `ha.md` (consistency section: real quorum semantics, no rollback) — the full doc pass remains in R9.

*Outcome: quorum mode delivers what it promises at ACK time; the PG incremental sync loses no rows; the control merge no longer has the resurrection window.*

---

## R2 — Cluster test infrastructure (real partitions, available mode, catch-up)

The proving ground of the R1 fixes and of everything else. Extends `bin/cluster`, the dedicated compose file and `tests/integration/test_cluster.py`.

- [ ] **Partition helpers** in `bin/cluster`: `partition <n>` / `heal <n>` via `docker network disconnect/connect` on the `arca-cluster` compose project network (both processes stay ALIVE: that is the difference from `node-stop`).
- [ ] **Quorum window test (verifies §2.1)**: partition 2 peers → the isolated node must refuse writes with 503 *even though membership will only notice a few seconds later* (the missing ACKs close the window); heal → convergence.
- [ ] **Available-mode overlay (D8, §5.1)**: `docker/cluster/config-available.toml` (mode=available, everything else identical) + compose overlay; test phases: (a) 1/3 live nodes still writable; (b) partition, writes to the SAME key from both sides, heal → a single LWW winner everywhere, the loser disappears with no errors (also the basis of the doc sentence requested in §5).
- [ ] **Control-plane catch-up test (§5.2)**: node 3 down → create a bucket + a credential via the LB → node 3 up → after one reconcile tick, bucket and credential present when querying `ARCA_NODE3_ENDPOINT` directly.
- [ ] **Flakiness fixes (§5.3)**: `bin/test:315` waits ≥ 5 s or polls the LB (HAProxy `fall 2 inter 2s` ≈ 4 s); phase E: precondition that arca-3 shows alive in the topology before the `disk_available_bytes` check.
- [ ] Register any new pytest markers in `conftest.py`; update the "Integration — HA Cluster" row in the `README.md` Test Coverage table.

*Outcome: every consistency claim is exercised by a reproducible test, partitions included. Propose release `v0.26.0` (H11).*

---

## R3 — Membership and quorum integrity (ghost quorum, guards)

- [ ] **D1 Authenticated liveness**: new `GET /cluster/v1/ping` under `cluster_auth` answering `{node_id, config_fingerprint, disk_total, disk_available, max_seq}` (`max_seq` serves D3c in R7); the membership probe uses the signed `ping` via `ClusterClient`, with a fallback to `/cluster/v1/health` on 404 (legacy peer, H10).
- [ ] **§3.5 Minimized public health**: the unauthenticated `/cluster/v1/health` answers only `{status, node_id}` (the rest lives in the authenticated ping). Update any consumers (legacy membership, docs).
- [ ] **H7 Drift out of the quorum**: a peer with `config_ok=false` excluded from `has_write_quorum` (`cluster.rs:775-799`); it stays in `/admin/cluster` with the drift flag; the test phase F extended: with a diverging secret the node must NOT sustain the quorum (3 nodes, 1 drifted → quorum 2 still ok; 2 drifted → 503).
- [ ] **Failure-detector tolerance (D12.3, plan line 73)**: dead after 2 consecutive ping failures (not 1; `membership.rs:104`), alive at the first success. A comment on the why.
- [ ] **D3a `cluster_size` guard (decision H6, to be confirmed)**: in quorum mode, if the observed live nodes (including self) exceed `cluster_size` → write gate closed (503 with an explicit message) + `size_exceeded: true` in `/admin/cluster` + an error log on transitions.
- [ ] **§3.2 Liveness guard on the tombstone GC** (`anti_entropy.rs:119-134`): before `purge_tombstones`/`purge_control_tombstones`, verify that every known peer has been seen within the grace window; otherwise skip the purge with a highly visible warning + a `tombstone_gc_blocked` flag in `/admin/cluster`.
- [ ] **M3 Membership pruning** (`membership.rs:84-161`): remove peers unreachable for more than a configurable period (default = `tombstone_grace_days`); note: pruning also unblocks the §3.2 guard (a removed node does not block the GC forever); the residual risk of a beyond-grace re-entry is already documented (plan note 2-bis).
- [ ] Unit tests for gates/guards (pure functions where possible); drift test extension (above).

*Outcome: the quorum only counts nodes that can really receive replicas; no more ghost quorum; tombstone GC aware of liveness.*

---

## R4 — Inter-node transport security

- [ ] **§3.1 Anti-replay**: in `cluster_auth.rs` (~138-146), compare `x-amz-date` with the clock: outside ±15 minutes → 403. Unit tests (inside/outside the window, missing header).
- [ ] **§3.4 Dedicated body limit**: `DefaultBodyLimit` ~2 MiB on the `/cluster/v1/{object,op,manifest,control-snapshot}` sub-router (NOT on the blob routes).
- [ ] **M5**: startup validation `secret` ≥ 16 characters (clear error in `config.rs:800`).
- [ ] **M6**: `Uuid::parse_str` on the path `blob_id` in the cluster handlers before `write_raw`/`read_raw` (defense in depth).
- [ ] **D12.1 Receive-side filter**: the application of `ServerConfigSet/Delete` in `handlers/cluster.rs:275-279` skips node-local keys (shared `is_node_local_key`, today sender-side only).
- [ ] **H8/D3b Dual-secret for rotation**: optional `[cluster] secret_previous`; inbound auth tries both (constant-time on each), outbound and fingerprint use only `secret`; config validation (≥ 16 chars for previous too); unit tests; runbook in R9.
- [ ] Evaluate together with TD-015 (inter-node TLS) whether to close that here too (shared CA / pinning) or leave it an explicit TD: decide with Pietro at the start of the milestone.

*Outcome: an inter-node surface with a replay window, body limits, validations and secret rotation without downtime.*

---

## R5 — Reconcile completeness (TD-016, multipart, SSE-C)

- [ ] **D9 / TD-016 — extend snapshot + tombstones to the missing families**:
    - [ ] migrations (sqlite v22+, next pg): `updated_at` on `user_grants`, `team_grants`, `team_members`, `bucket_config`, `bucket_tags`, `server_config` (backfill = now or `created_at` where it exists), maintained in all the write paths including the `apply_remote_*`.
    - [ ] `control_tombstones` registration on detach/remove/delete of: grant attachments (composite key `user_id:grant_id` etc.), memberships, `bucket_config`/`bucket_tags` keys, `server_config` keys (node-local ones excluded).
    - [ ] extend `ControlSnapshot` (additive fields, H10) and `build_control_snapshot`/`apply_*`; the snapshot EXCLUDES node-local `server_config` keys.
    - [ ] extend `plan_control_merge` (pure per-family functions, alive-vs-tombstone LWW unit tests for each).
    - [ ] extend the R2 catch-up test: a grant-attachment revocation and a `bucket_config` change with the node down → reconciled at re-entry.
    - [ ] close TD-016 in `TECH_DEBT.md` + roadmap.
- [ ] **D4 Multipart**:
    - [ ] include `multipart_uploads` + `parts` in the reconcile snapshot with a `multipart:<upload_id>` tombstone registered on Complete and Abort (a closed upload must not resurrect).
    - [ ] `ClusterBlobStore::concat` (`cluster_blob.rs:179-186`): pre-check the part sidecars; missing ones are fetched from peers (reusing the repair path) before delegating to `inner.concat`.
    - [ ] tests: abort with a node down → at re-entry the upload does not exist on the node; Complete with a locally missing part → succeeds via fetch-from-peer (unit or targeted integration).
- [ ] **M7 Manifest churn**: in `apply_remote_object` (sqlite and pg), a no-op without stamping a new `seq` when the incoming row is identical to the existing one.
- [ ] **§3.6 SSE-C in the cluster — spike (2 h timebox)**: verify whether wrapping the SSE-C path in `ClusterBlobStore` suffices (`write_sidecar` fan-out + read-repair); if yes implement + test; if not, create **TD-017** in `TECH_DEBT.md` (+ a code marker at `main.rs:166`) and document the limitation in `ha.md` (R9). Either way the outcome must be tracked.
- [ ] **D12.2 Export/import and `node_id`**: the export omits `node_id` AND the import refuses/skips it (double defense), so an export imported on another node does not rewrite its identity (`admin_export.rs:138-145`, `admin_import.rs:99`).

*Outcome: all managed state converges after an absence, multipart included; TD-016 resolved; SSE-C fixed or honestly tracked.*

---

## R6 — Cluster-aware workers (leader gate)

- [ ] **§3.3 / H5**: helper `ClusterContext::is_worker_leader()` = own `node_id` is the lowest among the live ones (including self); always true on single-node.
- [ ] **Lifecycle worker** (`worker.rs`): in a cluster, only the leader runs the tick (debug-level log when skipping). The deletes it produces stay replicated/tombstoned as today.
- [ ] **Phase 28 replication worker** (external S3 destinations): same gate → no more duplicate deliveries to the destination. Document the double-execution window during a leadership change (H5).
- [ ] **Audit of the other workers** (an explicit decision, written in the code or here): retention purge, metrics snapshot, notification delivery = NOT gated (they operate on local tables/events by design); the lifecycle's abort-incomplete-multipart = gated with the lifecycle.
- [ ] Tests: 3-node cluster with a short-expiry lifecycle rule → the expiry happens exactly once (the leader's audit log, no duplicate delete); stopping the leader → the next node takes the role at the following tick.

*Outcome: no work duplicated N times, no duplicate deliveries to external destinations, with automatic failover of the role.*

---

## R7 — Synchronization state and operability

- [ ] **D2 "Syncing" readiness**: `ClusterContext` tracks, for every live peer, the completion of the FIRST reconcile pass since startup; until it is complete towards all live peers, `/admin/health` answers `503 {"status":"syncing"}` (drain takes precedence; no live peers → degraded `ok`, to be documented). The LB and the k8s probes see it → a realigning node receives no traffic until it is coherent. Test: a node restarted after downtime with new data → health 503 until catch-up completes, then 200.
- [ ] **Exposed lag**: `/admin/cluster` (and `?verbose=1`) with, per peer: last completed reconcile, HWM cursor, `first_pass_done`.
- [ ] **D3c Rewind detection**: the `ping` (R3) exposes `max_seq`; if the local HWM for that peer exceeds its `max_seq` → reset the HWM to 0 + a warning (covers restore-from-backup without restarting the peers).
- [ ] **M1 Stuck HWM**: after K=5 consecutive failures on the same `seq` in `apply_remote_object` (`anti_entropy.rs:361-371`), skip the entry with a warning + a counter visible in `/admin/cluster`.
- [ ] **M2 Repair budget**: `repair_blobs` (`anti_entropy.rs:154-206`) with a per-tick budget (default 100 blobs) + a resume cursor; no more scans monopolizing the worker for hours.
- [ ] **M4 `Retry-After`**: on ALL cluster 503s (missing quorum, syncing, size_exceeded) — the rate limiter already does it.
- [ ] **M8**: `tracing::warn!` on the mtime→now fallback in `fs/blob.rs:703-708`.
- [ ] Update the console topology card if the new fields are needed (lag, syncing, flags): minimal visualization, the console bulk is R8.

*Outcome: a re-entering node serves no wrong answers; the operator sees lag and anomalies; the worker neither stalls nor monopolizes.*

---

## R8 — Console: per-node views behind the LB

- [ ] **D6 (decision H9, to be confirmed)**: a `?node=<node_id>` parameter on the per-node admin endpoints (`/admin/audit`, `/admin/metrics/history`, notification event log): when present and ≠ self, the node proxies the request to the peer via `ClusterClient` (short timeout, clear error when the peer is down).
- [ ] Console: a node selector in the audit/monitoring/events views (populated from `/admin/cluster`, default "this node via LB" with the source `node_id` always visible). Go through the frontend-design skill; replicate the existing view filter patterns EXACTLY; a coherence sweep across all the per-node views.
- [ ] Screenshots + console manual (`documentation/docs/guide/console.md` + `bin/screenshots`) if the UI changes visibly.

*Outcome: the operator always knows WHICH node they are looking at and can pick it, without depending on round-robin luck.*

---

## R9 — Documentation, deploy and closure

- [ ] **`ha.md`** (source + `docs/` via `bin/docs-build`):
    - [ ] the REAL post-R1 quorum semantics (durability ACK, no rollback, error examples) and the available mode one: *concurrent writes to the same key are both acknowledged but only the LWW winner survives; the loser is discarded with no error to the client* (§5-doc1).
    - [ ] **D11**: available-mode RPO note (single-copy window).
    - [ ] **D7**: the real read window (partition, not just lag); CP = no conflicting writes, not read linearizability; sticky as a reference configuration.
    - [ ] **D5**: the 503-behind-LB behavior and its mitigation (SDK retries; the write-aware option as a future possibility, decide whether to implement it or only document it).
    - [ ] **D10**: WORM/Object Lock threat model in the cluster (shared secret = full control; link with TD-015).
    - [ ] **§3.6**: the SSE-C limitation (if not fixed in R5).
    - [ ] **Operational runbooks (D3, §5-doc2)**: replacing a dead node (empty disk = safe, the syncing readiness protects it); restore from backup (rewind detected by D3c, procedure documented anyway); cluster resize (recommended cold procedure + why rolling is dangerous, the D3a guard); secret rotation (dual-secret, H8); coherent backups (SQLite WAL); forming a cluster from non-empty nodes (D12.4: union+LWW merge, identical master key required, discouraged unless necessary).
- [ ] **Deploy**: align `fall/rise` across `docker/cluster/haproxy.cfg`, `deploy/haproxy/haproxy.cfg` and the `ha.md` snippet (or motivate the difference in comments); add a commented sticky example (`balance source` or cookie) in both cfgs; k8s probes with `periodSeconds: 5, failureThreshold: 2` in `arca-cluster.yaml` (+ a note that readiness now reflects syncing, D2).
- [ ] **`TECH_DEBT.md` + roadmap**: TD-016 resolved (R5); TD-017 SSE-C if open; re-evaluate TD-015 after R4; the roadmap tech-debt section in sync.
- [ ] **Roadmap**: update the Phase 29 section with a line about the post-review hardening (checkbox or note), coherent with the existing format.
- [ ] **Review**: mark the resolved findings in the review (a note at the top pointing to this plan), so review and plan stay coherent.
- [ ] **README**: Test Coverage table updated (new R2/R5/R6/R7 tests).
- [ ] **CHANGELOG**: consolidate Unreleased → final release; versions in sync (Cargo.toml, console, roadmap) per `RELEASING.md`.

*Outcome: the system declares exactly what it does, the operator has the runbooks, the debt is tracked, final release proposed.*

---

## Finding → milestone traceability

Update the Status column as work proceeds: ⬜ to do, 🔧 in progress, ✅ done, ➖ decided not to do (with a note).

| Finding | Short description | Milestone | Status |
|---|---|---|---|
| §2.1 | Quorum = admission gate, not a write quorum | R1 | ⬜ |
| §2.2 | `seq` cursor race on PostgreSQL | R1 | ⬜ |
| §2.3 | Deletes before tombstones in the control merge | R1 | ⬜ |
| §2.4 | Sequential fan-out | R1 | ⬜ |
| §3.1 | No anti-replay window | R4 | ⬜ |
| §3.2 | Tombstone GC blind to liveness | R3 | ⬜ |
| §3.3 | Workers duplicated on every node | R6 | ⬜ |
| §3.4 | Cluster endpoints without a body limit | R4 | ⬜ |
| §3.5 | Public health exposes disk/fingerprint | R3 | ⬜ |
| §3.6 | SSE-C not replicated | R5 (spike) + R9 (doc) | ⬜ |
| M1 | HWM stuck on a failing entry | R7 | ⬜ |
| M2 | Repair without a budget | R7 | ⬜ |
| M3 | Membership without eviction | R3 | ⬜ |
| M4 | 503 without Retry-After | R7 | ⬜ |
| M5 | 1-character secret accepted | R4 | ⬜ |
| M6 | Path blob_id not validated | R4 | ⬜ |
| M7 | seq churn on identical rows | R5 | ⬜ |
| M8 | Silent mtime fallback | R7 | ⬜ |
| §5.1 | No available-mode test | R2 | ⬜ |
| §5.2 | No control-plane catch-up test | R2 | ⬜ |
| §5.3 | Latent flakiness (wait/phase E) | R2 | ⬜ |
| §5.4 | Listed test debt (GC, repair, bootstrap, skew) | R2 (partial: bootstrap in the R7 test; skew stays ➖ documented) | ⬜ |
| §5-deploy | Inconsistent HAProxy fall/rise; k8s probes | R9 | ⬜ |
| §5-doc1/2/3 | Available LWW, runbooks, SSE-C/quorum | R9 | ⬜ |
| D1 | Ghost quorum (unauthenticated liveness, drift ignored) | R3 | ⬜ |
| D2 | No syncing state (404s/partial listings at re-entry) | R7 | ⬜ |
| D3a | No nodes > cluster_size guard | R3 | ⬜ |
| D3b | Secret rotation without dual-secret | R4 (+ runbook R9) | ⬜ |
| D3c | Restore from backup: seq rewind vs HWM | R7 (+ runbook R9) | ⬜ |
| D4 | Multipart without reconcile; local-only concat | R5 | ⬜ |
| D5 | LB blind to writability (client-visible 503s) | R9 (doc; write-aware to be decided) | ⬜ |
| D6 | Console incoherent on per-node views | R8 | ⬜ |
| D7 | Quorum reads weaker than the CP label | R9 (doc + sticky reference) | ⬜ |
| D8 | No real-partition test | R2 | ⬜ |
| D9 | TD-016 underestimated (RBAC/bucket_config) | R5 | ⬜ |
| D10 | WORM in cluster: trust model undocumented | R9 | ⬜ |
| D11 | Available RPO undeclared | R9 | ⬜ |
| D12.1 | Receive side without a node-local key filter | R4 | ⬜ |
| D12.2 | Export/import rewrites node_id | R5 | ⬜ |
| D12.3 | Failure detector without tolerance | R3 | ⬜ |
| D12.4 | Non-empty single-node merge undocumented | R9 | ⬜ |

## Estimate and sequence

R1 ≈ 2-3 d · R2 ≈ 1-2 d · R3 ≈ 1-2 d · R4 ≈ 1 d · R5 ≈ 2-3 d · R6 ≈ 1 d · R7 ≈ 1-2 d · R8 ≈ 1 d · R9 ≈ 1 d → **~11-16 effective days**. The R1→R9 order is binding only where there is a technical dependency (R2 verifies R1; R3 provides the `ping` used by R7; the rest can be reordered if needed).

## Out of scope (explicitly deferred)

- Read-quorum / linearizable reads (stays future hardening, as per the Phase 29 plan).
- HLC instead of NTP+LWW.
- A write-aware health check as the LB default (D5: documented; implemented only if decided in R9).
- Automated clock-skew tests (documented as a limit, the NTP constraint is already in `ha.md`).
- Erasure coding, sharding, gossip: out of scope as per the Phase 29 plan.
