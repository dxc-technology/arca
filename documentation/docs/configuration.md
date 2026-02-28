# Configuration

!!! note
    Configuration details will be expanded as the server implementation progresses. This page documents the planned configuration surface.

## Overview

Arca is configured via a TOML file with environment variable overrides. The default configuration file is `config/default.toml`.

## Server

| Setting | Default | Description |
|---------|---------|-------------|
| `server.bind` | `0.0.0.0:9000` | Address and port to listen on |

## Storage

| Setting | Default | Description |
|---------|---------|-------------|
| `storage.data_dir` | `./data` | Directory for blob storage (UUID files + `.meta` sidecars) |
| `storage.db_path` | `./data/arca.db` | Path to the SQLite metadata database |

## Auth

| Setting | Default | Description |
|---------|---------|-------------|
| `auth.access_key_id` | — | AWS access key ID for request authentication |
| `auth.secret_access_key` | — | AWS secret access key for request authentication |

## Environment Variables

Environment variables override TOML settings. The naming convention is `ARCA_` prefix with double underscore as section separator:

```bash
ARCA_SERVER__BIND=0.0.0.0:9000
ARCA_STORAGE__DATA_DIR=/var/lib/arca/data
ARCA_AUTH__ACCESS_KEY_ID=myaccesskey
ARCA_AUTH__SECRET_ACCESS_KEY=mysecretkey
```

## Docker

When running with Docker Compose, configuration can be passed via environment variables in the compose file or via volume-mounted config files.

```yaml
services:
  arca:
    environment:
      ARCA_AUTH__ACCESS_KEY_ID: minioadmin
      ARCA_AUTH__SECRET_ACCESS_KEY: minioadmin
```
