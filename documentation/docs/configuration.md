# Configuration

!!! note
    Configuration details will be expanded as the server implementation progresses. This page documents the planned configuration surface.

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

Credentials are stored in the SQLite database and managed via CLI:

```bash
# Add a credential (planned for Phase 5)
arca credential add --access-key <key> --secret-key <secret>
```

On first startup, if no credentials exist, Arca auto-generates a root access key pair and prints it to stdout.

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
