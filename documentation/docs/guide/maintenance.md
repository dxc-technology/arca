# Migration & Maintenance

Arca can change almost everything about a running instance — the encryption state
of existing objects, the metadata backend, the cluster topology — **in place**,
without migrating data to a new instance. This is a hard architectural constraint:
unlike systems that force a fresh deployment when you change a fundamental setting,
Arca always provides a migration path that preserves existing data where it lives.

These operations are exposed two ways:

- **Maintenance jobs** — long-running operations run by a background worker, with
  persisted progress and pause / resume / cancel controls. Re-encryption
  (`encrypt` / `decrypt`) is launched from the **web console** or the admin API.
  Metadata migration (`migrate-db`) is a maintenance job too, but it is launched
  from the **admin API only** (the console UI does not offer it) and **requires
  maintenance mode** — see below.
- **Offline CLI commands** — escape hatches that perform the same transform with
  the server stopped (`arca encrypt-existing`, `arca decrypt-existing`,
  `arca migrate-db`), plus the CLI-only topology transition (`arca migrate-topology`).

## The maintenance-jobs model

A maintenance job is a row in the `maintenance_jobs` table. Because the state is
persisted, progress survives a restart and the job is observable from the console.

- **One job at a time.** Starting a job while another is active (`pending`,
  `running`, or `paused`) is rejected with `409 Conflict`. Cancel the active job
  before launching another.
- **Progress is persisted per item.** The worker commits `done` / `total` as it
  goes, so **pause**, **cancel** and a process restart are always safe — a resumed
  job picks up where it left off, and a cancelled one stops without corrupting
  anything.
- **State-guarded transitions.** A job moves through `pending → running →
  completed` (or `failed` / `cancelled`); pause is allowed only from
  `pending` / `running`, resume only from `paused`, cancel from any non-terminal
  state. Invalid transitions return `409`.

### Live vs maintenance mode

Every job runs in one of two modes:

| Mode | S3 API on the node | Use when |
|------|--------------------|----------|
| **`live`** (default) | Stays fully available — zero downtime | Online, while clients keep reading and writing |
| **`maintenance`** | **Drained** for the job's whole lifetime | You want full speed and can take the node out of service |

In **maintenance** mode the node's health check reports `draining`, so a load
balancer in front stops routing S3 traffic to it while the job runs (and resumes
once the job leaves the active slot). The **admin API and the worker stay live**
throughout, so you can still watch progress and pause / cancel the job. A
maintenance-mode job drains the node even while paused — the slot is held until
the job reaches a terminal state.

### Cluster leader-gating

In a cluster the maintenance worker is **leader-gated**: only the **worker-leader**
node (the lowest `node_id` among the eligible nodes — the same predicate that gates
the cluster write quorum and the lifecycle evaluator) runs jobs. The data these
jobs mutate is fully replicated, so running them on every node would duplicate the
work; a non-leader node never holds the maintenance drain. Leader failover is
automatic if the current leader stops.

## Re-encryption (encrypt / decrypt)

Encrypt existing **plaintext** objects to SSE-S3 (AES-256-GCM), or decrypt SSE-S3
objects back to plaintext, **in place** — without re-uploading and without
migrating to a new instance. This is the tool for turning on at-rest encryption
after objects already exist, or for reverting it.

### What it does

The job scans every bucket's object versions and selects the candidates for the
chosen direction:

- **`encrypt`** — objects currently stored as plaintext (no encryption).
- **`decrypt`** — objects currently stored as SSE-S3 (`AES256`).

Each candidate is rewritten **copy-on-write (COW)**: a brand-new blob is written
(encrypted for `encrypt`, plaintext for `decrypt`), then the object row is swapped
to the new blob with a **compare-and-swap** against the old blob id. Because the
swap is atomic and the old blob stays in place until it succeeds:

- **Concurrent readers always see a consistent blob** — they get either the old or
  the new one, never a half-written file. This is why re-encryption is safe to run
  **live**, with zero downtime.
- **The ETag and Last-Modified are preserved** — re-encryption does not change the
  object's identity or its lifecycle clock. (The ETag is computed on plaintext, so
  it is the same before and after.)
- If a **client overwrites the object** while the job is mid-flight, the CAS loses
  and the freshly written blob is discarded — the client's newer write wins.

The orphaned old blob is then reclaimed: by the encrypting store's delete on a
single node, and by the cluster's blob GC in a cluster.

!!! warning "Skipped objects (TD-014)"
    Two object kinds are **skipped** and left as they are:

    - **SSE-C objects** (customer-provided keys) — the server never holds the key,
      so it cannot re-encrypt them.
    - **Multipart / composite objects** — the optimised `CompleteMultipartUpload`
      stores them as composite blobs that re-encryption does not yet rewrite
      (tracked as TD-014).

### Live mode and the throttle

In **live** mode the job uses COW + CAS (as above) and honours an optional
byte-rate throttle (`rate_bytes_per_sec`, `0` = unlimited): after each object it
sleeps proportionally to the object size, so you can cap the I/O the job adds to a
busy server. In **maintenance** mode the S3 API is drained and the job runs at full
speed (the throttle is ignored).

!!! note "Cost in a cluster"
    Re-encryption **rewrites every blob once** (COW gives each object a new blob
    id). In a cluster, those new blobs are immutable and propagate to peers via
    read-repair and anti-entropy — so a re-encryption pass triggers a **one-time
    full re-replication** of the rewritten data across the cluster. Plan for the
    extra inter-node traffic and disk churn: use the live-mode throttle, run it
    during a quiet window, and expect the anti-entropy worker to be busy until the
    cluster reconverges.

### Console usage

Open the **Maintenance** page, choose **Encrypt objects (SSE-S3)** or **Decrypt
objects** as the job type, pick **Live** or **Maintenance** mode, optionally scope
to a single **bucket** and / or key **prefix**, set a **throttle** (live mode only),
and launch. The active-job card shows progress, ETA and rate; per-object errors are
logged without aborting the run.

Re-encryption requires encryption to be configured (a master key). On an instance
without encryption the job fails cleanly with a clear message.

### Offline CLI

For disaster recovery (server stopped), the same transform is available as an
escape hatch:

```bash
# Encrypt all existing plaintext objects to SSE-S3, in place
arca encrypt-existing --config-path /etc/arca/config.toml

# Decrypt SSE-S3 objects back to plaintext
arca decrypt-existing --config-path /etc/arca/config.toml

# Preview only — report what would change, write nothing
arca encrypt-existing --dry-run

# Scope to a bucket and/or prefix
arca encrypt-existing --bucket my-bucket --prefix logs/
```

## Metadata migration (migrate-db)

Move **all metadata** from one backend to the other — SQLite ⇄ PostgreSQL — **in
place**, without re-uploading data. Blob files are filesystem-resident and are
**not touched**; only the metadata database moves.

### How it works

The copier is a single generic, type-aware column walker driven by a static
description of every metadata table. It:

- copies tables in **foreign-key-safe order**,
- translates each backend's native representation (SQLite `TEXT` timestamps ⇄ PG
  `TIMESTAMPTZ`, SQLite `INTEGER` booleans ⇄ PG `BOOLEAN`, SQLite `TEXT` JSON ⇄ PG
  `JSONB`),
- **reconciles source and destination row counts per table** — a mismatch aborts
  the run, naming the offending table,
- migrates / reseeds the `object_seq` write counter and the audit / metrics
  surrogate sequences, so writes on the new backend cannot collide.

It refuses a destination that already holds operator data unless `--force` (which
fully replaces it); the always-present schema baseline (root user, built-in grants,
seq counter) is ignored.

### Running it

As a **maintenance-mode job** (`type: "migrate-db"`, params `{ "target", "force" }`,
launched via the admin API — the console UI does not offer it). The job **requires
maintenance mode**: a `live`-mode request is rejected, because the copy walks every
table across separate read transactions and a concurrent write mid-copy would land
in some already-dumped tables but not others, producing a torn destination that
per-table row-count reconciliation cannot detect. In maintenance mode the S3 API on
its node is drained while it copies. As an **offline CLI escape hatch** (server
stopped, so no writes can race the copy):

```bash
# Migrate metadata from the configured backend to PostgreSQL
arca migrate-db --to postgres --config-path /etc/arca/config.toml

# Migrate to SQLite, replacing any existing destination data
arca migrate-db --to sqlite --force
```

### After a run

Migration only moves the data; it does not switch the running backend. To complete
the move:

1. Edit the config: set `metadata_backend` to the new backend (and add a
   `[storage.postgres]` section when migrating to PostgreSQL).
2. Restart Arca onto the new backend.

!!! note "Online direction constraint"
    The **online** (maintenance-job) direction is **postgres → sqlite**: Arca
    auto-detects PostgreSQL as the running backend whenever a `[storage.postgres]`
    section is present, so a live instance whose source is PostgreSQL can migrate to
    SQLite online. The **sqlite → postgres** direction is done with the **offline
    CLI** (`arca migrate-db --to postgres`), since the running config has no
    PostgreSQL section yet.

## Topology migration (migrate-topology)

Guided in-place transition between a **standalone single node** and an **HA
cluster**. Arca's cluster is **fully replicated** (every node holds every object),
**not sharded**, so there is **no data redistribution** in either direction — this
tool only generates the `[cluster]` config stanza and runs a couple of small
database chores. It is **CLI-only** by design: both directions are inherently
operator-plus-restart actions an online job cannot perform.

```bash
# Single -> HA: make this standalone instance the FIRST node of a new cluster
arca migrate-topology --to-cluster --config-path /etc/arca/config.toml

# Also write the generated [cluster] stanza to a file
arca migrate-topology --to-cluster --output cluster.toml

# HA -> standalone: collapse the cluster back to THIS surviving node
arca migrate-topology --to-single --force
```

### `--to-cluster`

Refuses to run if the instance is already clustered. It:

- emits a ready-to-paste `[cluster]` stanza with a generated `cluster_id`, a strong
  random `secret` (32 bytes hex), `mode = "quorum"`, `cluster_size = 3`,
  `discovery = "mdns"`, and commented templates for static / DNS discovery and
  `[cluster.tls]` (pointing you at `arca tls generate-cluster` for the inter-node
  CA),
- reconciles this node's `object_seq` write counter to `MAX(seq)`, so the first
  clustered write cannot skip a pre-cluster object.

Paste the stanza into the config and restart; clustering (tombstone mode) turns on
automatically. Bring up the additional **empty** nodes with the same config and
they converge via anti-entropy — no data copy.

### `--to-single`

Run on the surviving authoritative node after every peer is confirmed **synced and
stopped** (`--force` is the explicit confirmation — collapsing while a peer is
behind loses its un-replicated writes). It purges the cluster-only state (object
tombstones + control-plane tombstones) and `VACUUM`s SQLite (PostgreSQL autovacuum
needs none). Then strip the `[cluster]` section from the config and restart
standalone.

!!! tip "Full cluster bringup"
    `migrate-topology --to-cluster` produces the first node's config; the complete
    runbook for standing up the remaining nodes, minting inter-node TLS material and
    operating the cluster lives in the [High Availability guide](ha.md).

## Admin API: `/admin/maintenance/jobs`

All endpoints are JSON over the admin API, SigV4-authenticated and admin-only, like
the rest of `/admin/*`. Known job types: `noop`, `encrypt`, `decrypt`,
`migrate-db`.

### Job object

```json
{
    "id": "8f3c…",
    "job_type": "encrypt",
    "status": "running",
    "mode": "live",
    "params": { "bucket": "my-bucket", "rate_bytes_per_sec": 10485760 },
    "total": 1280,
    "done": 412,
    "rate": 37.5,
    "last_error": null,
    "created_at": "2026-06-24T10:00:00Z",
    "updated_at": "2026-06-24T10:02:11Z",
    "started_at": "2026-06-24T10:00:01Z",
    "finished_at": null
}
```

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Job id (UUID) |
| `job_type` | string | `noop` \| `encrypt` \| `decrypt` \| `migrate-db` |
| `status` | string | `pending` \| `running` \| `paused` \| `completed` \| `failed` \| `cancelled` |
| `mode` | string | `live` \| `maintenance` |
| `params` | object | Job-type-specific parameters (see below) |
| `total` / `done` | integer | Progress counters |
| `rate` | number | Current throughput (items/sec) |
| `last_error` | string \| null | Error message if the job failed |
| `created_at` / `updated_at` / `started_at` / `finished_at` | string \| null | ISO 8601 timestamps |

### Create a job

```
POST /admin/maintenance/jobs
```

**Request body**:

```json
{
    "type": "encrypt",
    "mode": "live",
    "params": { "bucket": "my-bucket", "prefix": "logs/", "rate_bytes_per_sec": 10485760 }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `type` | string | Yes | One of the known job types |
| `mode` | string | No | `live` (default) or `maintenance` |
| `params` | object | No | Job-type parameters |

Job-type parameters:

- **`encrypt` / `decrypt`**: `bucket` (optional — all buckets if omitted), `prefix`
  (optional — all keys if omitted), `rate_bytes_per_sec` (optional, live mode only,
  `0` = unlimited).
- **`migrate-db`**: `target` (`"sqlite"` \| `"postgres"`, required), `force`
  (boolean, replace a non-empty destination). Requires `"mode": "maintenance"` —
  a `live`-mode `migrate-db` request is rejected with `400` (the copy must run
  with the S3 API drained; see [Metadata migration](#metadata-migration-migrate-db)).
- **`noop`**: `n` (steps), `delay_ms` (optional per-step sleep) — a test job that
  exercises the subsystem.

**Response** `201`: the created job object.
**Response** `400`: unknown job type, invalid mode, or a `live`-mode `migrate-db`.
**Response** `409`: a job is already active.

### List jobs

```
GET /admin/maintenance/jobs?limit=50
```

**Response** `200`:

```json
{
    "active": { "...": "the in-flight job, or null" },
    "jobs":   [ "...recent history, newest first..." ]
}
```

`limit` caps the history page (default 50, max 500).

### Get one job with logs

```
GET /admin/maintenance/jobs/{id}
```

**Response** `200`:

```json
{
    "job":  { "...": "the job object" },
    "logs": [ { "level": "info", "message": "job started", "...": "..." } ]
}
```

**Response** `404`: no such job.

### Pause / resume / cancel

```
POST   /admin/maintenance/jobs/{id}/pause     # running|pending  -> paused
POST   /admin/maintenance/jobs/{id}/resume    # paused           -> running
DELETE /admin/maintenance/jobs/{id}           # any non-terminal -> cancelled
```

Each returns `200` with the updated job object, `404` if the job does not exist,
and `409` if the job is not in a state from which the transition is allowed.
