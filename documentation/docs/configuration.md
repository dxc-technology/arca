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

# List all credentials
arca credential list

# Remove a credential
arca credential remove <ACCESS_KEY_ID>
```

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
