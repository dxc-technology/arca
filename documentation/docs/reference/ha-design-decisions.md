# HA Design Decisions

These are the design decisions behind Arca's HA clustering, taken during the HA hardening that shipped in **v0.26.0** (Phase 29.1, the remediation of the Phase 29 review). Code comments cite them as "decision H*n*" (for example `decision H4` in `cluster_meta.rs`); each section below has a stable anchor (`#h1` to `#h12`) so those citations stay resolvable.

Each decision states what the code does **today**, which is not always what was first planned: where a decision was refined later, the section says so.

- For how to run a cluster, see the operator guide: [High Availability](../guide/ha.md).
- For the overall system design, see [Architecture](architecture.md).
- The review and planning labels that code comments and the changelog still use (§3.2, D3a, M1, N2, ...) are explained in [Other named mechanisms](#other-named-mechanisms) at the end of this page.

| # | Decision | Since |
|---|---|---|
| [H1](#h1) | True write quorum: count fan-out ACKs, `503` on shortfall, no rollback | v0.26.0 |
| [H2](#h2) | An ACK means "row applied and blob present" | v0.26.0 |
| [H3](#h3) | PostgreSQL commit-ordered `seq` cursor via a counter table | v0.26.0 |
| [H4](#h4) | ACK counting covers object data-plane mutations only | v0.26.0 |
| [H5](#h5) | Symmetric worker-leader gate (lowest eligible `node_id`) | v0.26.0 (maintenance worker since v0.28.0) |
| [H6](#h6) | Over-sized membership closes the write gate, no escape hatch | v0.26.0 |
| [H7](#h7) | A config-drifted peer is out of the quorum | v0.26.0 |
| [H8](#h8) | Secret rotation via `secret_previous` | v0.26.0 |
| [H9](#h9) | Per-node admin views through a server-side proxy | v0.26.0 |
| [H10](#h10) | Rolling upgrades: additive wire changes | v0.26.0 (broken once by v0.29.0) |
| [H11](#h11) | One release for the whole hardening plan | v0.26.0 (release history) |
| [H12](#h12) | Authenticate the peer, not just the request | v0.26.0 |

## H1: True write quorum {#h1}

**Decision.** In `quorum` mode an object write is acknowledged only when at least `write_quorum` nodes (`floor(cluster_size / 2) + 1`) durably hold it at acknowledgement time: the local copy plus every peer that returned a full ACK (see [H2](#h2)). The origin writes locally, fans out to all eligible peers in parallel, counts the ACKs and, if `1 + acks < write_quorum`, answers `503 ServiceUnavailable` with `Retry-After: 5`. A fast admission gate runs before the local write and refuses early when membership already knows too few eligible nodes are reachable; the ACK count is the authoritative check, because it also sees peers that membership still believes alive but that did not receive the copy.

The local copy is **not rolled back** on a shortfall. The error means "not acknowledged as replicated", not "undone": anti-entropy later propagates the local copy, or a client retry overwrites it. In `available` mode the ACKs are ignored.

**Rationale.** Before v0.26.0 the quorum was only an admission gate: the fan-out swallowed every error, so a write admitted with two live nodes could exist on one disk and still be answered `200 OK`. That broke the CP promise in two ways: a lost acknowledged write if that node died, and conflicting acknowledged writes inside the failure-detection window right after a partition. The fan-out was already awaited before the response, so counting its results cost almost nothing.

**Alternatives rejected.**

- *Keep the admission gate and reword the documentation* ("admission-gated, best-effort replication"). Near-zero cost, but it would have emptied quorum mode of its value.
- *Roll back the local copy on failure.* That needs a distributed transaction (or a compensating delete that can itself fail and race); no quorum system without distributed transactions offers it, and anti-entropy convergence already gives a well-defined outcome.

**Where in the code.** `quorum_satisfied` and `ClusterState::write_gate` / `WriteGate` in `crates/arca-core/src/cluster.rs`; `check_write_gate`, `ClusterMetadataStore::enforce_ack_quorum`, `fan_out_object` and `fan_out_version_delete` in `crates/arca-server/src/cluster/cluster_meta.rs`; the `Retry-After` header is added for every `ServiceUnavailable` by `s3_error_response` in `crates/arca-proto/src/xml/error_response.rs`.

**Since.** v0.26.0.

## H2: An ACK means "row applied and blob present" {#h2}

**Decision.** `POST /cluster/v1/object` and `POST /cluster/v1/object/delete` answer with a JSON `ClusterObjectAck { applied, has_blob }`. The receiving peer self-certifies what it holds: `applied` means the idempotent LWW upsert (or delete) succeeded, `has_blob` means the sidecar of the blob the row references is present (for a composite multipart object, the composite sidecar). For rows that reference no blob (delete markers, tombstones, version deletes) `has_blob` is vacuously true. Only `applied && has_blob` counts toward [H1](#h1).

**Rationale.** Blob bytes and the metadata row travel separately (the blob first, then the row). Letting the peer certify both in one response avoids threading per-write state between the blob fan-out and the row fan-out on the origin. Multipart parts already fan out individually as they are uploaded, so the composite sidecar is the right thing to certify at completion.

**Alternatives rejected.** Tracking blob delivery on the origin and correlating it with the row ACK: more state, more failure modes, and the peer is the only party that knows what it durably holds.

**Where in the code.** `ClusterObjectAck` in `crates/arca-core/src/cluster.rs`; `receive_object` and `receive_version_delete` in `crates/arca-proto/src/handlers/cluster.rs` (producers); `ClusterClient::send_object`, `send_version_delete` and `parse_ack` in `crates/arca-server/src/cluster/client.rs`; `fan_out_object` in `crates/arca-server/src/cluster/cluster_meta.rs` (consumer).

**Since.** v0.26.0.

## H3: PostgreSQL commit-ordered `seq` cursor {#h3}

**Decision.** On PostgreSQL the node-local object write cursor (`seq`, the key of the changed-since manifest that anti-entropy pulls) comes from a single-row counter table, `object_seq`, advanced with `UPDATE object_seq SET value = value + 1 RETURNING value` **inside the same transaction as the row**. The counter's row lock is held until commit, so `seq` order equals commit order: by the time a reader sees `seq = N+1` committed, `N` is either committed and visible or aborted for good (a harmless gap). This mirrors the SQLite backend, which already used a counter table. Every objects-writing transaction takes the `seq` before its first row-mutating statement (a uniform lock order that rules out deadlocks).

**Rationale.** The original PostgreSQL `SEQUENCE` is not transactional: a transaction holding `seq = 100` could still be in flight while `seq = 101` committed. A peer pulling the manifest saw 101, moved its high-water mark past 100, and the row committed later with 100 was never delivered by the incremental sync: silent replication loss under concurrent writes.

**Cost.** Concurrent object writes on PostgreSQL serialize on the counter row for the final stretch of their transaction (from taking the `seq` to commit). Accepted: it is no worse than SQLite, whose single write connection serializes every write entirely.

**Alternatives rejected.** A probabilistic guard window (do not return the newest N rows, or the last T milliseconds, or read only up to a snapshot-safe watermark). It closes most of the window but not all of it, and makes correctness depend on a tuning value. A commit-ordered cursor is exact.

**Where in the code.** Migration `crates/arca-storage/src/pg/migrations/0009_commit_ordered_seq.sql` (creates `object_seq`, seeded from the larger of `MAX(seq)` and the old sequence, and drops the sequence); `next_object_seq` in `crates/arca-storage/src/pg/metadata.rs`, called by every write path.

**Since.** v0.26.0.

## H4: Scope of ACK counting {#h4}

**Decision.** [H1](#h1) ACK counting covers **object data-plane mutations only**: `PutObject` (and every path that writes an object row), the versioned delete marker, and version hard deletes (tombstones in cluster mode). Everything else passes **only the admission gate** and then fans out **best-effort** (the ACKs are not counted):

- control-plane operations: buckets, bucket config, bucket tags, multipart upload and part rows, credentials, users, teams, grants, server settings;
- object tags;
- Object Lock changes (retention, legal hold) and the re-encryption row updates of the maintenance jobs.

These are repaired by anti-entropy if a peer missed the fan-out (the control-plane snapshot merge, or the object manifest for lock and re-encryption changes, which stamp a fresh `seq`). **Object tags are the exception**: they bump no `seq` and are not part of the control-plane snapshot, so a peer that misses the fan-out keeps its old tags until the object is rewritten (TD-033 in [Technical Debt](../tech-debt.md)).

**Rationale.** Control-plane mutations are rare and small, and the periodic full-snapshot reconcile converges them. Extending ACK counting to them was possible but not needed to close the review's data-loss findings; it stays a possible later extension.

**Alternatives rejected.** ACK-counting every replicated mutation in v0.26.0: a much larger change (every control-plane decorator, every receive handler) for mutations whose loss window is already bounded by one reconcile cycle.

**Where in the code.** The scope note in the module doc of `crates/arca-server/src/cluster/cluster_meta.rs`; `put_object_if`, `delete_object_if` and `delete_object_version_if` (counted) versus `fan_out_op` and `replicate_lock_change` (best-effort) in the same file; `fan_out_op` in `crates/arca-server/src/cluster/cluster_control.rs` for the identity control plane.

**Since.** v0.26.0.

## H5: Symmetric worker-leader gate {#h5}

**Decision.** Background work that reads fully replicated state runs on one node only: the **worker leader**, the node whose `node_id` is the lowest among the **eligible** nodes (alive, authenticated and config-aligned peers, plus itself). Every node computes this locally from its membership view; there is no election. When the leader dies, the next-lowest eligible node takes over at its next tick. Two gated workers exist today:

- the **lifecycle worker** (expirations, noncurrent-version deletes, stale multipart aborts);
- the **maintenance worker** (the re-encryption and migration jobs added in v0.28.0); a non-leader also never holds the maintenance drain.

The **Phase 28 bucket replication worker is deliberately not gated**: its journal is node-local (an entry is written only by the node that served the client write, never by cluster replication), so each write is journaled exactly once cluster-wide and deliveries are already exactly-once. Gating it would orphan the entries journaled on non-leader nodes. The metrics snapshot, the retention purge and the notification delivery worker are not gated either: they work on node-local state.

During a membership disagreement two nodes can briefly both claim the role; the resulting double execution is accepted because the gated work is idempotent and converges.

**Rationale.** Without a gate every node ran the lifecycle tick over the same replicated table: N times the deletes, N times the fan-out, duplicate audit entries, racing deleters. The predicate is *eligible*, not merely *alive*, for the same reason the quorum uses it: an unauthenticated rogue (or a drifted peer) with a low `node_id` must not be able to take the role and silence the workers cluster-wide.

**Alternatives rejected.** A leader election or lease protocol: more machinery than a deterministic, locally computed rule for idempotent work. Gating the replication worker, as the review originally proposed: based on the wrong premise that every node journals every write.

**Where in the code.** `ClusterState::is_worker_leader` in `crates/arca-core/src/cluster.rs`; the gate in the lifecycle worker in `crates/arca-server/src/worker.rs` and in `spawn_maintenance_worker` in `crates/arca-server/src/maintenance.rs`; the reason the replication worker is not gated is in the module doc of `crates/arca-server/src/replicator/worker.rs`. Exposed as `worker_leader` on `/admin/cluster` and in `/admin/health?verbose=1`.

**Since.** v0.26.0 (lifecycle worker); the maintenance worker has been gated since it was introduced in v0.28.0.

## H6: Over-sized membership fails closed {#h6}

**Decision.** In `quorum` mode, if a node observes **more eligible nodes than `cluster_size`** (itself included), its write gate closes: writes are refused with a distinct `503` message pointing at the resize runbook, `/admin/cluster` reports `size_exceeded: true`, and an error is logged on each transition. There is no configuration switch to turn the guard off.

Only eligible nodes count, so an unauthenticated stranger cannot close the gate (no write denial of service via an mDNS registration), while a correctly configured extra node can.

**Rationale.** The majority is derived from `cluster_size`, not from what is observed. With four nodes and `cluster_size = 3` every node computes a quorum of 2, so two partitioned pairs can both accept conflicting writes: split-brain in the very mode that promises to exclude it. The identical config means drift detection cannot catch it. A misconfiguration that dangerous must fail closed rather than warn.

**Alternatives rejected.**

- *A warning only*: leaves the split-brain possible.
- *An escape-hatch setting* to accept the over-sized membership: a foot-gun. It would be turned on during some incident and forgotten, silently re-enabling split-brain. The supported path is the cold resize runbook in the [HA guide](../guide/ha.md#resizing-the-cluster).

**Where in the code.** `WriteGate::SizeExceeded` and `ClusterState::write_gate` in `crates/arca-core/src/cluster.rs`; the 503 mapping in `check_write_gate` in `crates/arca-server/src/cluster/cluster_meta.rs`; the transition log in `crates/arca-server/src/cluster/membership.rs`.

**Since.** v0.26.0.

## H7: A config-drifted peer is out of the quorum {#h7}

**Decision.** A peer whose cluster-critical configuration differs from ours (`config_ok = false`) is **not eligible**: it receives no fan-out, is not pulled from by anti-entropy, does not count toward the write quorum or the capacity minimum, and cannot be the worker leader. It stays visible in `/admin/cluster` with the drift flag. Drift is detected two ways: the peer's config fingerprint (exchanged on the authenticated ping) differs from ours, or the peer answers our ping with `403` because it rejects our secret.

**Rationale.** A node with a different secret cannot authenticate the replicas it is sent; a node with a different master key cannot decrypt them. Counting it toward the quorum would acknowledge writes that are not really held by a majority.

**Alternatives rejected.** Refusing to start, or isolating the drifted peer's process: a node cannot know its peers' configuration at startup, and one misconfigured node must not take down the healthy ones. The cluster surfaces the problem loudly and keeps running.

**Where in the code.** `PeerNode::eligible` and `config_fingerprint` in `crates/arca-core/src/cluster.rs`; `contact_config_ok` and the probe verdicts in `crates/arca-server/src/cluster/membership.rs`. The fingerprint covers `cluster_id`, `mode`, the write quorum, the secret and the master-key id; it does not include `cluster_size` itself (TD-036 in [Technical Debt](../tech-debt.md)).

**Since.** v0.26.0.

## H8: Secret rotation via `secret_previous` {#h8}

**Decision.** An optional `[cluster] secret_previous` makes a secret rotation a rolling operation. Inbound authentication tries `secret` first and then `secret_previous` (each with a full constant-time verification); outbound signing and the config fingerprint always use `secret` only. The ping handler computes its challenge MAC with whichever secret verified the request, so a prober on either side of the rotation can verify the answer; the prober also accepts a MAC keyed by either of its own secrets. Validation applies the same strength rules to `secret_previous` and rejects it when it equals `secret`.

Runbook: set `secret_previous` to the old secret and `secret` to the new one on every node, rolling-restart, then remove `secret_previous`.

**Rationale.** Without a dual-secret window, changing the secret meant stopping the whole cluster: downtime in an HA product.

**Alternatives rejected.** A full stop-and-restart rotation (downtime), or a set of N accepted secrets (more state for no practical gain over one previous secret).

**Where in the code.** `ClusterConfig::secret_previous` and its validation in `crates/arca-server/src/config.rs`; `cluster_auth_middleware` and `MatchedClusterSecret` in `crates/arca-proto/src/middleware/cluster_auth.rs`; `ping` in `crates/arca-proto/src/handlers/cluster.rs`; the dual-key MAC check in `crates/arca-server/src/cluster/membership.rs`.

**Since.** v0.26.0.

## H9: Per-node admin views through a server-side proxy {#h9}

**Decision.** Four admin data families are node-local by design: the audit log, the metrics history, the notification event log and the replication journal. Their list endpoints take a `?node=` selector:

- absent: the serving node's own data (the load balancer's pick), with a top-level `node` field naming it;
- `?node=<node_id>`: the serving node proxies the query to that peer over the signed inter-node transport (`POST /cluster/v1/admin/*`). Only eligible peers are valid targets: unknown node `404`, known but ineligible `503`, unreachable `502`, and a peer's own error is forwarded with its status;
- `?node=all`: the query fans out to every eligible node, rows are merged newest-first by parsed timestamp, each labelled with its source node, and a `sources` array reports each node's total or error. Pagination is per source page, an accepted approximation.

The console exposes this as a node selector in the four views.

**Rationale.** Behind a round-robin balancer every console refresh could show a different node with nothing saying which. The typical deployment exposes only the balancer, so the browser often cannot reach individual nodes at all, and the cluster credential must stay on the server side.

**Alternatives rejected.** Direct browser-to-node calls (CORS, nodes unreachable from the browser, credentials in the browser); cross-node pagination cursors for the merged view (complexity out of proportion to an operator view).

**Where in the code.** `ClusterAdminProxy` in `crates/arca-core/src/cluster.rs`; `ClusterAdminProxyImpl` in `crates/arca-server/src/cluster/client.rs`; `select_node` and `dispatch` in `crates/arca-proto/src/handlers/admin_proxy.rs`; the `/cluster/v1/admin/*` receive handlers in `crates/arca-proto/src/handlers/cluster.rs`; `console/js/node-selector.js`.

**Since.** v0.26.0.

## H10: Rolling upgrades across mixed versions {#h10}

**Decision.** Wire changes to the inter-node protocol are meant to be **additive**: new JSON fields are optional (`#[serde(default)]`) and ignored by older nodes, so a cluster can be upgraded one node at a time. Concretely:

- The membership probe uses the authenticated `GET /cluster/v1/ping`; a `404` means a pre-0.26 peer, which is then read through the public `/cluster/v1/health`. Such a peer is visible as alive but cannot prove it holds the secret, so it is **not eligible** (no fan-out, no quorum contribution) and a warning is logged once. In a 3-node `quorum` cluster the first upgraded node therefore refuses writes until a second node is upgraded.
- Snapshots, ACKs and object rows gained fields with defaults (for example a pre-0.26 control snapshot reads as "no information", never as "everything deleted").
- `ClusterClient::parse_ack` treats a `2xx` response whose body does not parse as a `ClusterObjectAck` as a **full ACK** (the pre-ACK contract answered an empty `200`). This legacy path is practically unreachable today, because fan-out only targets eligible peers and a legacy peer is never eligible.

**The rule was broken once, by v0.29.0.** That release removed the `admin` field from credentials. An upgraded node ignores an incoming `admin` field, but a pre-0.29 node requires it, so credentials replicated from a 0.29 node to an older one fail. **A cluster must not run mixed versions across 0.29.0** (see the [changelog](../changelog.md)).

**Rationale.** An HA product must be upgradable without a full outage.

**Alternatives rejected.** Protocol version negotiation: unnecessary while changes stay additive, and the one break so far was handled by a release note.

**Where in the code.** `probe_peer` and `probe_health` in `crates/arca-server/src/cluster/membership.rs`; `ClusterClient::parse_ack` in `crates/arca-server/src/cluster/client.rs`; the `#[serde(default)]` fields on `ControlSnapshot`, `ClusterPingResponse` and `ObjectRecord` in `crates/arca-core/src/cluster.rs` and `crates/arca-core/src/types.rs`.

**Since.** v0.26.0.

## H11: One release for the whole hardening plan {#h11}

*Release-process history.* The hardening was first planned as a release after its first two milestones followed by per-milestone releases. It was revised to a **single release** at the end of all nine milestones, at least a minor version because quorum mode changed observable behaviour, and that release became **v0.26.0**. Milestones were still committed and pushed as they completed. Nothing in the code depends on this decision.

## H12: Authenticate the peer, not just the request {#h12}

**Decision.** Before v0.26.0 the cluster authenticated the *sender* of every `/cluster/v1/*` request but never the *receiver* of a fan-out: membership admitted any endpoint that answered with the right `cluster_id`, so a rogue process registering itself over mDNS received every new write without knowing the secret. Today a peer must prove itself before it can receive or supply anything, in two layers:

1. **HMAC challenge-response, for every cluster** (plain HTTP included). Each membership probe sends a fresh random nonce on the signed `GET /cluster/v1/ping`; the peer must answer with `HMAC-SHA256(secret, nonce)` (domain-separated, verified in constant time). A peer that answers `200` without a valid MAC is alive but not authenticated. Only authenticated and config-aligned peers ([H7](#h7)) are eligible: eligibility gates the fan-out targets, the write quorum, anti-entropy pulls (pulling from an unauthenticated endpoint would be a data-poisoning vector), the capacity minimum, the worker leader and the `?node=` proxy. The public `/cluster/v1/health` was reduced to `{status, node_id}`, so it no longer leaks a fingerprint that could be brute-forced offline for the secret.
2. **Verified mutual TLS with an operator-distributed CA, on HTTPS clusters, fail-closed.** When `[server.tls]` is enabled, `[cluster.tls]` (`ca_file`, `cert_file`, `key_file`) is **required**; the node refuses to start without it, and there is no "accept invalid certificates" fallback. Inter-node clients verify peers against the cluster CA (in addition to the system roots) and present the node's CA-signed certificate; the listener requests a client certificate without requiring it at the TLS layer (S3 clients share the port), and `cluster_auth` refuses `/cluster/v1/*` requests whose connection did not present a verified one. `arca tls generate-cluster` mints the CA and the per-node certificates.

`[cluster.tls]` without `[server.tls]` is rejected, and so is `[server.tls] ca_file` (mandatory client certificates for every connection) together with `[cluster.tls]`: one listener cannot enforce two client-certificate policies.

Around these layers: a `±15` minute `x-amz-date` replay window on cluster requests, a 2 MiB body limit on the JSON cluster routes, and a minimum secret strength (at least 16 characters, shipped placeholders rejected, a warning for low-entropy values).

**Rationale.** Authenticating requests protects the receiver; only authenticating peers protects the sender's data. The challenge must be answered *to us* over a nonce we chose: a signed `/ping` alone proves nothing, because a rogue controls its own server and can answer `200` unconditionally. The challenge-response is not a fallback for mTLS: it is the baseline that also protects plain-HTTP deployments (where no certificates exist) against the easy attack, a rogue mDNS registrant. Sniffing and man-in-the-middle on an untrusted network still need TLS, which is where the second, independent factor applies.

**Alternatives rejected.** *Kubernetes-style auto-enrollment* (the first node generates a CA and joining peers get their certificate requests signed over a channel authenticated by the cluster secret). Issuance gated by the standing secret would collapse mTLS to the strength of the secret: whoever has the secret could mint a certificate. An operator-distributed CA is an independent second factor: pushing or pulling cluster data requires the secret **and** a CA-signed key. (Kubernetes mitigates the same problem with short-lived join tokens; that remains a possible future evolution.)

**Where in the code.** `ping_nonce_mac`, `verify_ping_nonce_mac`, `ClusterPingResponse` and `PeerNode::eligible` in `crates/arca-core/src/cluster.rs`; `probe_peer` in `crates/arca-server/src/cluster/membership.rs`; `ping` and `health` in `crates/arca-proto/src/handlers/cluster.rs`; `cluster_auth_middleware` and `ClusterPeerCertVerified` in `crates/arca-proto/src/middleware/cluster_auth.rs`; the client-certificate verifier in `crates/arca-server/src/tls.rs`; `ClusterTlsConfig` and `validate_cluster_transport` in `crates/arca-server/src/config.rs`; `generate_cluster` in `crates/arca-server/src/tls_generate.rs`; `REPLAY_WINDOW_SECS` in `crates/arca-proto/src/middleware/mod.rs`.

**Since.** v0.26.0.

## Other named mechanisms

The review and the hardening plan labelled findings with section numbers and short IDs. Code comments and the changelog still use some of them; this glossary gives each a plain name. All shipped in v0.26.0.

| Label | Plain name | What it is |
|---|---|---|
| §3.2 | Tombstone-GC liveness guard | Tombstones are not purged while any known peer has been unseen for longer than the grace window (it could still need them); the purge is skipped with a warning and `tombstone_gc_blocked` on `/admin/cluster`. `tombstone_gc_blockers` in `arca-core`. |
| §3.7 | Peer trust (the rogue-peer gap) | The security finding that fan-out authenticated no receiver, so a rogue peer got every write without the secret. Closed by [H12](#h12). |
| D2 | Syncing readiness gate | Until a node completes its first reconcile pass toward every eligible peer since startup, the plain `/admin/health` answers `503 {"status":"syncing"}`, keeping it out of load-balancer rotation. `ClusterState::is_syncing`. |
| D3a | Cluster-size guard | More eligible nodes than `cluster_size` closes the write gate. See [H6](#h6). |
| D3c | Cursor rewind detection | A peer reporting (on the ping) an object `seq` counter below what this node already consumed was restored from a backup; the high-water mark is reset and the peer is re-pulled in full. `sync_rewound` in `arca-core`. |
| D6 | Per-node admin views | The `?node=` selector and merged view for the four node-local admin families. See [H9](#h9). |
| M1 | Stuck-entry skip | A manifest entry that fails to apply on 5 consecutive passes is skipped with a warning and counted in `skipped_entries` per peer, so one poison row cannot block a peer's sync forever. `StuckTracker` in `anti_entropy.rs`. |
| M2 | Blob-repair budget | The proactive blob-repair sweep fetches at most `[cluster] blob_repair_budget` blobs (default 100) per anti-entropy tick and resumes where it stopped. `repair_blobs` in `anti_entropy.rs`. |
| M3 | Membership pruning | A peer unreachable for longer than `[cluster] peer_prune_days` (default: the tombstone grace) is removed from membership, which also stops it blocking the §3.2 guard. `membership.rs`. |
| M7 | Identical-row redelivery no-op | `apply_remote_object` skips an incoming row identical to the local one (no rewrite, no fresh `seq`); without it two caught-up nodes redelivered their whole object tables to each other on every pass. `ObjectRecord::same_replicated_content`. |
| N1 | Lock changes visible to anti-entropy | Retention and legal-hold changes stamp a fresh `seq`, so the changed-since manifest redelivers the row to a peer that missed the fan-out. |
| N2 | Lock-state ordering | Lock changes do not bump `last_modified`, so they carry their own LWW timestamp, `lock_updated_at`, and a stale lock-free copy can never overwrite a newer lock state. Re-encryption later got a second, independent register, `content_updated_at`. `ObjectRecord::resolve_replicated`. |
