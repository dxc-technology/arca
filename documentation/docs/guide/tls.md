# TLS

Arca supports native TLS termination, serving HTTPS directly without a reverse proxy. TLS is configured via the `[server.tls]` section in the config file.

## Quick Start

=== "Real certificates"

    Place your certificate and key PEM files in the `certs/` directory at the repository root:

    ```
    certs/
      fullchain.pem    # certificate chain (server + intermediates)
      privkey.pem      # private key
    ```

    Start Arca with TLS:

    ```bash
    bin/arca start -d --build --tls
    ```

    Verify:

    ```bash
    curl https://your-domain:9000/admin/health
    ```

    To start the web console alongside TLS:

    ```bash
    bin/console start -d --build --tls
    ```

    With `--tls`, the console does not preset an endpoint URL — enter the HTTPS URL (e.g. `https://your-domain:9000`) at the login screen.

=== "Self-signed certificates"

    Generate a self-signed CA and server certificate:

    ```bash
    # Build the image first if needed
    bin/build

    # Generate certs into ./certs/
    mkdir -p certs
    docker compose -f docker/docker-compose.yml -f docker/docker-compose.tls.yml \
        --profile tls-init run --rm tls-init
    ```

    This creates four files in `certs/`:

    | File | Description |
    |------|-------------|
    | `arca-ca.crt` | CA certificate (distribute to clients) |
    | `arca-ca.key` | CA private key (keep secure) |
    | `arca-server.crt` | Server certificate (signed by CA) |
    | `arca-server.key` | Server private key |

    Start Arca with TLS:

    ```bash
    bin/arca start -d --tls
    ```

    Verify (passing the CA cert for trust):

    ```bash
    curl --cacert certs/arca-ca.crt https://localhost:9000/admin/health
    ```

    !!! warning
        Self-signed certificates are suitable for development and internal testing. For production, use certificates issued by a trusted Certificate Authority.

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

### File Permissions

Every Arca image runs as a non-root user, so the certificate files must be readable by that user — and by the console's user, which is a *different* one. The rule is a single line:

> **Private keys are group-readable, never other-readable.**

```bash
chgrp 65532 /etc/ssl/private/arca.key
chmod 640   /etc/ssl/private/arca.key
```

`65532` is **Arca's service GID**: the `arca` container runs as `65532:65532`, and the key must be readable by it in any case. It is the distroless `nobody` GID, not a privilege boundary — treat it as "the identity Arca runs under", not as a general-purpose secrets group.

The web console runs as the nginx package's own user, `100:101`, and in the Compose dev setup it reads the *same* key file. No owner and mode can grant read to both users without also granting it to *everyone*, so the console reaches the key through the key's **group** instead:

=== "Docker Compose"

    ```yaml
    services:
      console:
        group_add: ["65532"]
    ```

=== "Kubernetes"

    ```yaml
    spec:
      securityContext:
        runAsUser: 100
        runAsGroup: 101
        fsGroup: 65532          # or: supplementalGroups: [65532]
    ```

This is already wired up in `docker/docker-compose.tls.yml` and `deploy/kubernetes/arca-console.yaml`. Without it, the console finds the key, cannot read it, logs a warning and serves plain HTTP — the certificate is never loaded.

These are the modes `arca tls generate` writes, and the ones to reproduce for certificates obtained elsewhere:

| File | Mode | Why |
|------|------|-----|
| `arca-ca.crt`, `arca-server.crt` | `0644` | Certificates are public material. |
| `arca-server.key` (server/node key) | `0640` | Read by Arca as owner, by the console through the group. |
| `arca-ca.key` (CA key) | `0600` | Nothing reads it at runtime; it only signs. |

Generated files take the **generating process's** group. Inside the `tls-init` container that is `65532`, which is what we want; run `arca tls generate` on the host as yourself and a `chgrp 65532` is still required.

!!! danger "Certbot rewrites the key on every renewal"
    Let's Encrypt renewals replace `privkey.pem` as `root:root` mode `0600`. A one-time `chgrp` therefore reverts silently, and TLS breaks 60-90 days later — far from the change that caused it. Re-apply ownership from a deploy hook:

    ```bash
    # /etc/letsencrypt/renewal-hooks/deploy/arca-permissions.sh
    #!/bin/sh
    set -e
    KEY="/etc/letsencrypt/live/example.com/privkey.pem"
    chgrp 65532 "$(realpath "$KEY")"
    chmod 640   "$(realpath "$KEY")"
    docker kill --signal=HUP arca     # pick up the new certificate
    ```

    ```bash
    chmod +x /etc/letsencrypt/renewal-hooks/deploy/arca-permissions.sh
    # or, one-off:  certbot renew --deploy-hook /path/to/arca-permissions.sh
    ```

    Certbot's `live/` entries are symlinks into `archive/`, hence the `realpath` — and note that `archive/` and `live/` themselves default to `0700`, so the container also needs `chmod 0750` + `chgrp 65532` on both directories, or a copy of the material into a directory you own.

    Where group ownership cannot be changed (a shared key, another service's requirements), grant an ACL entry instead:

    ```bash
    setfacl -m g:65532:r /etc/ssl/private/arca.key
    ```

!!! warning "This does not reproduce on macOS"
    Docker Desktop fakes bind-mount ownership: a key that is `0600` and owned by your host user appears owned by whatever UID the container runs as, and reads fine. A permission bug therefore shows up **only on a real Linux host**. Do not conclude from a working Mac that the modes are right — run `bin/test tls-permissions`, which stages the material in a named Docker volume (where ownership is real on every host) and checks both that the console *can* read the key with the service GID and that it *cannot* without it.

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

## Console HTTPS

When using `--tls`, the web console also serves over HTTPS. The console entrypoint auto-detects certificate and key PEM files in the mounted `certs/` directory (skipping CA files) and switches nginx to TLS mode.

The console runs as uid `100`, not as Arca's `65532`, so it reads the private key through the supplementary group described in [File Permissions](#file-permissions). If the key is there but unreadable, the entrypoint logs a `WARNING` and keeps serving plain HTTP on port 80 — check `bin/console logs` when port 9443 refuses connections.

```bash
bin/console start -d --build --tls
```

The console is available on **port 9443** (HTTPS). At the login screen, enter the Arca HTTPS endpoint (e.g. `https://your-domain:9000`) and your credentials.

!!! note
    With `--tls` the console does not preset an endpoint URL, since the HTTPS domain depends on your certificate setup.

### Console TLS Indicator

When TLS is enabled, the console dashboard shows a green lock icon with "TLS" in the Transport section. This information comes from the `/admin/info` endpoint's `tls_enabled` field.
