# CLI Reference

The `arca` binary provides subcommands for starting the server, managing credentials, and performing disaster recovery. All subcommands accept `--config-path` (default: `/etc/arca/config.toml`) to locate the configuration file.

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
arca credential add [--description <TEXT>] [--admin] [--user <USER_ID>]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--description` | | Human-readable label for the credential |
| `--admin` | | Grant admin privileges (access to [Admin API](../reference/admin-api.md) and console management) |
| `--user` | `root` | User ID to associate the credential with |

```bash
# Create a regular credential
arca credential add --description "my app"

# Create an admin credential
arca credential add --description "admin user" --admin

# Create a credential for a specific user
arca credential add --description "alice key" --user alice-uuid
```

The generated access key and secret key are printed to stdout. The secret key is shown only once — store it securely.

### `arca credential list`

List all credentials.

```bash
arca credential list [--config-path <PATH>]
```

Shows access key ID, description, role (Admin/User), and status (active/inactive) for each credential.

### `arca credential remove`

Delete a credential by its access key ID.

```bash
arca credential remove <ACCESS_KEY_ID> [--config-path <PATH>]
```

!!! warning "Lockout Prevention"
    Arca prevents deleting the last admin credential or the last active credential to avoid lockout.

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

Walks the blobs directory and compresses any blob that does not already carry compression metadata, honoring the live-write MIME and size filters. Atomic per-blob (writes a `.compressing.tmp` then renames) and resumable: re-running skips already-compressed blobs. See the [Compression guide](compression.md#offline-retrofit).

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

Inverse of `compress-existing` — reads each sidecar, and for compressed blobs, rewrites the plaintext to disk and removes the compression metadata. Same atomicity and resume properties.

```
arca decompress-existing [--config-path <PATH>] [--dry-run] [--bucket <NAME>]
```

```bash
arca decompress-existing --dry-run
arca decompress-existing --bucket my-bucket
```
