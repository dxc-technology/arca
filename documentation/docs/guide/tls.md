# TLS

Arca supports native TLS termination, serving HTTPS directly without a reverse proxy. TLS is configured via the `[server.tls]` section in the config file.

## Quick Start

Generate a self-signed CA and server certificate:

```bash
docker compose exec arca arca tls generate --output-dir /etc/arca/certs --sans "arca,localhost,127.0.0.1,::1"
```

This creates four files:

| File | Description |
|------|-------------|
| `arca-ca.crt` | CA certificate (distribute to clients) |
| `arca-ca.key` | CA private key (keep secure) |
| `arca-server.crt` | Server certificate (signed by CA) |
| `arca-server.key` | Server private key |

Add the TLS section to your config:

```toml
[server.tls]
cert_dir = "/etc/arca/certs"
cert_file = "arca-server.crt"
key_file = "arca-server.key"
```

Restart Arca. The server now listens on HTTPS:

```bash
curl --cacert arca-ca.crt https://localhost:9000/admin/health
```

## Configuration

Three configuration scenarios are supported:

### 1. Directory Only (Auto-Detect)

```toml
[server.tls]
cert_dir = "/etc/arca/certs"
```

Arca scans the directory for PEM files (`.pem`, `.crt`, `.key`, `.cert`) and classifies them by reading PEM headers. Requires exactly one key file and at least one certificate file. Fails with a clear error if ambiguous.

### 2. Directory + Relative Filenames

```toml
[server.tls]
cert_dir = "/etc/arca/certs"
cert_file = "server.crt"
key_file = "server.key"
```

Filenames are resolved relative to `cert_dir`. This is the recommended approach for most deployments.

### 3. Absolute Paths

```toml
[server.tls]
cert_file = "/etc/ssl/certs/arca.pem"
key_file = "/etc/ssl/private/arca.key"
```

Use this when the certificate and key are in different directories.

### All Options

| Setting | Default | Description |
|---------|---------|-------------|
| `cert_dir` | *(none)* | Base directory for certificates. Enables auto-detection or relative path resolution. |
| `cert_file` | *(none)* | Certificate chain PEM file (relative to `cert_dir`, or absolute). |
| `key_file` | *(none)* | Private key PEM file (relative to `cert_dir`, or absolute). |
| `ca_file` | *(none)* | Client CA certificate for mTLS (relative to `cert_dir`, or absolute). Always explicit — never auto-detected. |

**Validation rules**: You must provide at least `cert_dir` alone (auto-detect), or both `cert_file` and `key_file`. Providing only one of `cert_file`/`key_file` without the other is an error.

## Production Setup

For production, use certificates issued by a trusted CA (e.g. Let's Encrypt, your organization's internal CA):

```toml
[server.tls]
cert_file = "/etc/ssl/certs/arca.pem"
key_file = "/etc/ssl/private/arca.key"
```

!!! tip
    The `cert_file` should contain the full certificate chain (server cert + intermediates). Most CAs provide this as a "fullchain" file.

## Certificate Rotation

Arca supports zero-downtime certificate rotation via SIGHUP. When the server receives a SIGHUP signal, it re-reads the certificate and key files from disk and applies them to new connections. Existing connections are not affected.

```bash
# Replace certificate files on disk, then:
kill -HUP $(pgrep arca)

# When running in Docker (no shell in the container):
docker kill --signal=HUP arca
# or with Docker Compose:
docker compose kill --signal=HUP arca
```

```
INFO TLS certificates reloaded
```

If the new files are invalid, the reload fails gracefully and the server continues using the previous certificates:

```
ERROR TLS reload failed (keeping old config): ...
```

## mTLS (Mutual TLS)

For client certificate verification, add a `ca_file` pointing to the CA that signed your client certificates:

```toml
[server.tls]
cert_dir = "/etc/arca/certs"
cert_file = "server.crt"
key_file = "server.key"
ca_file = "client-ca.crt"
```

When `ca_file` is set, the server requires clients to present a valid certificate signed by the specified CA.

## `arca tls generate`

Generate a self-signed CA and server certificate for development and testing. See the [CLI Reference](cli.md#arca-tls-generate) for full usage.

```bash
arca tls generate \
    --output-dir /etc/arca/certs \
    --sans "myhost.example.com,localhost,127.0.0.1,::1" \
    --days 365
```

!!! warning
    Self-signed certificates are suitable for development and internal testing. For production, use certificates issued by a trusted Certificate Authority.

## Console TLS Indicator

When TLS is enabled, the [web console](console.md) dashboard shows a green lock icon with "TLS" in the Transport section. This information comes from the `/admin/info` endpoint's `tls_enabled` field.
