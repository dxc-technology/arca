# Arca — HA Hardening (remediation after the Phase 29 review)

## Context

The Phase 29 review ([`arca-phase-29-ha-review.md`](https://github.com/dxc-technology/arca/blob/main/.claude/reviews/arca-phase-29-ha-review.md), passes of 2026-06-10 and 2026-06-11) produced the findings: **3 P0s** (§2.1–§2.3), **§2.4 + 9 P1s** (§3.x, D1, D2, D3a, D9), a **security-critical chain** (§3.7), **a series of P2s** (M1–M8, §5, D3b/c, D4–D8) and **P3/doc** (D10–D12). This plan implements ALL of them, organized in 9 milestones (R1–R9) ordered by priority and technical dependency. It is a living document, published on the documentation site as an annex of the roadmap (Phase 29.1) via a symlink to the canonical file `.claude/plans/arca-phase-29-ha-hardening.md`.

> **Security workstream (review §3.7).** The single most important security gap — a rogue peer receiving all new data with no secret, because the fan-out push direction authenticates no peer — plus its amplifiers (brute-forceable fingerprint, weak secrets allowed, plain-HTTP/unverified-TLS/no-replay) is spread across R3 (peer authentication of liveness + fan-out gating; fingerprint off the public endpoint) and R4 (verified inter-node TLS / TD-015; secret strength; anti-replay). On a deployment whose cluster network is **not** a trusted, isolated segment, treat the peer-authentication half (decision H12) with **P0 urgency** and consider pulling R4's TLS item forward to sit beside R3. The secret itself is well handled where it is used (never on the wire, HMAC-keyed SigV4, isolated credential); the gap is that authenticating *peers* — not just *requests* — was never designed.

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
| H6 | **D3a = gate, not just a warning**: in quorum mode, if the observed live nodes exceed `cluster_size`, writes are refused (503 with a clear message) + flag in `/admin/cluster`. It is a misconfiguration that enables split-brain: the safety mode must fail closed. No config escape hatch (rejected as a foot-gun if left enabled); the R9 runbook documents the correct resize procedure instead. | ✅ confirmed by Pietro (2026-06-11) |
| H7 | **Drift = out of the quorum**: a peer with `config_ok=false` does not count toward the quorum (it stays visible in `/admin/cluster` with the drift flag). | ✅ decided in planning |
| H8 | **Secret rotation via dual-secret**: optional `[cluster] secret_previous` key; inbound auth accepts both, outbound uses only `secret`. Runbook: set `secret_previous=old, secret=new`, rolling restart, then remove `secret_previous`. | ✅ decided in planning |
| H9 | **D6 console via server-side proxy**: a `?node=<node_id>` parameter on the per-node admin endpoints (audit/metrics/events), internal proxying via `ClusterClient`. Avoids CORS and browser-unreachable endpoints. | ⚠ recommended, confirm with Pietro at the start of R8 |
| H10 | **Rolling upgrade across mixed versions**: every wire change is additive (new JSON fields ignored by old nodes); the probe uses the new authenticated `/cluster/v1/ping` with a fallback to the old `/cluster/v1/health` on 404 (legacy peer, counts as alive with a warning). | ✅ decided in planning |
| H11 | **Versions**: a SINGLE release at the end of the full remediation plan (R1–R9); version number decided then (at least MINOR: the quorum changes observable behavior). Supersedes the original idea of `v0.26.0` after R1+R2 with per-milestone releases. Milestones are still committed and pushed as they complete. | ✅ revised and decided by Pietro (2026-06-11) |
| H12 | **Authenticate the peer, not just the request (closes review §3.7).** Today the cluster authenticates the *sender* of every `/cluster/v1/*` request (inbound, via `cluster_auth`) but never the *receiver* of a fan-out: `live_peers()` filters on `alive` only, and membership admits an mDNS peer on a `cluster_id` match alone — so a rogue peer receives all new writes with no secret. Fix, in order of preference: **(1) mutual TLS with a shared cluster CA** — a node verifies the peer's CA-signed cert before adding it to membership / fanning out (also resolves TD-015 and the cleartext/MITM exposure). **(2) Secret-only challenge-response**: an app-layer exchange where the peer proves possession of the secret over a fresh nonce before being counted live — NOT merely "a signed `/ping`" (a rogue controls its own server and can return 200 unconditionally; the peer must prove possession *to us*). Either way, `live_peers()` (fan-out) AND the quorum count only authenticated **and** `config_ok` peers. **CA origin (confirmed with Pietro)**: the CA is *operator-distributed via config* (`[cluster]` CA path + per-node cert/key) with a shipped generator command reusing the rcgen `tls-init` generator — NOT a K8s-style auto-enrollment (first node generates the CA, peers get CSRs signed over a secret-authenticated channel): issuance gated by the standing secret would collapse mTLS to secret strength, while an operator-distributed CA is an independent second factor (K8s mitigates with short-TTL join tokens; possible future evolution, not now). **Layering (confirmed)**: (2) is not a fallback — it ships for ALL clusters as the baseline peer-auth layer in R3 (it also closes the rogue peer on plain-HTTP deployments, where no certificates exist; on a plain-HTTP network it stops the easy attack — a rogue mDNS registrant — while sniffing/MITM still requires TLS), with (1) on top as the independent second factor on TLS clusters. | ✅ confirmed by Pietro (2026-06-11): BOTH layers — challenge-response baseline for every cluster (R3) + mutual TLS with the CA via config + shipped generator tooling (R4) |

---

## R1 — P0 correctness (true quorum, PG cursor, tombstone-first)

The review's three P0s plus the §2.4 prerequisite. No new test infrastructure required (unit + the existing cluster suite); partition-based verification arrives in R2.

- [x] **§2.4 Parallel fan-out** — replaced the sequential loops with `futures_util::future::join_all`: `cluster_meta.rs` (`fan_out_object`, `fan_out_version_delete`, `fan_out_op`), `cluster_blob.rs` (`fan_out`), `cluster_control.rs` (`fan_out_op`). `ClusterClient` already had a per-request reqwest timeout (`[cluster].request_timeout_seconds`, default 10 s) — verified, nothing to add.
- [x] **§2.1 True quorum (decisions H1, H2, H4)**:
    - [x] response of `POST /cluster/v1/object` (and `/object/delete`) → JSON `ClusterObjectAck {applied, has_blob}`; `has_blob` = sidecar present when the row references a blob, vacuously true otherwise (deletes/markers/tombstones). Legacy peers answering an empty 200 count as a full ack (H10 rolling upgrade, handled in `ClusterClient::parse_ack`).
    - [x] `ClusterMetadataStore::put_object`: after the local write, parallel fan-out, `acks = 1 + full_acks`; shortfall in mode=quorum → `503 ServiceUnavailable` + `Retry-After: 5` (header added in `s3_error_response` for every ServiceUnavailable). Same enforcement in `delete_object` (marker + version-delete branches) and `delete_object_version`.
    - [x] mode=available: unchanged (ACKs ignored).
    - [x] pure function `quorum_satisfied(acks, write_quorum)` in `arca-core::cluster` with unit tests; `cluster_meta.rs` module doc rewritten (admission gate + ACK counting, no rollback, anti-entropy convergence; "no divergence" absolute removed). 8 new unit tests total (quorum_satisfied ×2, control_merge ×2, cluster_meta ×4 incl. a fake-peer ACK server; the old `quorum_mode_writes_with_majority_despite_unreachable_peer` flipped into `quorum_mode_refuses_write_when_acks_below_quorum`).
    - [x] integration: full `bin/test cluster` green (phases A–F; B: 2 alive → writes OK with real ACKs; C: 1 alive → 503).
- [x] **§2.2 PG commit-ordered cursor (decision H3)** — pg migration 0009: `object_seq` single-row counter seeded `GREATEST(MAX(seq), objects_seq.last_value)`; all write paths (`put_object` ×3 branches, delete marker ×2, tombstone UPDATEs ×5, `apply_remote_object` ×2) now take the seq via `next_object_seq` (`UPDATE ... RETURNING`) inside the row's transaction; sequence + column DEFAULT dropped. Commit-order guarantee + a uniform seq→rows lock-order rule (deadlock avoidance) documented on the helper. Concurrency smoke test added to the PG suite (40 parallel puts + mixed overwrite/delete) — `bin/test postgres` green (21 tests).
- [x] **§2.3 Tombstone-first in `apply_control_merge`** (`control_merge.rs`): tombstones adopted BEFORE upserts and deletes (clears stay last); TDD unit test with a recording mock store pinning the order (red on the old code, green after); a comment in `reconcile_peer_control` pins that bucket deletes also run after the adoption.
- [x] CHANGELOG (Unreleased: Changed ×2, Fixed ×2) + `ha.md` consistency section updated (ACK-time durability quorum, two enforcement layers, no-rollback warning box, LWW scope per mode) — the full doc pass remains in R9.

*Outcome: quorum mode delivers what it promises at ACK time; the PG incremental sync loses no rows; the control merge no longer has the resurrection window.*

---

## R2 — Cluster test infrastructure (real partitions, available mode, catch-up)

The proving ground of the R1 fixes and of everything else. Extends `bin/cluster`, the dedicated compose file and `tests/integration/test_cluster.py`.

- [x] **Partition helpers** in `bin/cluster`: `partition <n>` / `heal <n>` via `docker network disconnect/connect` (both processes stay ALIVE: that is the difference from `node-stop`). Required a **dual-network compose design**: the `cluster` network carries inter-node + LB traffic and the seed list now uses `arca-N-cluster` aliases that exist ONLY there, so disconnecting a node severs peer traffic; the never-partitioned `mgmt` network keeps the test runner connected to every node by its plain service name (without it, the runner would lose the isolated node along with the peers). `heal` re-attaches WITH the alias (a manual `network connect` does not restore compose aliases); the node may return with a new IP — peers re-resolve per probe, but HAProxy (startup-resolved) may not track it, so the partition phases are self-contained (fresh `up`/`down -v`) and post-heal assertions target nodes directly. Test configs also gained `request_timeout_seconds = 5` (not in the drift fingerprint — verified) so fan-out toward an unreachable peer fails fast.
- [x] **Quorum window test (verifies §2.1)** — phase G: partition arca-3 → it refuses writes with 503 *with no wait after the disconnect, on purpose*: within the window where its membership still sees the peers alive, the missing fan-out ACKs produce the 503 (observed: the failing PUT took ~5 s = the fan-out timeout, i.e. the in-window path); reads still served from the isolated node; the majority side keeps writing; heal → the majority-side write converges on arca-3 and a polled PUT proves it accepts writes again (its own membership view lags a few ticks).
- [x] **Available-mode overlay (D8, §5.1)** — `docker/cluster/config-available.toml` (mode=available, everything else identical) + `docker-compose.cluster.available.yml` mounting it on all 3 nodes; phase H: (a) with 2/3 nodes stopped the survivor still accepts writes (same topology where quorum mode 503s in phase C); (b) partition, writes to the SAME key from both sides — both accepted — heal → a single LWW winner everywhere, the loser disappears with no errors (the basis of the §5 doc sentence, to be written in R9).
- [x] **Control-plane catch-up test (§5.2)** — folded into the existing lifecycle: phase B (node 3 down) creates `cluster-catchup-cp` bucket + a credential via the LB (new signed `_admin_post` helper); phase D polls node 3 directly until both appear via the control-plane reconcile.
- [x] **Flakiness fixes (§5.3)**: `wait_live` post-convergence sleep 2 s → 5 s (HAProxy `fall 2 inter 2s` ≈ 4 s to evict); phase E precondition now requires `live_node_count == 3` on `/admin/cluster` alongside the cluster-min free-space check (arca-3's disk stats are cleared while it is considered dead).
- [x] 7 new pytest markers registered in `conftest.py` (`cluster_partition_before/minority/healed`, `cluster_available_full/split/converged/minority`); "Integration — HA Cluster" row in `README.md` updated 11 → 24 (+ totals). Full suite green: 8 phases (A–H), 24/24.

*Outcome: every consistency claim is exercised by a reproducible test, partitions included. (Release deferred to plan completion — H11 as revised.)*

---

## R3 — Membership and quorum integrity (ghost quorum, guards)

- [x] **D1 / §3.7(A) Authenticate the peer, then gate fan-out AND quorum on it (decision H12)**: it is not enough to sign our outbound probe — the *peer* must prove it holds the secret (or presents a cluster-CA cert) before we trust it, else a rogue receives all fan-out with no secret. Implement the chosen H12 path:
    - mutual-TLS path (CONFIRMED 2026-06-11, see H12): **→ R4** — once R4 wires the shared cluster CA, membership only trusts a peer whose cert validates against it (independent second factor on top of the challenge-response below);
    - [x] secret challenge-response (the baseline peer-auth layer for ALL clusters, plain-HTTP included): new `GET /cluster/v1/ping` under `cluster_auth` returning `{node_id, config_fingerprint, disk_total, disk_available, max_seq, nonce_mac}` where `nonce_mac = HMAC(secret, our_nonce)` (HMAC-SHA256, domain-separated, constant-time verify) proves possession to us (`max_seq` also serves D3c in R7, read from the seq *counter*, not `MAX(seq)` — purged tombstones would make the row maximum go backwards and false-alarm the rewind detection; new `MetadataStore::current_object_seq` on both backends). The membership probe sends a fresh UUID nonce per probe and verifies the MAC. As built, the probe distinguishes: valid MAC → `authenticated`; 200 with missing/bad MAC → alive, untrusted (a rogue answering 200 unconditionally); **403 → the peer rejects our secret: identity recovered via the public health, flagged `config_ok=false`** (this replaces the fingerprint-based detection for wrong-secret drift, since §3.5 removed the public fingerprint); 404 → legacy pre-ping peer (H10), public-health fallback, alive-but-excluded with a once-per-transition warning (consequence, documented in `ha.md`: during a rolling upgrade the first upgraded node refuses writes until a second upgraded node is up — legacy nodes' own fan-out/anti-entropy keep data converging meanwhile); 409 (loop-prevention LoopDetected) → it is this node itself. Probing is now parallel across endpoints (§2.4 spirit: one dead peer must not serialize the tick).
    - [x] **`live_peers()` (fan-out target list in `cluster_blob.rs`/`cluster_meta.rs`) and `has_write_quorum` count only peers that passed peer-authentication AND are `config_ok`** — `PeerNode::eligible()` is the single predicate; it also gates the control-plane `fan_out_op`, the anti-entropy pulls (manifest, control snapshot, blob repair — pulling from an unauthenticated endpoint is a data-poisoning vector, and a wrong-master-key peer's blobs would be undecryptable), and `min_disk` (a rogue advertising a tiny disk must not close the 507 capacity guard). `/admin/cluster` exposes per-node `authenticated` and the cluster-level `eligible_node_count` (what the quorum is measured against; `live_node_count` stays the visibility count).
- [x] **§3.5 / §3.7(B) Minimized public health**: the unauthenticated `/cluster/v1/health` answers only `{status, node_id}` — the `config_fingerprint`, disk stats and `max_seq` move to the authenticated ping. This removes the offline brute-force oracle for the secret (the truncated SHA-256 over mostly-guessable inputs). Consumers updated: membership reads the detail from the ping (and still reads it from a *legacy* peer's public health during the upgrade window); the console and tests use `/admin/cluster`, untouched.
- [x] **H7 Drift out of the quorum**: a peer with `config_ok=false` excluded from `has_write_quorum` (now `ClusterState::write_gate()` over eligible nodes); it stays in `/admin/cluster` with the drift flag; test phase F extended: with a diverging secret the node must NOT sustain the quorum (3 nodes, 1 drifted → quorum 2 still ok and writes succeed; 2 drifted, distinct wrong secrets → the aligned node 503s both object writes and bucket creation while reads keep working — new phase F2 with a second drift overlay on arca-2).
- [x] **Failure-detector tolerance (D12.3)**: dead after 2 consecutive probe failures (`DEAD_AFTER_FAILURES` in `membership.rs`, with the why), alive at the first success; within the window the last live view is re-emitted. Dead peers now keep `last_seen` (= last successful contact) instead of clearing it — the §3.2 guard, M3 pruning and the admin view all reason about how long a peer has been unseen.
- [x] **D3a `cluster_size` guard (decision H6, fail-closed, no escape hatch)**: in quorum mode, if the observed ELIGIBLE nodes (including self) exceed `cluster_size` → write gate closed (`WriteGate::SizeExceeded`, distinct 503 message pointing at the resize runbook) + `size_exceeded: true` in `/admin/cluster` + an error log on transitions (in membership, which owns the tick). Counting eligible — not merely alive — nodes is deliberate: an unauthenticated rogue must not be able to close the gate (write-DoS via mDNS registration), while a same-config 4th node is eligible and trips it.
- [x] **§3.2 Liveness guard on the tombstone GC** (`anti_entropy.rs`): before `purge_tombstones`/`purge_control_tombstones`, verify that every known peer has been seen within the grace window (pure `tombstone_gc_blockers()` in arca-core, unit-tested); otherwise skip the purge with a warning naming the blockers + the `tombstone_gc_blocked` flag in `/admin/cluster`. Soundness: a peer seen within the grace was alive after every purge-eligible tombstone's deletion (so it already pulled it); a tombstone recorded while the peer was already unreachable purges only after the peer has been unseen longer than the grace — which is exactly when the guard blocks.
- [x] **M3 Membership pruning** (`membership.rs`): remove peers unreachable for more than `[cluster] peer_prune_days` (validated ≥ 1; default = `tombstone_grace_days`); pruning also unblocks the §3.2 guard (a removed node does not block the GC forever); the residual risk of a beyond-grace re-entry is documented (plan note 2-bis + the pruning warn log + `ha.md`). The guard's memory is process-local (membership state): after a restart, peers that never came back are unknown and cannot block — documented limitation, acceptable because the grace ≫ restart frequency.
- [x] Unit tests for gates/guards: arca-core (MAC roundtrip/tamper, eligibility, quorum/size gates over eligible peers, min_disk eligibility, GC blockers, snapshot flags, legacy-payload serde) + membership (D12.3 transitions, drift verdicts, end-to-end probe against a fake ping peer with real/bogus MACs) + config (prune default/validation) + sqlite (`current_object_seq` tracks the counter). 805 unit tests total (was 780). Drift integration extension above.

*Outcome: the quorum only counts nodes that can really receive replicas; no more ghost quorum; tombstone GC aware of liveness.*

---

## R4 — Inter-node transport security

- [ ] **§3.7(C) / TD-015 Verified inter-node TLS with a shared cluster CA (committed, not "evaluate")**: build the membership probe and `ClusterClient` `reqwest` clients against a shared cluster CA root and DROP `danger_accept_invalid_certs` (`membership.rs:50`, `client.rs:74`). This is the H12 path confirmed by Pietro (2026-06-11): verifying the peer's CA-signed cert authenticates the *receiver* of a fan-out (closes §3.7(A) robustly) AND restores confidentiality against passive sniffing / active MITM on the cluster LAN (§3.7(C)). Design as confirmed:
    - CA distributed via config (`[cluster]` CA path alongside `secret`) + per-node cert/key signed by it; verification in BOTH directions (client verifies server cert, server verifies client cert on cluster routes).
    - **Shipped generator tooling** for operators without a PKI: mint the cluster CA + per-node certs reusing the existing rcgen `tls-init` generator (distinct DistinguishedName CA vs nodes, `pem` feature — known gotchas).
    - **Single-port constraint**: S3 clients share the listener with `/cluster/v1/*`, so the client cert is *requested but optional* at the TLS layer and **enforced at the route layer** on the cluster endpoints (we control the accept loop with tokio-rustls; expose the verified peer cert to handlers via request extensions).
    - K8s-style auto-enrollment rejected for now (see H12 rationale); document the CA + cert distribution in the R9 runbook. Resolves TD-015 in `TECH_DEBT.md` + roadmap.
- [ ] **§3.1 / §3.7(C) Anti-replay**: in `cluster_auth.rs` (~138-146), compare `x-amz-date` with the clock: outside ±15 minutes → 403. Unit tests (inside/outside the window, missing header).
- [ ] **§3.4 Dedicated body limit**: `DefaultBodyLimit` ~2 MiB on the `/cluster/v1/{object,op,manifest,control-snapshot}` sub-router (NOT on the blob routes).
- [ ] **M5 / §3.7(B) Secret strength (security-critical, not cosmetic)**: startup validation `secret` length ≥ 16 chars AND reject the shipped placeholders (`dev-cluster-secret-change-me`, `CHANGEME-CLUSTER-SECRET`); warn if it looks low-entropy; document that it must be a high-entropy random value (clear errors in `config.rs:800`). Pairs with §3.5 (removing the public fingerprint oracle) and the CA path above.
- [ ] **M6**: `Uuid::parse_str` on the path `blob_id` in the cluster handlers before `write_raw`/`read_raw` (defense in depth).
- [ ] **D12.1 Receive-side filter**: the application of `ServerConfigSet/Delete` in `handlers/cluster.rs:275-279` skips node-local keys (shared `is_node_local_key`, today sender-side only).
- [ ] **H8/D3b Dual-secret for rotation**: optional `[cluster] secret_previous`; inbound auth tries both (constant-time on each), outbound and fingerprint use only `secret`; config validation (≥ 16 chars for previous too); unit tests; runbook in R9.

*Outcome: an inter-node surface with authenticated peers (verified TLS), a replay window, body limits, strong-secret enforcement and secret rotation without downtime — closing the §3.7 security chain together with R3.*

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
- [ ] **N1 Object-lock changes invisible to anti-entropy** (found during R1, not in the review): `set_object_retention` / `set_object_legal_hold` UPDATE the row WITHOUT a new `seq` on BOTH backends (`sqlite/metadata.rs:1391-1457`, `pg/metadata.rs` retention/legal-hold UPDATEs), so a peer that was down during a lock change never receives it via the changed-since manifest — only the real-time `replicate_lock_change` fan-out covers it. Fix: stamp a fresh `seq` (next_object_seq) in both backends' lock UPDATEs; the peer's `apply_remote_object` equal-tuple LWW guard (`>=`) already accepts the row with updated lock columns. Add to the R2/R5 catch-up test: a retention change with the node down → reconciled at re-entry.
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
- [ ] **M4 `Retry-After`**: on ALL cluster 503s — since R1 `s3_error_response` adds it to every `ServiceUnavailable`, so the quorum 503 already carries it and the new syncing/size_exceeded 503s inherit it automatically; here just verify they do (the rate limiter already sets its own).
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
| §2.1 | Quorum = admission gate, not a write quorum | R1 | ✅ |
| §2.2 | `seq` cursor race on PostgreSQL | R1 | ✅ |
| §2.3 | Deletes before tombstones in the control merge | R1 | ✅ |
| §2.4 | Sequential fan-out | R1 | ✅ |
| §3.1 | No anti-replay window | R4 | ⬜ |
| §3.2 | Tombstone GC blind to liveness | R3 | ✅ |
| §3.3 | Workers duplicated on every node | R6 | ⬜ |
| §3.4 | Cluster endpoints without a body limit | R4 | ⬜ |
| §3.5 | Public health exposes disk/fingerprint | R3 | ✅ |
| §3.6 | SSE-C not replicated | R5 (spike) + R9 (doc) | ⬜ |
| §3.7(A) | Rogue peer receives all new data with no secret (fan-out authenticates no peer) | R3 (H12: peer auth, gate fan-out+quorum) + R4 (mutual TLS) | 🔧 (R3 half ✅: challenge-response peer auth gates fan-out, anti-entropy pulls, quorum and min_disk; R4 adds the mutual-TLS second factor) |
| §3.7(B) | Secret brute-forceable from public fingerprint; weak secrets allowed | R3 (§3.5 fingerprint off public) + R4 (M5 secret strength) | 🔧 (R3 half ✅: fingerprint off the public health; R4 adds M5 secret-strength enforcement) |
| §3.7(C) | Plain-HTTP/unverified-TLS/no-replay enable sniff/MITM/replay | R4 (TD-015 verified TLS + §3.1 anti-replay) | ⬜ |
| TD-015 | Inter-node TLS accepts invalid certs (now committed, not deferred) | R4 | ⬜ |
| M1 | HWM stuck on a failing entry | R7 | ⬜ |
| M2 | Repair without a budget | R7 | ⬜ |
| M3 | Membership without eviction | R3 | ✅ |
| M4 | 503 without Retry-After | R7 | 🔧 (quorum 503s carry it since R1 — `s3_error_response` adds it to every ServiceUnavailable; R7 verifies syncing/size_exceeded inherit it) |
| M5 | 1-character secret accepted | R4 | ⬜ |
| M6 | Path blob_id not validated | R4 | ⬜ |
| M7 | seq churn on identical rows | R5 | ⬜ |
| N1 | Lock changes (retention/legal-hold) don't bump `seq` → invisible to anti-entropy (found during R1) | R5 | ⬜ |
| M8 | Silent mtime fallback | R7 | ⬜ |
| §5.1 | No available-mode test | R2 | ✅ |
| §5.2 | No control-plane catch-up test | R2 | ✅ |
| §5.3 | Latent flakiness (wait/phase E) | R2 | ✅ |
| §5.4 | Listed test debt (GC, repair, bootstrap, skew) | R2 (partial: bootstrap in the R7 test; skew stays ➖ documented) | 🔧 (still open after R2: tombstone-GC + blob-repair tests; bootstrap arrives with the R7 readiness test; skew = doc-only in R9) |
| §5-deploy | Inconsistent HAProxy fall/rise; k8s probes | R9 | ⬜ |
| §5-doc1/2/3 | Available LWW, runbooks, SSE-C/quorum | R9 | ⬜ |
| D1 | Ghost quorum (unauthenticated liveness, drift ignored) | R3 | ✅ |
| D2 | No syncing state (404s/partial listings at re-entry) | R7 | ⬜ |
| D3a | No nodes > cluster_size guard | R3 | ✅ |
| D3b | Secret rotation without dual-secret | R4 (+ runbook R9) | ⬜ |
| D3c | Restore from backup: seq rewind vs HWM | R7 (+ runbook R9) | ⬜ |
| D4 | Multipart without reconcile; local-only concat | R5 | ⬜ |
| D5 | LB blind to writability (client-visible 503s) | R9 (doc; write-aware to be decided) | ⬜ |
| D6 | Console incoherent on per-node views | R8 | ⬜ |
| D7 | Quorum reads weaker than the CP label | R9 (doc + sticky reference) | ⬜ |
| D8 | No real-partition test | R2 | ✅ |
| D9 | TD-016 underestimated (RBAC/bucket_config) | R5 | ⬜ |
| D10 | WORM in cluster: trust model undocumented | R9 | ⬜ |
| D11 | Available RPO undeclared | R9 | ⬜ |
| D12.1 | Receive side without a node-local key filter | R4 | ⬜ |
| D12.2 | Export/import rewrites node_id | R5 | ⬜ |
| D12.3 | Failure detector without tolerance | R3 | ✅ |
| D12.4 | Non-empty single-node merge undocumented | R9 | ⬜ |

## Estimate and sequence

R1 ≈ 2-3 d · R2 ≈ 1-2 d · R3 ≈ 1-2 d · R4 ≈ 1 d · R5 ≈ 2-3 d · R6 ≈ 1 d · R7 ≈ 1-2 d · R8 ≈ 1 d · R9 ≈ 1 d → **~11-16 effective days**. The R1→R9 order is binding only where there is a technical dependency (R2 verifies R1; R3 provides the `ping` used by R7; the rest can be reordered if needed).

## Out of scope (explicitly deferred)

- Read-quorum / linearizable reads (stays future hardening, as per the Phase 29 plan).
- HLC instead of NTP+LWW.
- A write-aware health check as the LB default (D5: documented; implemented only if decided in R9).
- Automated clock-skew tests (documented as a limit, the NTP constraint is already in `ha.md`).
- Erasure coding, sharding, gossip: out of scope as per the Phase 29 plan.
