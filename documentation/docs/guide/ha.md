# High Availability (Clustering)

Arca runs as a **symmetric, self-configuring, shared-nothing cluster**. Every node is identical: there is no primary, no coordinator, no per-node configuration. You give all nodes the same config, they discover each other, and they replicate everything. A client can read and write through any node; a load balancer in front spreads traffic and routes around dead nodes.

This is the **full-replication** model (every node holds a complete copy), targeting small clusters (typically 3 nodes) where availability and operational simplicity matter more than storage efficiency. Sharding and erasure coding are explicitly out of scope (see [Design trade-offs](#design-trade-offs-and-honest-limitations)).

## How it works

### Symmetry and identity

The `[cluster]` section is **byte-for-byte identical on every node**. There is no `node_id` to assign and no peer list to maintain by hand: on first start each node generates a stable `node_id`, persists it in its own data directory, and keeps it across restarts. Adding a node is just starting another process with the same config.

### Discovery

Nodes find each other through one of three mechanisms (`[cluster].discovery`):

| Mode     | How peers are found | Use when |
|----------|---------------------|----------|
| `mdns`   | Multicast DNS on the local segment (default) | bare-metal / VMs on the same L2 subnet |
| `static` | A fixed `seeds` list of endpoints (identical on every node; a node ignores its own entry) | Docker, Kubernetes, or any routed network where multicast is dropped |
| `dns`    | A headless service name that resolves to all peer IPs | Kubernetes `StatefulSet` + headless `Service` |

Discovery only supplies *candidate endpoints*. A node then health-pings each candidate's `/cluster/v1/health`; the response carries the peer's `node_id`, which is how a node recognises (and ignores) itself and learns who is alive.

### Replication — real time, then self-healing

Every mutation fans out to live peers **in real time** over the internal `/cluster/v1/*` API (signed with the shared cluster secret, never the public S3 path). That covers both planes:

- **Data plane** — object rows and blob bytes (`PutObject`, multipart, deletes, retention/legal-hold, tags).
- **Control plane** — buckets, bucket config, credentials, users, teams, grants, server settings.

Real-time fan-out is best-effort. A node that was **down, slow, or unreachable** during a write catches up on its own through several converging mechanisms — this is what makes the cluster self-healing without operator action:

1. **Anti-entropy (objects)** — each node periodically pulls every peer's *changed-since* manifest (an indexed, incremental `seq` cursor) and applies the rows it is missing. Cheap enough to run frequently.
2. **Control-plane reconcile** — the control plane is small, so nodes periodically exchange a full snapshot and merge it last-writer-wins (see [Consistency](#consistency-model)).
3. **Tombstones** — a hard delete leaves a tombstone (a marker, not the row/blob) so the deletion *propagates* and a lagging peer cannot resurrect a deleted object/entity by shipping its stale copy back. Tombstones are invisible to reads and garbage-collected after a grace window.
4. **Read-repair** — a `GET` for an object whose bytes are missing locally fetches them from a peer on the spot.
5. **Blob repair + GC** — on a slower cadence each node proactively fetches blob bytes it has the row for but not the file (durability), and reclaims orphan blobs left after deletes (composite-multipart-aware, so live parts are never deleted).

### Consistency model

Two policies, set per cluster with `[cluster].mode`:

- **`quorum` (CP, default)** — a write must reach a **majority** of nodes (`floor(cluster_size/2) + 1`) to be acknowledged. Below majority a node refuses writes with `503 ServiceUnavailable` and stays **read-only**, so the cluster never diverges into conflicting writes. With `cluster_size = 3` the write quorum is `2`: the cluster tolerates losing **one** node and keeps serving reads and writes.
- **`available` (AP)** — any single node accepts writes and fans out best-effort. Maximum availability, at the cost of accepting writes that may momentarily diverge and converge later.

Both modes are **eventually consistent** across nodes: replication and reconcile are asynchronous, and conflicts resolve **last-writer-wins (LWW)**. The LWW key is `(last_modified, version_id, blob_id)` — the `blob_id` is a stable tiebreaker so two nodes that wrote the "same" null-version object at the same wall-clock instant still pick the same winner deterministically, without a coordination protocol.

> **Read-after-write:** within a single node it is immediate. Across the cluster (through a round-robin load balancer) a read may briefly hit a node that has not yet received the write. Pin a client to one node (LB sticky sessions) if you need read-your-writes through the balancer.

## Prerequisites

- **Same `[cluster].secret` on every node.** It authenticates all inter-node traffic. There is no per-node credential.
- **Same encryption master key / same KMS on every node**, *if* encryption is enabled. Replication ships the encrypted bytes verbatim, so every node must be able to unwrap them. (SSE-C needs nothing special — the bytes are opaque and the nonce travels in the sidecar.)
- **Synchronized clocks (NTP).** LWW compares wall-clock timestamps; severe skew can pick the wrong winner on null-version rows. The `blob_id` tiebreaker mitigates ties, but NTP is still required.
- **Full connectivity between nodes** on the cluster port, plus the same `cluster_id`. `mdns` additionally needs a shared L2 subnet; otherwise use `static` or `dns`.
- **No data migration.** Enabling clustering on a node that already has data just works — anti-entropy populates the peers. This honours Arca's "configuration change without data migration" guarantee.

## Quick start

### Single-node dev cluster

To exercise the cluster code path and the console topology widget on one machine:

```bash
bin/arca start -d --dev --cluster      # 1-node "available" cluster (mDNS)
bin/console start -d --build           # console on http://localhost:9080
```

The dashboard's **Cluster Topology** card shows the mode, write status, and this single node.

### Three-node local cluster

A self-contained 3-node cluster with an HAProxy load balancer, for integration testing and demos:

```bash
bin/cluster up -d            # arca-1/2/3 + HAProxy + console
bin/cluster status           # container + per-node /admin/health view
```

Endpoints:

| Service       | URL |
|---------------|-----|
| Load balancer | `http://localhost:9000` (round-robin over the live nodes) |
| Console       | `http://localhost:9080` |
| Node 1/2/3    | `http://localhost:9001` / `:9002` / `:9003` (direct, for inspection) |

Simulate a failure and watch the cluster react:

```bash
bin/cluster node-stop 3      # kill a node — the LB drops it, peers mark it dead
bin/cluster status           # now 2/3 live; still writable (2 ≥ quorum)
bin/cluster node-start 3      # it rejoins and catches up via anti-entropy
bin/cluster down             # stop everything (add -v to wipe the data volumes)
```

The 3 nodes share one symmetric config (`docker/cluster/config.toml`, `mode = "quorum"`, `discovery = "static"`) — Docker's bridge network drops multicast, hence static seeds.

## Observability

- **Console** — the dashboard **Cluster Topology** card shows the consistency mode (Quorum·W=N / Available), the write status (Writable / Read-only when quorum is lost), and every node with a green/red status dot, the local-node badge, endpoint, and last-seen time. The Server card's *Topology* field summarises it as `Cluster · live/total`.
- **`GET /admin/cluster`** (admin SigV4) — JSON the console consumes: `mode`, `write_quorum`, `has_write_quorum`, `live_node_count`, `node_count`, and the `nodes` list. Returns `{"enabled": false}` on a single-node deployment.
- **`GET /admin/health?verbose=1`** (unauthenticated) — liveness plus the cluster snapshot, handy for scripts and load-balancer debugging. The plain `GET /admin/health` (200 / 503-on-drain) is the load-balancer check.

### Config-drift detection

A symmetric cluster only works if the alignment-critical config is identical on every node, and the dangerous mismatches are *silent*: a wrong `secret` lets a node look alive while every replication request 403s; a different encryption master key makes replicated blobs unreadable on the peer. So nodes actively check it.

Each node computes a **fingerprint** (a one-way hash, exposing nothing sensitive) of the fields that must match — `cluster_id`, `secret`, `mode`, `cluster_size`, and the encryption master-key id — and advertises it on `/cluster/v1/health`. Every node compares each peer's fingerprint to its own. On a mismatch:

- it **logs a `WARN`** naming the offending peer (once, on transition — not every tick);
- the console **Cluster Topology** card shows an amber warning banner and an alert icon on the mismatched node;
- `GET /admin/cluster` reports `config_aligned: false` and `config_ok: false` on that node.

The cluster does **not** refuse to start or auto-isolate the peer — a node can't know its peers' config at startup, and one misconfigured node shouldn't take down the healthy ones. It surfaces the problem loudly and keeps running; you fix the config and the warning clears on its own. (Fields that legitimately differ per node — port, `advertise_addr`, `node_id`, seeds, intervals, storage backend — are deliberately excluded from the fingerprint.)

## Production deployment

### Load balancer

Put any L7/L4 balancer in front and health-check `GET /admin/health` (200 = up, 503 = draining → out of rotation). The reference HAProxy backend:

```haproxy
backend arca_nodes
    balance roundrobin
    option httpchk
    http-check send meth GET uri /admin/health
    http-check expect status 200
    server arca-1 10.0.0.1:9000 check inter 2s fall 2 rise 1
    server arca-2 10.0.0.2:9000 check inter 2s fall 2 rise 1
    server arca-3 10.0.0.3:9000 check inter 2s fall 2 rise 1
```

For an active/passive VIP, pair HAProxy with keepalived. In Kubernetes, run a `StatefulSet` of 3 replicas with a headless `Service` and `discovery = "dns"` pointed at it; front it with a normal `Service` / Ingress.

### A note on the load balancer being a SPOF

The balancer is a single point of failure unless it is itself redundant (keepalived VIP, multiple ingress replicas, or a managed cloud LB). The Arca *cluster* survives node loss; make sure the entry point does too.

## Design trade-offs (and honest limitations)

This section exists so you can judge the approach, not just use it.

- **Full replication, not sharding/erasure coding.** Every node stores everything. Simple and highly available; storage cost is `N×`. Sized for small clusters. Erasure coding and sharding are future work.
- **Eventual consistency, hand-rolled (Dynamo-style), not Raft.** There is no consensus log. Convergence is LWW + anti-entropy + tombstones. This keeps the data path coordination-free and fast, at the cost of the well-known AP/eventual-consistency caveats. It also means correctness rests on the convergence machinery being right — which is why each piece (manifest, tombstones, reconcile, GC) is unit-tested in isolation.
- **No hinted-handoff.** Larger clusters (Cassandra et al.) buffer writes for absent nodes. At N=3 with full replication, frequent incremental anti-entropy plus read-repair recover an absent node quickly enough that the extra machinery isn't worth it.
- **Control-plane catch-up gaps (documented).** Credentials, users, teams, grants and buckets reconcile fully (including deletions, via tombstones). Their *associations and settings* — team memberships, grant attachments, `bucket_config`, bucket tags, server settings — replicate in **real time** but are **not** part of the periodic reconcile yet. A node that was **down during one of those specific changes** may miss it until the entity is touched again. This is a known follow-up, not a data-loss bug.
- **Tombstone grace vs downtime.** A returning node must come back **within `tombstone_grace_days`** (default 7) to learn of deletions that happened while it was away; past the grace, tombstones are GC'd and a very-long-absent node could resurrect deleted data. Set the grace longer than your worst-case planned downtime.
- **Clock dependence.** LWW uses wall-clock time; run NTP. A hybrid logical clock is possible future hardening.

## TOML configuration reference

```toml
[cluster]
enabled    = true
cluster_id = "arca-prod"          # only nodes sharing this id form a cluster
secret     = "change-me"          # shared inter-node auth secret (identical everywhere)
mode       = "quorum"             # "quorum" (CP, default) | "available" (AP)
cluster_size = 3                  # required in quorum mode → write quorum = 2

discovery  = "static"             # "mdns" (default) | "static" | "dns"
seeds      = ["arca-1:9000", "arca-2:9000", "arca-3:9000"]  # discovery = "static"
dns_name   = "arca-headless"      # discovery = "dns"

# Optional: what this node advertises to peers (defaults: auto-detected addr,
# [server].port). Set only behind NAT / port remapping.
advertise_addr = "10.0.0.1"
advertise_port = 9000

health_interval_seconds       = 3     # peer health ping cadence
anti_entropy_interval_seconds = 5     # reconcile pass cadence
request_timeout_seconds       = 30    # inter-node HTTP timeout
tombstone_grace_days          = 7     # MUST exceed worst-case node downtime
```

## Related

- [Encryption](encryption.md) — every node needs the same master key for encrypted clusters.
- [Replication](replication.md) — asynchronous *cross-cluster* mirroring to any S3 endpoint (a different feature from intra-cluster HA; `--cluster` and `--replication` are mutually exclusive).
- [Configuration](configuration.md) — full TOML reference.
