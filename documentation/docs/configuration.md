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
| `storage.data_dir` | `/data` | Directory for blob storage (UUID files + `.meta` sidecars) and SQLite database |

### Example

```toml
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
```

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
