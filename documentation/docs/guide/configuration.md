# Configuration

## Overview

Arca follows a MinIO-like configuration model:

- **Config file** — static server settings (bind address, storage paths, logging). Set once at deploy time, rarely changes.
- **Database** — dynamic runtime data (credentials, users, grants). Managed via [CLI commands](cli.md).

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

### TLS

| Setting | Default | Description |
|---------|---------|-------------|
| `server.tls.cert_dir` | *(none)* | Base directory for certificates. Enables auto-detection or relative path resolution. |
| `server.tls.cert_file` | *(none)* | Certificate chain PEM file (relative to `cert_dir`, or absolute). |
| `server.tls.key_file` | *(none)* | Private key PEM file (relative to `cert_dir`, or absolute). |
| `server.tls.ca_file` | *(none)* | Client CA certificate for mTLS (relative to `cert_dir`, or absolute). |

When `[server.tls]` is present, the server listens on HTTPS. See the [TLS guide](tls.md) for details on the three configuration scenarios (auto-detect, relative paths, absolute paths).

### Encryption

| Setting | Default | Description |
|---------|---------|-------------|
| `encryption.enabled` | `false` | Enable server-side encryption (AES-256-GCM) for new objects. |
| `encryption.master_key` | *(none)* | Base64-encoded 256-bit master key. Generate with `arca encryption generate-key`. Mutually exclusive with `[encryption.kms]`. |
| `encryption.previous_master_key` | *(none)* | Previous master key for key rotation. Used to read objects encrypted with the old key. |

When `[encryption]` is present and `enabled = true`, all new objects are encrypted at rest. Existing unencrypted objects remain readable. See the [Encryption guide](encryption.md) for details.

### KMS (Vault/OpenBAO)

| Setting | Default | Description |
|---------|---------|-------------|
| `encryption.kms.endpoint` | *(required)* | Vault/OpenBAO endpoint URL. |
| `encryption.kms.auth_method` | *(required)* | `"token"` or `"approle"`. |
| `encryption.kms.token` | *(none)* | Vault token (required for `token` auth). |
| `encryption.kms.role_id` | *(none)* | AppRole role ID (required for `approle` auth). |
| `encryption.kms.secret_id` | *(none)* | AppRole secret ID (required for `approle` auth). |
| `encryption.kms.secret_path` | `secret/arca/master-key` | KV v2 secret path. Auto-normalized. |
| `encryption.kms.secret_field` | `key` | Field containing the base64 key. |
| `encryption.kms.tls_skip_verify` | `false` | Skip TLS verification (dev only). |
| `encryption.kms.ca_file` | *(none)* | CA certificate for Vault TLS. |

When `[encryption.kms]` is present, the master key is fetched from Vault/OpenBAO at startup. See the [Encryption guide](encryption.md#kms-integration-vaultopenbao) for details.

### Request Limits

| Setting | Default | Description |
|---------|---------|-------------|
| `server.limits.max_body_size` | `5000000000` (5 GB) | Maximum request body size in bytes. Set to `0` for unlimited. Requests exceeding this return `EntityTooLarge` (400). |
| `server.limits.max_header_count` | `100` | Maximum number of HTTP headers per request. |
| `server.limits.max_metadata_size` | `2048` (2 KB) | Maximum total size of user metadata (`x-amz-meta-*` headers) in bytes. Matches S3's 2 KB limit. |
| `server.limits.drain_timeout_seconds` | `30` | Seconds to wait during graceful shutdown before closing connections. During this window, `/admin/health` returns 503 so load balancers stop routing. |

### Rate Limiting

| Setting | Default | Description |
|---------|---------|-------------|
| `server.limits.rate_limit_per_second` | `0` (disabled) | Maximum S3 requests per second per credential. Uses GCRA algorithm. `0` disables per-credential rate limiting. |
| `server.limits.rate_limit_burst` | `0` | Burst capacity per credential. Must be ≥ `rate_limit_per_second`. |
| `server.limits.rate_limit_per_ip_per_second` | `0` (disabled) | Maximum requests per second per client IP (or `X-Forwarded-For` first entry). `0` disables per-IP rate limiting. |
| `server.limits.rate_limit_per_ip_burst` | `0` | Burst capacity per IP. Must be ≥ `rate_limit_per_ip_per_second`. |

When a rate limit is exceeded, the server returns HTTP 503 with S3 error code `SlowDown` and a `Retry-After: 1` header. Rate limiting is disabled by default, enable it for production deployments exposed to the internet.

### Metadata Cache

| Setting | Default | Description |
|---------|---------|-------------|
| `server.cache.enabled` | `true` | Enable the in-memory LRU metadata cache. Caches bucket existence and object HEAD results to reduce SQLite queries under load. |
| `server.cache.bucket_cache_size` | `1000` | Maximum number of cached bucket entries. |
| `server.cache.bucket_cache_ttl_seconds` | `60` | Time-to-live for bucket cache entries. |
| `server.cache.object_cache_size` | `10000` | Maximum number of cached object metadata entries. |
| `server.cache.object_cache_ttl_seconds` | `30` | Time-to-live for object cache entries. |

The cache is transparent to clients: write operations (create/delete bucket, put/delete object) invalidate the corresponding cache entry immediately. TTL provides an additional safety net for eventual consistency.

### Monitoring

| Setting | Default | Description |
|---------|---------|-------------|
| `monitoring.audit.enabled` | `true` | Enable audit logging (every S3/admin operation is recorded). |
| `monitoring.audit.retention_days` | *(console-managed)* | Days to retain audit entries. When set in TOML, the value is locked (read-only in console). |
| `monitoring.metrics.enabled` | `true` | Enable Prometheus metrics collection. |
| `monitoring.metrics.retention_days` | *(console-managed)* | Days to retain metrics snapshots. When set in TOML, the value is locked. |
| `monitoring.metrics.interval_seconds` | `60` | How often to snapshot gauge metrics (seconds). |

### Example

```toml
[server]
bind = "0.0.0.0"
port = 9000
# domain = "s3.example.com"  # optional, enables virtual-hosted-style requests

# [server.tls]               # optional, enables HTTPS
# cert_dir = "/etc/arca/certs"
# cert_file = "arca-server.crt"
# key_file = "arca-server.key"

# [server.limits]             # optional, all have sane defaults
# max_body_size = 5000000000  # 5 GB (S3 single PutObject limit)
# rate_limit_per_second = 100 # per-credential, 0 = disabled
# rate_limit_burst = 200
# drain_timeout_seconds = 30

# [server.cache]              # optional, enabled by default
# enabled = true
# bucket_cache_size = 1000
# object_cache_size = 10000

[storage]
data_dir = "/data"
# blob_prefix_depth = 2  # optional, default is 2

# [encryption]               # optional, enables SSE-S3
# enabled = true
# master_key = "base64..."   # generate with: arca encryption generate-key

# Alternative: fetch master key from Vault/OpenBAO
# [encryption.kms]
# endpoint = "http://vault:8200"
# auth_method = "token"
# token = "s.my-vault-token"
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

## Credentials

Credentials are stored in the SQLite database (`{data_dir}/arca.db`) and managed via the [`arca credential`](cli.md#arca-credential) CLI command:

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

Credentials have an **admin** flag that controls access to the [Admin API](../reference/admin-api.md) and management features in the [web console](console.md). Non-admin credentials can only use the S3 API. The root credential generated on first startup is always an admin credential.

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

## Environment Variables

Environment variables override the corresponding config file values. This is useful for Docker/Kubernetes deployments where you want a single base config but need to vary a few settings per instance.

| Variable | Description |
|----------|-------------|
| `ARCA_SERVER_BIND` | Override `server.bind` (e.g. `127.0.0.1`). |
| `ARCA_SERVER_PORT` | Override `server.port` (e.g. `8080`). |
| `ARCA_STORAGE_DATA_DIR` | Override `storage.data_dir` (e.g. `/mnt/data`). |
| `ARCA_LOG` | Logging level filter (default: `info`). Example: `ARCA_LOG=debug`. Supports per-module filtering (e.g. `ARCA_LOG=info,arca_auth=debug`). |
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
