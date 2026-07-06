# Storage Reclamation (Blob Garbage Collection)

Arca stores each object's bytes as a **blob file** on disk, with the authoritative
reference held in the metadata database. Deleting an object removes the metadata row first,
then the blob file. Because the metadata delete is the point at which the object stops being
retrievable (a subsequent `GET` returns `404`), the object is considered deleted from S3's
point of view as soon as its row is gone — the blob bytes are an internal storage detail.

That ordering is deliberate (it mirrors the write order and keeps [disaster
recovery](recovery.md) able to rebuild the database from blobs), but it means the safe
failure mode is an **orphan blob**: a blob file on disk that no live object row, in-progress
multipart part, or non-orphan composite sidecar references. Orphans arise from:

- interrupted uploads (blob written, object row never committed);
- overwrites (the previous blob is unlinked after the new row is written);
- a crash between the metadata delete and the blob delete;
- a blob-delete failure during object deletion — this is logged and does **not** fail the
  request (the object is already gone), so the caller correctly sees success.

Orphans waste disk but are never served and never corrupt data. Reclaiming them is the job
of blob garbage collection, described below.

## Detecting orphans — `arca fsck`

`arca fsck` reports orphaned blobs (`ORPHANED_BLOB`) without deleting anything. Use it to
measure the backlog before and after a reclamation run. On large stores it walks every blob
file and loads the full database, so it is slow but safe. See the
[CLI reference](../guide/cli.md#arca-fsck).

## Reclaiming orphans

There are three reclamation paths. All share the same **composite-aware** selection (a
composite object's part blobs are kept alive while the composite is still referenced) and
the same **fail-safe** rule (if any enumeration fails, the pass deletes nothing).

### The grace window

Every path protects **freshly-written** blobs with a grace window, because a blob file
exists on disk *before* its object row is committed — an in-flight upload looks exactly like
an orphan. The grace must exceed the longest expected upload (including large multipart
uploads). Only reduce it to `0` when you are certain no writes are in flight (for example,
the server is stopped).

### 1. `arca gc` — on-demand / cron (single-node)

The offline command reclaims orphans on demand. It previews by default; pass `--reclaim` to
delete.

```bash
# Preview (safe; nothing deleted)
arca gc

# Reclaim, protecting blobs younger than 24h (default)
arca gc --reclaim

# Reclaim everything unreferenced, server stopped
arca gc --reclaim --grace-seconds 0
```

This is the recommended mechanism on **large** single-node stores: run it from cron so the
full-store scan runs in its own process, on your schedule, isolated from the serving process.
Full option list: [`arca gc`](../guide/cli.md#arca-gc). Currently supports the `sqlite`
metadata backend.

### 2. Opt-in background worker (single-node)

For hands-off operation without an external scheduler, enable the in-process worker:

```toml
[storage]
blob_gc_enabled = true              # default: false
blob_gc_interval_seconds = 3600     # how often to scan (default: 1h)
blob_gc_grace_seconds = 86400       # protect blobs younger than this (default: 24h)
```

The worker runs the same reclamation on the configured interval. It is **skipped when
clustering is enabled** (the cluster path below handles it). On very large stores prefer
cron-ing `arca gc` instead, so the periodic full-store scan does not run inside the serving
process. See [Configuration → Storage](../guide/configuration.md#storage).

### 3. Cluster anti-entropy worker (automatic, multi-node)

When [High Availability](../guide/ha.md) clustering is enabled, the anti-entropy worker
reclaims orphans automatically as part of its periodic self-heal, using the cluster's
tombstone grace. No configuration and no single-node worker/CLI are required (though
`arca gc` still works offline if you need it). This is why the opt-in single-node worker is
disabled under clustering.

## Metrics

Two Prometheus counters (exposed on the [admin metrics endpoint](monitoring.md)) make
orphan creation and reclamation observable:

| Metric | Meaning |
|--------|---------|
| `arca_blob_delete_failures_total` | Blob-delete failures during object deletion — each one leaves an orphan for GC. A rising value means GC needs to run. |
| `arca_blobs_reclaimed_total` | Orphan blob files reclaimed by the single-node background worker. |

## Which should I use?

| Deployment | Recommended |
|------------|-------------|
| Single-node, small/medium store | Opt-in background worker, or cron `arca gc` |
| Single-node, large store (millions of blobs) | Cron `arca gc` (isolates the heavy scan) |
| Cluster (HA) | Nothing — the anti-entropy worker reclaims automatically |
