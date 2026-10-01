# CLI Reference

The `arca` binary provides subcommands for starting the server, managing credentials and users, generating TLS material, maintaining the data in place, inspecting a cluster, and performing disaster recovery. Every subcommand that reads the configuration accepts `--config-path` (default: `/etc/arca/config.toml`) to locate the configuration file; `arca tls` and `arca encryption generate-key` do not read it.

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

Check database and filesystem consistency. See [Disaster Recovery](../operations/recovery.md) for a detailed guide.

```bash
arca fsck [--config-path <PATH>] [--verify-checksums]
```

| Option | Description |
|--------|-------------|
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |
| `--verify-checksums` | Read every blob file and verify MD5 against stored ETag (slow) |

### Check Types

| Check | Description |
|-------|-------------|
| `ORPHANED_BLOB` | Blob file on disk with no corresponding database record |
| `MISSING_BLOB` | Database record references a blob file that doesn't exist |
| `SIDECAR_MISMATCH` | `.meta` sidecar data doesn't match database record (bucket, key, size, or etag) |
| `ORPHANED_SIDECAR` | `.meta` file exists without a corresponding blob file |
| `STALE_TMP` | Leftover `.tmp` file from an interrupted write |
| `CORRUPT` | Blob file MD5 doesn't match stored ETag (only with `--verify-checksums`) |

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | No issues found |
| 1 | One or more issues detected |

```bash
# Quick consistency check
arca fsck

# Full check including blob checksums (slow for large datasets)
arca fsck --verify-checksums
```

## `arca gc`

Reclaim orphaned blob files — on-disk blobs referenced by no live object row, in-progress multipart part, or non-orphan composite sidecar. Orphans accumulate from interrupted uploads, overwrites, crashes between the metadata and blob delete, and blob-delete failures (which are logged but do not fail the request, because the object's metadata is already gone). Where `arca fsck` only *reports* orphans (`ORPHANED_BLOB`), `arca gc` *removes* them, using the same composite-aware, fail-safe selection as the cluster anti-entropy worker.

On a **single-node** deployment there is no anti-entropy worker, so `arca gc` (typically from cron) is the reclamation path. Alternatively, enable the opt-in background worker with `[storage] blob_gc_enabled = true` (see [Configuration](configuration.md#storage)) to reclaim on a schedule without an external cron job; on very large stores cron-ing this command is preferable so the scan runs outside the serving process. On a cluster the anti-entropy worker already reclaims orphans automatically.

```bash
arca gc [--config-path <PATH>] [--reclaim] [--grace-seconds <SECONDS>] [--verbose]
```

| Option | Description |
|--------|-------------|
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |
| `--reclaim` | Actually delete the orphans. Without it, only report what would be reclaimed (dry run) |
| `--grace-seconds` | Protect blobs written within this many seconds (default: `86400`) |
| `--verbose` | List every orphan blob id before the summary |

The `--grace-seconds` window protects freshly-written blobs whose object row may not be committed yet (a blob file is written before its metadata row). Keep it comfortably above your longest in-flight upload when running against a **live** server; drop it to `0` for an immediate full reclaim only when the server is **stopped**.

Fail-safe: if any enumeration (referenced ids, sidecars, on-disk blobs) fails, the command aborts and deletes nothing.

Currently supports the `sqlite` metadata backend only (like `arca recover` / `arca fsck`).

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

Walks the blobs directory and compresses any blob that does not already carry compression metadata, honoring the live-write MIME and size filters. Atomic per-blob (writes a `.compressing.tmp` then renames) and resumable: re-running skips already-compressed blobs. Encrypted blobs (SSE-S3, SSE-KMS, SSE-C) and composite (multipart) blobs are skipped with a `SKIP` message and counted in `skipped=`; the run continues. See the [Compression guide](compression.md#offline-retrofit).

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

Inverse of `compress-existing` — reads each sidecar, and for compressed blobs, rewrites the plaintext to disk and removes the compression metadata. Same atomicity and resume properties. Composite blobs and encrypted blobs (which hold compressed data under the encryption layer) are skipped with a `SKIP` message; the run continues.

```
arca decompress-existing [--config-path <PATH>] [--dry-run] [--bucket <NAME>]
```

```bash
arca decompress-existing --dry-run
arca decompress-existing --bucket my-bucket
```

## `arca encrypt-existing` / `arca decrypt-existing`

Offline counterparts of the `encrypt` / `decrypt` maintenance jobs, run with the server **stopped**. `encrypt-existing` encrypts plaintext blobs to SSE-S3; `decrypt-existing` decrypts SSE-S3 blobs back to plaintext. Each blob is rewritten in place (atomic temp file + rename), and its `.meta` sidecar and database row are updated. Re-running is safe: blobs already in the target state are skipped. See the [Migration & Maintenance guide](maintenance.md#offline-cli).

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
