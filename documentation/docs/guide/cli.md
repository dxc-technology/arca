# CLI Reference

The `arca` binary provides subcommands for starting the server, managing credentials and users, generating TLS material, maintaining the data in place, inspecting a cluster, and performing disaster recovery. Every subcommand that reads the configuration accepts `--config-path` (default: `/etc/arca/config.toml`) to locate the configuration file; `arca tls` and `arca encryption generate-key` do not read it.

`arca --version` (or `-V`) prints the version of the binary (`arca X.Y.Z`).

## `arca serve`

Start the S3-compatible server.

```bash
arca serve [--config-path <PATH>] [--log-format <FORMAT>]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--config-path` | `/etc/arca/config.toml` | Path to the configuration file |
| `--log-format` | `text` | Log output format: `text` (human-readable) or `json` (structured, for log aggregation) |

The server handles SIGTERM and SIGINT for graceful shutdown — it finishes in-flight requests before stopping.

```bash
# Start with default config
arca serve

# Start with custom config and JSON logging
arca serve --config-path /opt/arca/config.toml --log-format json
```

## `arca credential`

Manage S3 access credentials stored in the SQLite database. See [Configuration — Credentials](configuration.md#credentials) for background.

### `arca credential add`

Generate a new access key pair.

```bash
arca credential add [--description <TEXT>] [--user <USER_ID>]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--description` | | Human-readable label for the credential |
| `--user` | `root` | User ID to associate the credential with |

```bash
# Create a credential for the root user (full access, including the Admin API)
arca credential add --description "my app"

# Create a credential for a specific user
arca credential add --description "alice key" --user alice-uuid
```

A credential carries no privileges of its own: it inherits them from the user
it belongs to. Credentials on a root user have implicit full access, including
the [Admin API](../reference/admin-api.md) and console management; credentials
on any other user are authorized through that user's
[grants](access-control.md).

The generated access key and secret key are printed to stdout. The secret key is shown only once — store it securely.

### `arca credential list`

List all credentials.

```bash
arca credential list [--config-path <PATH>]
```

Shows access key ID, status (active/inactive), owning user, creation date and description for each credential.

### `arca credential remove`

Delete a credential by its access key ID.

```bash
arca credential remove <ACCESS_KEY_ID> [--config-path <PATH>]
```

!!! warning "Lockout Prevention"
    Arca prevents deleting the last active credential, and the last active credential belonging to a root user, to avoid lockout.

## `arca user`

Manage users stored in the SQLite database. This is an offline command that accesses the database directly (the server does not need to be running).

### `arca user create`

Create a new user.

```bash
arca user create <USERNAME> [--description <TEXT>] [--config-path <PATH>]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--description` | | Human-readable description for the user |
| `--config-path` | `/etc/arca/config.toml` | Path to the configuration file |

```bash
# Create a user
arca user create alice

# Create a user with a description
arca user create alice --description "Alice from engineering"
```

### `arca user list`

List all users.

```bash
arca user list [--config-path <PATH>]
```

### `arca user delete`

Delete a user by its user ID.

```bash
arca user delete <USER_ID> [--config-path <PATH>]
```

```bash
# Delete a user by ID
arca user delete 550e8400-e29b-41d4-a716-446655440000
```

## `arca tls generate`

Generate a self-signed CA and server certificate for development and testing.

```bash
arca tls generate [--output-dir <PATH>] [--sans <NAMES>] [--days <N>]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--output-dir` | `/etc/arca/certs` | Directory to write certificate files |
| `--sans` | `localhost,127.0.0.1,::1` | Subject Alternative Names (comma-separated DNS names and IP addresses) |
| `--days` | `365` | Certificate validity period in days |

Generates four files in the output directory:

| File | Description |
|------|-------------|
| `arca-ca.crt` | CA certificate |
| `arca-ca.key` | CA private key |
| `arca-server.crt` | Server certificate (signed by the CA) |
| `arca-server.key` | Server private key |

```bash
# Generate certs for local development
arca tls generate

# Generate certs with custom SANs and validity
arca tls generate --output-dir /etc/arca/certs --sans "myhost.example.com,localhost,127.0.0.1,::1" --days 730
```

After generating, add the TLS section to your config file:

```toml
[server.tls]
cert_dir = "/etc/arca/certs"
cert_file = "arca-server.crt"
key_file = "arca-server.key"
```

Distribute `arca-ca.crt` to clients that need to trust the self-signed certificate.

!!! warning
    Self-signed certificates are suitable for development and internal testing. For production, use certificates issued by a trusted Certificate Authority.

## `arca tls ensure`

Keep a local CA and a server certificate valid, (re)generating only what is
needed. Unlike `arca tls generate`, it is idempotent and safe to run on every
start.

```bash
arca tls ensure --output-dir <PATH> --ca-dir <PATH> [--sans <NAMES>] \
    [--days <N>] [--ca-days <N>] [--renew-within-days <N>]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--output-dir` | *(required)* | Directory for the server certificate, its key and a copy of the CA certificate — the one Arca and the console read |
| `--ca-dir` | *(required)* | Directory for the CA certificate and private key; keep it out of the containers, only renewals need it |
| `--sans` | `localhost,127.0.0.1,::1` | Subject Alternative Names (comma-separated DNS names and IP addresses; order and case do not matter) |
| `--days` | `365` | Server certificate validity in days |
| `--ca-days` | `3650` | CA validity in days |
| `--renew-within-days` | `30` | Renew whatever expires within this many days |

What it does on each run:

| Situation | Result |
|-----------|--------|
| No CA, unreadable CA, or CA key not matching its certificate | New CA, new server certificate |
| CA expiring within the renewal window | New CA, new server certificate (clients must trust the new CA) |
| Server certificate missing, unreadable, expiring, issued for other SANs, or not signed by the current CA | New server certificate, signed by the **same** CA |
| Everything valid | Nothing is written |

Because renewals reuse the CA, a client that trusts `arca-ca.crt` once keeps
trusting every renewed server certificate for the CA's whole lifetime.

| File | Directory | Mode |
|------|-----------|------|
| `arca-server.crt` | output | `0644` |
| `arca-server.key` | output | `0640` |
| `arca-ca.crt` | output and CA | `0644` |
| `arca-ca.key` | CA only | `0600` |

```bash
arca tls ensure --output-dir /certs/local --ca-dir /certs/local-ca \
    --sans "localhost,127.0.0.1,::1,arca"
```

## `arca tls generate-cluster`

Generate a cluster CA and one certificate per node for the verified mutual TLS between cluster nodes (`[cluster.tls]`). See the [High Availability guide](ha.md#inter-node-transport-security-tls-mutual-tls) for when it is required.

```bash
arca tls generate-cluster --node <SPEC> [--node <SPEC> ...] [--output-dir <PATH>] [--days <N>]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--node` | *(required, repeatable)* | One per node: `name` or `name=san1,san2,...`. The SANs must cover every DNS name and IP address peers use to reach the node (seeds entries, advertised addresses); without them the SAN is the name itself. The name becomes a file name, so only letters, digits, `-`, `_` and `.` are allowed. |
| `--output-dir` | `/etc/arca/certs/cluster` | Directory to write certificate files |
| `--days` | `365` | Node certificate validity in days (the CA is valid twice as long) |

| File | Mode | Description |
|------|------|-------------|
| `arca-cluster-ca.crt` | `0644` | Cluster CA certificate: the same `ca_file` on every node |
| `arca-cluster-ca.key` | `0600` | Cluster CA private key: keep it offline, it is only needed to mint more node certificates |
| `<name>.crt` | `0644` | Node certificate, signed by the cluster CA, valid for both server and client authentication |
| `<name>.key` | `0640` | Node private key |

Each node uses its own certificate and key both as its listener certificate (`[server.tls]`) and as its inter-node client identity (`[cluster.tls]`); the command prints the matching configuration snippet. S3 clients must then trust the cluster CA too, or you keep a separate public certificate in `[server.tls]`.

```bash
arca tls generate-cluster --output-dir /etc/arca/certs/cluster \
    --node node-a=node-a.example.com,10.0.0.1 \
    --node node-b=node-b.example.com,10.0.0.2 \
    --node node-c=node-c.example.com,10.0.0.3
```

## `arca encryption generate-key`

Generate a random 256-bit master key for server-side encryption.

```bash
arca encryption generate-key
```

Outputs a base64-encoded 32-byte key to stdout. Use this value for the `master_key` field in the `[encryption]` config section.

```bash
# Generate a key and add it to config
KEY=$(arca encryption generate-key)
echo "[encryption]"
echo "enabled = true"
echo "master_key = \"$KEY\""
```

See the [Encryption guide](encryption.md) for setup instructions.

## `arca recover`

Rebuild the SQLite database from `.meta` sidecar files. Use this for disaster recovery when the database is lost or corrupted. See [Disaster Recovery](../operations/recovery.md) for a detailed guide.

```bash
arca recover [--config-path <PATH>] [--dry-run] [--skip-verify]
```

| Option | Description |
|--------|-------------|
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |
| `--dry-run` | Print what would be recovered without modifying the database |
| `--skip-verify` | Skip MD5 checksum verification of blob files (faster) |

The recover command:

1. Walks `{data_dir}/blobs/` recursively, reading all `.meta` sidecar files
2. Verifies each blob file exists and its MD5 matches the sidecar ETag (unless `--skip-verify`)
3. Preserves credentials from the existing database (if any)
4. Deletes the old database and creates a fresh one
5. Recreates all buckets and objects from sidecar data

Multipart objects (ETag contains `-`) skip checksum verification since the composite ETag is not a simple MD5 of the assembled blob. Encrypted objects also skip checksum verification since the on-disk ciphertext MD5 differs from the plaintext ETag. Orphaned sidecars (no blob file), malformed JSON, and checksum mismatches are skipped with warnings.

!!! warning
    The rebuilt database is not equivalent to the original: only buckets, objects and credentials are restored, the oldest version of each key wins, multipart (composite) objects are dropped, compressed objects are skipped unless `--skip-verify` is given, and the command always writes a SQLite database, even on a PostgreSQL deployment (TD-014, TD-032). Read [Disaster Recovery](../operations/recovery.md) before running it.

```bash
# Preview what would be recovered
arca recover --dry-run

# Full recovery (with checksum verification)
arca recover

# Fast recovery (skip checksum verification)
arca recover --skip-verify
```

## `arca fsck`

Check database and filesystem consistency, and optionally repair what it finds. See [Disaster Recovery](../operations/recovery.md#arca-fsck) for a detailed guide.

```bash
arca fsck [--config-path <PATH>] [--verify-checksums] [--grace-seconds <SECONDS>]
          [--repair [--yes] [--delete] [--abort-uploads-older-than <DAYS>]]
```

| Option | Description |
|--------|-------------|
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |
| `--verify-checksums` | Read every blob file and verify MD5 against stored ETag (slow) |
| `--grace-seconds` | Skip files written within this many seconds (default: `86400`): they may belong to an upload still in flight. Use `0` only with the server stopped |
| `--repair` | Repair what the check finds, asking before each fix (see [Repair](#repair)) |
| `-y`, `--yes` | With `--repair`: answer yes to every question |
| `--delete` | With `--repair`: delete orphaned files instead of moving them to `lost+found` |
| `--abort-uploads-older-than` | With `--repair`: also offer to abort the unfinished multipart uploads initiated more than this many days ago |

It works with both metadata backends (SQLite and PostgreSQL), and refuses to start when the SQLite database file does not exist rather than creating an empty one.

### Check Types

What counts as referenced is exactly what [`arca gc`](#arca-gc) keeps: object rows, the parts of in-progress multipart uploads, and the parts of every completed multipart object. A completed multipart object is a *composite*: its sidecar lists its part blobs and it has no blob file of its own.

| Check | Description |
|-------|-------------|
| `ORPHANED_BLOB` | Blob file referenced by no object, upload or composite |
| `ORPHANED_SIDECAR` | `.meta` file with neither a blob file nor an object (for example the sidecar of a composite whose object is gone) |
| `MISSING_BLOB` | An object version whose blob file, or one of whose composite parts, does not exist |
| `SIDECAR_MISMATCH` | `.meta` sidecar data doesn't match the object row (bucket, key, size, etag, or a version id the sidecar carries) |
| `UNREADABLE_SIDECAR` | `.meta` file that cannot be read or parsed |
| `STALE_TMP` | Leftover temp file of an interrupted write (`<blob>.tmp`, `<blob>.meta.<id>.tmp`) |
| `REWRITE_RECOVERY` | Recovery file of an unfinished in-place rewrite (`*.rewrite.tmp`, `*.meta.pending.tmp`): re-run the rewrite command to complete it, never delete it by hand |
| `CORRUPT` | Blob file MD5 doesn't match stored ETag (only with `--verify-checksums`; multipart, encrypted and compressed objects are skipped) |
| `UNFINISHED_UPLOAD` | Informational: a multipart upload never completed nor aborted, with its start date, part count and size. Its parts are live data until the upload is aborted |

The report ends with a summary: the count of each category and the bytes involved, and how many files were skipped as younger than the grace period.

### Repair

`--repair` fixes the issues one by one and asks before each fix: `y` (yes), `n` (no), `a` (yes to the rest of the category), `q` (stop). `--yes` answers yes to everything; without a terminal, `--repair` requires it.

| Check | Repair | Asks |
|-------|--------|------|
| `STALE_TMP` | Deleted (partial bytes nothing reads) | no |
| `ORPHANED_BLOB`, `ORPHANED_SIDECAR` | Moved to `<data_dir>/lost+found/<timestamp>/`, keeping their path under `blobs/`; deleted with `--delete`. A blob takes its sidecar along | yes |
| `MISSING_BLOB` | The row of that version is deleted (its data is already lost: a GET of it fails). Re-upload the object afterwards | yes |
| `SIDECAR_MISMATCH` | The sidecar is rewritten from the row (bucket, key, size, etag, version id); its encryption, compression and part list are kept | yes |
| `UNFINISHED_UPLOAD` | Aborted (row and part files deleted), only with `--abort-uploads-older-than` and only for uploads older than that | yes |
| `UNREADABLE_SIDECAR`, `REWRITE_RECOVERY`, `CORRUPT` | None: they need a decision `fsck` cannot take | |

How it stays safe:

- **Running server.** Repair may run while the server is up. Files younger than the grace period are never fixed (on an HA cluster node the grace is at least `[cluster] tombstone_grace_days`, like anti-entropy GC), and every item is checked again right before its fix: an item that changed since the scan is skipped. A row is deleted only if it still points at the blob found missing (checked inside the delete's transaction), and the referenced set is recomputed before orphans are touched. A running server may still serve a deleted row from its metadata cache for up to `object_cache_ttl_seconds` (a GET of it already failed before the repair).
- **Leftovers in the same run.** Database fixes run first; if any was applied, `fsck` rescans, so the files a deleted row or an aborted upload left behind are found and offered in the same run.
- **Orphans while a sidecar is unreadable** are not repaired: that sidecar may list the parts of a live composite.
- **Aborting uploads.** An abort deletes the parts, so a client still uploading to that upload fails. If a client is completing that very upload at the same moment, one of the two wins as a whole: a Complete that wins keeps its object intact (the abort finds nothing to delete), and an abort that wins makes the Complete answer `NoSuchUpload` without creating an object. Pick a `--abort-uploads-older-than` well beyond your slowest upload anyway, so you never cancel a transfer that is still running. On an HA cluster the guarantee holds per node: an abort and a Complete landing on different nodes at the same moment are settled by anti-entropy.
- **Interrupting.** Ctrl+C exits at once, except during a fix: the fix completes, then the run stops. Every fix is atomic and idempotent and there is no journal: run `arca fsck --repair` again and it finds only what is left.
- **In-place rewrites.** `--repair` refuses to start while the [in-progress marker](#the-in-progress-marker) of an offline rewrite exists.
- **HA cluster.** On a cluster node only the file repairs run (`STALE_TMP`, orphans, sidecar mismatches). `MISSING_BLOB` is left to anti-entropy, which fetches the blob from a peer; deleting the row there would replicate as a tombstone and delete the object on every node. Unfinished uploads are aborted through the S3 API (`AbortMultipartUpload`), which replicates. Run `fsck` on each node.

`lost+found` must be on the same filesystem as `blobs/` (it is, unless `blobs/` is a separate mount); otherwise use `--delete`. Delete the `lost+found` directory once you are satisfied.

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | No issues found (with `--repair`: none left) |
| 1 | One or more issues detected (with `--repair`: left unfixed) |

Unfinished uploads alone do not make the exit code 1: they are valid state.

```bash
# Quick consistency check
arca fsck

# Full check including blob checksums (slow for large datasets)
arca fsck --verify-checksums

# Repair, asking before each fix
arca fsck --repair

# Repair everything with the server stopped, aborting uploads older than a week
arca fsck --repair --yes --grace-seconds 0 --abort-uploads-older-than 7
```

## `arca gc`

Reclaim orphaned blob files — on-disk blobs referenced by no live object row, in-progress multipart part, or non-orphan composite sidecar — and the stale temp files of interrupted writes. Orphans accumulate from interrupted uploads, overwrites, crashes between the metadata and blob delete, and blob-delete failures (which are logged but do not fail the request, because the object's metadata is already gone). Where `arca fsck` only *reports* orphans (`ORPHANED_BLOB`), `arca gc` *removes* them, using the same composite-aware, fail-safe selection as the cluster anti-entropy worker.

On a **single-node** deployment there is no anti-entropy worker, so `arca gc` (typically from cron) is the reclamation path. Alternatively, enable the opt-in background worker with `[storage] blob_gc_enabled = true` (see [Configuration](configuration.md#storage)) to reclaim on a schedule without an external cron job; on very large stores cron-ing this command is preferable so the scan runs outside the serving process. On a cluster the anti-entropy worker already reclaims orphans automatically.

```bash
arca gc [--config-path <PATH>] [--reclaim] [--grace-seconds <SECONDS>] [--verbose]
```

| Option | Description |
|--------|-------------|
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |
| `--reclaim` | Actually delete the orphans and stale temp files. Without it, only report what would be reclaimed (dry run) |
| `--grace-seconds` | Protect blobs and temp files written within this many seconds (default: `86400`) |
| `--verbose` | List every orphan blob id and temp file before the summary |

The `--grace-seconds` window protects freshly-written blobs whose object row may not be committed yet (a blob file is written before its metadata row), and the temp files of uploads in flight. Keep it comfortably above your longest in-flight upload when running against a **live** server; drop it to `0` for an immediate full reclaim only when the server is **stopped**.

Temp files are the `<blob>.tmp` / `<blob>.meta.<id>.tmp` files of writes that never completed. The recovery files of an in-place rewrite (`*.rewrite.tmp`, `*.meta.pending.tmp`) are never touched: re-running the rewrite command completes them.

The summary gives the space involved: what a dry run would free, or what `--reclaim` freed (an orphan counts with its sidecar, which is deleted with it). The output also reports the unfinished multipart uploads and the bytes they hold. Their parts are not orphans: they stay until the upload is aborted (`arca fsck --repair --abort-uploads-older-than <days>`, `AbortMultipartUpload`, or an `AbortIncompleteMultipartUpload` lifecycle rule).

Fail-safe: if any enumeration (referenced ids, sidecars, on-disk blobs, temp files) fails, the command aborts and deletes nothing.

It works with both metadata backends (SQLite and PostgreSQL).

```bash
# Preview what would be reclaimed (safe; nothing is deleted)
arca gc

# List each orphan, then delete them
arca gc --reclaim --verbose

# Immediate full reclaim with the server stopped
arca gc --reclaim --grace-seconds 0
```

The `arca_blob_delete_failures_total` Prometheus metric reports how often a blob delete failed during object deletion (each one leaves an orphan for `arca gc`), so you can tell whether reclamation needs to run.

## `arca compress-existing`

Walks the blobs directory and compresses any blob that does not already carry compression metadata, honoring the live-write MIME and size filters. Crash-safe and durable per blob (see [Durability of the offline rewrite tools](#durability-of-the-offline-rewrite-tools)) and resumable: re-running skips already-compressed blobs. Encrypted blobs (SSE-S3, SSE-KMS, SSE-C) and composite (multipart) blobs are skipped with a `SKIP` message and counted in `skipped=`; the run continues. See the [Compression guide](compression.md#offline-retrofit).

```
arca compress-existing [--config-path <PATH>] [--dry-run]
                       [--bucket <NAME>] [--algorithm <NAME>]
```

| Flag | Purpose |
|---|---|
| `--dry-run` | Print what would be compressed without modifying files. |
| `--bucket` | Restrict to a single bucket. |
| `--algorithm` | Override `[compression].default_algorithm` for this run. One of `auto`, `zstd`, `lz4`, `snappy`, `gzip`, `brotli`, `xz`. |

```bash
# Preview
arca compress-existing --dry-run

# Apply compression to everything
arca compress-existing

# One bucket, force Brotli
arca compress-existing --bucket my-bucket --algorithm brotli
```

## `arca decompress-existing`

Inverse of `compress-existing` — reads each sidecar, and for compressed blobs, rewrites the plaintext to disk and removes the compression metadata. Same durability and resume properties. Composite blobs and encrypted blobs (which hold compressed data under the encryption layer) are skipped with a `SKIP` message; the run continues.

```
arca decompress-existing [--config-path <PATH>] [--dry-run] [--bucket <NAME>]
```

```bash
arca decompress-existing --dry-run
arca decompress-existing --bucket my-bucket
```

**Exit code.** `compress-existing` and `decompress-existing` abort with a non-zero exit on the first object that fails (the marker is kept); they exit `0` only after a clean run or a `--dry-run`.

## `arca encrypt-existing` / `arca decrypt-existing`

Offline counterparts of the `encrypt` / `decrypt` maintenance jobs, run with the server **stopped**. `encrypt-existing` encrypts plaintext blobs to SSE-S3; `decrypt-existing` decrypts SSE-S3 blobs back to plaintext. Each blob is rewritten in place (durable, crash-safe, see [Durability of the offline rewrite tools](#durability-of-the-offline-rewrite-tools)), and its `.meta` sidecar and database row are updated. Re-running is safe: blobs already in the target state are skipped. See the [Migration & Maintenance guide](maintenance.md#offline-cli).

```bash
arca encrypt-existing [--config-path <PATH>] [--dry-run] [--bucket <NAME>] [--prefix <PREFIX>]
arca decrypt-existing [--config-path <PATH>] [--dry-run] [--bucket <NAME>] [--prefix <PREFIX>]
```

| Option | Description |
|--------|-------------|
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |
| `--dry-run` | Only report what would change; no files or rows written |
| `--bucket` | Restrict to a single bucket |
| `--prefix` | Restrict to keys with this prefix |

Multipart objects and SSE-C objects are skipped (TD-018).

**Exit code.** `0` when every object was processed or skipped (and always for `--dry-run`). **Non-zero** when one or more objects failed: the usual summary (`Done. ... errors=N`) is still printed, the [in-progress marker](#the-in-progress-marker) is kept so `arca serve` stays blocked, and the error names the command to re-run. Scripts can rely on the exit status. Before this change these commands exited `0` even when objects had failed.

### Durability of the offline rewrite tools

`compress-existing`, `decompress-existing`, `encrypt-existing` and `decrypt-existing` replace blobs that are already safely on disk, so a power cut must never turn good data into bad. For each blob they:

1. write the new bytes to `<blob-id>.rewrite.tmp` next to the blob and `fsync` it;
2. write the new sidecar durably to `<blob-id>.meta.pending.tmp` (a journal; for encryption it holds the only copy of the new wrapped key);
3. rename the temp file over the blob and `fsync` the directory;
4. rename the journal over the sidecar and `fsync` the directory.

The original blob and sidecar are untouched until step 3, and an error before then removes the temp files. If the process or the machine dies part-way, simply **run the same command again**: before looking at each sidecar it discards a rewrite that never replaced the blob, or completes one that did (printing `recovered <path>: ...`), then carries on. After completing an `encrypt-existing`/`decrypt-existing` rewrite it also brings the database row in line. Do not delete a `*.meta.pending.tmp` file by hand; `arca fsck` lists these files as `REWRITE_RECOVERY`, which is expected after a crash and cleared by the re-run (neither `fsck --repair` nor `gc` ever removes them). A `--dry-run` never repairs or removes anything.

#### The in-progress marker

Until the re-run, an object caught between steps 3 and 4 has a blob that does not match its sidecar (for example ciphertext described as plaintext), and the server would serve it silently. To make that impossible, every real (non `--dry-run`) run first writes a marker file, durably, in the data directory: `<data_dir>/.offline-rewrite-in-progress`. It is a small JSON document with the command name, the arguments needed to re-run it (`--bucket`, `--prefix`, `--algorithm`), the start time and the pid. The marker is removed (and the directory fsynced) only when the run finished cleanly: every sidecar was processed and nothing failed. If the run aborts, reports errors, or the process or machine dies, the marker stays.

While the marker exists:

| Command | Behaviour |
|---|---|
| `arca serve` | Refuses to start (non-zero exit) before opening any store or binding a port. The error names the command and arguments to re-run and the marker path. There is no override flag. |
| The same rewrite command | Allowed, with any filters: it repairs every pending journal (the repair runs on all sidecars before the filters apply) and removes the marker when it finishes cleanly. |
| Another rewrite command | Refused, with the command to re-run first. |
| `recover`, `gc`, `migrate-db`, `migrate-topology` | Refused: they would rebuild the database from, delete based on, or copy state derived from mismatched sidecars. |
| `fsck` | Runs (it is read-only) but prints a warning, since its findings may be misleading until the rewrite is completed. `fsck --repair` is refused. |
| `--dry-run` of a rewrite command | Allowed, touches nothing. |

To resolve it, run the command named in the error, with the same `--config-path`, and wait for `Done.` without errors. **Never delete the marker by hand.** Doing so would let the server start on blobs and sidecars that may not match, and a pending `*.meta.pending.tmp` journal of `encrypt-existing` holds the only copy of the new wrapped key, which only the re-run commits.

## `arca migrate-db`

Copy **all** metadata from the configured backend into the other one (SQLite to PostgreSQL or the reverse), in place and offline. Blob files are not touched. After a successful run, switch `metadata_backend` in the config (adding `[storage.postgres]` when moving to PostgreSQL) and restart. See the [Migration & Maintenance guide](maintenance.md#metadata-migration-migrate-db).

```bash
arca migrate-db --to <BACKEND> [--config-path <PATH>] [--force]
```

| Option | Description |
|--------|-------------|
| `--to` | *(required)* Target backend: `sqlite` or `postgres`. The source is the configured backend. |
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |
| `--force` | Overwrite a non-empty target: delete every destination row first |

```bash
arca migrate-db --to postgres
arca migrate-db --to sqlite --force
```

## `arca migrate-topology`

Guided, offline transition between a standalone node and an HA cluster. The cluster is fully replicated, so nothing is redistributed: the command generates the `[cluster]` configuration stanza, runs a few small database operations and prints the next steps. Run it with the server stopped. Exactly one of `--to-cluster` and `--to-single` is required. See the [Migration & Maintenance guide](maintenance.md#topology-migration-migrate-topology) and the [High Availability guide](ha.md#guided-topology-transition-arca-migrate-topology).

```bash
arca migrate-topology --to-cluster [--output <FILE>] [--config-path <PATH>]
arca migrate-topology --to-single --force [--config-path <PATH>]
```

| Option | Description |
|--------|-------------|
| `--to-cluster` | Single to HA: make this standalone instance the **first** node of a new cluster. Prints a `[cluster]` stanza with a generated `cluster_id` and `secret` (`mode = "quorum"`, `cluster_size = 3`, `discovery = "mdns"`) and reconciles the object write counter. Refused if the instance is already clustered. |
| `--to-single` | HA to single: collapse the cluster back to **this** surviving node. Purges the cluster-only tombstones and runs `VACUUM` on SQLite. Refused if the instance is not clustered, and requires `--force`. |
| `--output` | (`--to-cluster`) Also write the generated stanza to this file |
| `--force` | (`--to-single`) Confirms that every peer is in sync and stopped. Collapsing while a peer is behind loses that peer's un-replicated writes. |
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |

Check that every peer is in sync (`first_pass_done` on every peer in [`GET /admin/cluster`](../reference/admin-api.md#cluster-topology)) before running `--to-single --force`.

## `arca cluster status`

Print this node's identity and its static cluster configuration.

```bash
arca cluster status [--config-path <PATH>]
```

It prints the `node_id`, the `cluster_id`, the consistency mode (with the write majority in `quorum` mode) and the discovery method (with the seeds for `static` discovery), all read from the configuration and the local database. When the node has no `node_id` yet, one is generated and stored, exactly as on the first server start. Without `[cluster] enabled = true` it only reports that clustering is not enabled.

It does **not** show live state: peer liveness, authentication, sync progress and quorum status come from the running server's [`GET /admin/cluster`](../reference/admin-api.md#cluster-topology) endpoint (and the console topology card).
