# Configuration

## Overview

Arca follows a MinIO-like configuration model:

- **Config file** — static server settings (bind address, storage paths, logging). Set once at deploy time, rarely changes.
- **Database** — dynamic runtime data (credentials, users, grants). Managed via CLI commands.

The default config file is `config/default.toml` in the repository. At runtime, the binary looks for `/etc/arca/config.toml` by default (override with `--config-path`).

## Config File

### Server

| Setting | Default | Description |
|---------|---------|-------------|
| `server.bind` | `0.0.0.0` | Address to listen on |
| `server.port` | `9000` | Port to listen on |
| `server.domain` | *(none)* | Base domain for virtual-hosted-style requests (e.g. `s3.example.com`). When set, requests to `bucket.s3.example.com` are rewritten to path-style `/{bucket}/...`. Leave unset to use path-style only. |

### Storage

| Setting | Default | Description |
|---------|---------|-------------|
| `storage.data_dir` | `/data` | Root directory for SQLite database (`arca.db`) and blob storage (`blobs/` subdirectory) |
| `storage.blob_prefix_depth` | `2` | Number of 2-char prefix directory levels for blob file sharding (1–4). Higher values spread files across more directories, reducing files-per-directory at the cost of deeper paths. See [blob storage](#blob-storage) below. |

### Example

```toml
[server]
bind = "0.0.0.0"
port = 9000
# domain = "s3.example.com"  # optional, enables virtual-hosted-style requests

[storage]
data_dir = "/data"
# blob_prefix_depth = 2  # optional, default is 2
```

### Blob Storage

Object data is stored as blob files under `{data_dir}/blobs/`, sharded into a hierarchy of 2-character prefix directories derived from the blob UUID. The `blob_prefix_depth` setting controls how many levels of prefix directories are used.

With depth=2 (default), a blob with UUID `550e8400-e29b-41d4-a716-446655440000` is stored at:

```
blobs/55/0e/550e8400-e29b-41d4-a716-446655440000
```

Each blob has a `.meta` sidecar file containing JSON metadata for disaster recovery.

| Depth | Leaf directories | Files/dir (at 100M objects) |
|-------|-----------------|---------------------------|
| 1     | 256             | ~390,000                  |
| 2     | 65,536          | ~1,525                    |
| 3     | 16.7M           | ~6                        |

The default depth of 2 works well for most deployments. Increase to 3 for very large installations (tens of millions of objects) where filesystem performance degrades with many files per directory.

## Credentials (Database)

Credentials are stored in the SQLite database (`{data_dir}/arca.db`) and managed via CLI:

```bash
# Add a credential (auto-generates access key and secret key)
arca credential add --description "my app"

# Add an admin credential (required for Admin API and web console management)
arca credential add --description "admin user" --admin

# List all credentials (shows access key, description, role, and status)
arca credential list

# Remove a credential
arca credential remove <ACCESS_KEY_ID>
```

Credentials have an **admin** flag that controls access to the [Admin API](admin-api.md) and management features in the web console. Non-admin credentials can only use the S3 API. The root credential generated on first startup is always an admin credential.

!!! warning "Lockout Prevention"
    Arca prevents deleting the last admin credential or the last active credential to avoid lockout.

On first startup, if no credentials exist, Arca auto-generates a root access key pair and prints it to stdout:

```
========================================
  Root credential created automatically
========================================
  Access Key: GHUZM9QTHSJKE3N6P50O
  Secret Key: aNdrqpNvsbI9BeU/O+3AA508Xtey4Sp3EILSXRQy
========================================
  WARNING: This will only be shown once.
  Store these credentials securely.
========================================
```

## CLI Commands

### `arca serve`

Starts the Arca server.

```bash
arca serve [--config-path <PATH>] [--log-format <FORMAT>]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--config-path` | `/etc/arca/config.toml` | Path to the configuration file |
| `--log-format` | `text` | Log output format: `text` (human-readable) or `json` (structured, for log aggregation) |

The server handles SIGTERM and SIGINT for graceful shutdown — it finishes in-flight requests before stopping.

### `arca credential`

Manage S3 access credentials. See [Credentials](#credentials-database) below.

### `arca recover`

Rebuild the SQLite database from `.meta` sidecar files (disaster recovery).

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

Multipart objects (ETag contains `-`) skip checksum verification since the composite ETag is not a simple MD5 of the assembled blob. Orphaned sidecars (no blob file), malformed JSON, and checksum mismatches are skipped with warnings.

### `arca fsck`

Check database and filesystem consistency.

```bash
arca fsck [--config-path <PATH>] [--verify-checksums]
```

| Option | Description |
|--------|-------------|
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |
| `--verify-checksums` | Read every blob file and verify MD5 against stored ETag (slow) |

The fsck command performs these checks:

| Check | Description |
|-------|-------------|
| `ORPHANED_BLOB` | Blob file on disk with no corresponding database record |
| `MISSING_BLOB` | Database record references a blob file that doesn't exist |
| `SIDECAR_MISMATCH` | `.meta` sidecar data doesn't match database record (bucket, key, size, or etag) |
| `ORPHANED_SIDECAR` | `.meta` file exists without a corresponding blob file |
| `STALE_TMP` | Leftover `.tmp` file from an interrupted write |
| `CORRUPT` | Blob file MD5 doesn't match stored ETag (only with `--verify-checksums`) |

Exit code: 0 if no issues found, 1 if any issues detected.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Logging level filter (default: `info`). Example: `RUST_LOG=debug` |
| `ARCA_ROOT_ACCESS_KEY` | Override root credential access key (used when no active credentials exist). For testing/CI. |
| `ARCA_ROOT_SECRET_KEY` | Override root credential secret key (used with `ARCA_ROOT_ACCESS_KEY`). Both must be set. |

When both `ARCA_ROOT_ACCESS_KEY` and `ARCA_ROOT_SECRET_KEY` are set and no active credentials exist in the database, Arca uses these values instead of generating random credentials. This is useful for Docker Compose testing setups where you need known credentials.

## Config File Location

| Context | Path |
|---------|------|
| Binary default | `/etc/arca/config.toml` |
| Repository template | `config/default.toml` |
| Docker | Copied to `/etc/arca/config.toml` at build time; mount your own to override |

## Docker

When running with Docker Compose, you can override the config file via a volume mount:

```yaml
services:
  arca:
    volumes:
      - ./my-config.toml:/etc/arca/config.toml:ro
```
