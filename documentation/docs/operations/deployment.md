# Production Deployment

## Docker Compose — Development

The default setup is designed for local development:

```bash
bin/arca start -d --build
```

!!! tip
    Drop the `-d` flag to run in the foreground and see logs in real time. Press ++ctrl+c++ to stop the server.

This starts Arca with:

- Auto-generated or environment-provided credentials
- Data stored in a Docker volume (`arca-data`)
- Port 9000 exposed to localhost

## Docker Compose — Production

For production, customize the configuration and use persistent storage.

### 1. Create a Config File

```toml
[server]
bind = "0.0.0.0"
port = 9000
# domain = "s3.example.com"  # uncomment for virtual-hosted-style

[storage]
data_dir = "/data"
blob_prefix_depth = 2
```

### 2. Docker Compose File

```yaml
services:
  arca:
    image: arca
    build:
      context: .
      dockerfile: docker/Dockerfile
      target: production
    ports:
      - "9000:9000"
    volumes:
      - /srv/arca/data:/data
      - ./config/production.toml:/etc/arca/config.toml:ro
    environment:
      - ARCA_LOG=info
    restart: unless-stopped
```

!!! warning
    Do **not** set `ARCA_ROOT_ACCESS_KEY` / `ARCA_ROOT_SECRET_KEY` in production. Let Arca generate credentials on first startup, then manage them via `arca credential` CLI commands.

### 3. First Startup

```bash
docker compose up -d
docker compose logs arca | grep "Access Key"
```

Store the generated credentials securely. They are shown only once.

### 4. Add Application Credentials

Create non-admin credentials for application access:

```bash
docker compose exec arca arca credential add --description "web app"
docker compose exec arca arca credential add --description "backup service"
```

Reserve admin credentials for the web console and administrative tasks.

## Native TLS

Arca supports native HTTPS without a reverse proxy. See the [TLS guide](../guide/tls.md) for full details.

### Quick Setup

Place your certificate and key PEM files in the `certs/` directory, then:

```bash
bin/arca start -d --build --tls
```

The `--tls` flag adds the TLS compose overlay which bind-mounts `certs/` into the container and enables auto-detection of PEM files. This works with any certificate provider (Let's Encrypt, internal CA, etc.) — just drop the PEM files in the directory.

Certificate rotation is supported via `docker compose kill --signal=HUP arca` without downtime.

## Native Binary (systemd)

Arca can run directly on a Linux host without Docker. The binary is statically linked (musl), so it has no runtime dependencies.

### 1. Install the Binary

Build for your target architecture and copy to the host:

```bash
# Build from source (requires Docker on the build machine)
bin/build --binary                  # native arch
bin/build --binary --arch amd64     # cross-compile for x86_64

# Copy to the server
scp build/arca-* server:/usr/local/bin/arca
chmod +x /usr/local/bin/arca
```

### 2. Create User and Directories

```bash
useradd --system --home-dir /var/lib/arca --shell /usr/sbin/nologin arca
mkdir -p /var/lib/arca /etc/arca
chown arca:arca /var/lib/arca
```

### 3. Configuration

Copy the sample config and adjust it:

```bash
cp deploy/config/arca.toml /etc/arca/config.toml
```

At minimum, review `[server]` bind/port and `[storage] data_dir` (set to `/var/lib/arca` for systemd deployments). See the [configuration reference](../guide/configuration.md) for all options.

### 4. Install the systemd Unit

```bash
cp deploy/systemd/arca.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now arca
```

Check startup and first-run credentials:

```bash
journalctl -u arca -f
```

!!! tip
    The systemd unit includes security hardening (sandboxed filesystem, no new privileges, private /tmp). It also sets `TimeoutStopSec=45` to allow in-flight requests to complete during graceful shutdown.

### 5. TLS with systemd

To enable native TLS:

```bash
mkdir -p /etc/arca/certs
# Place your cert and key files in /etc/arca/certs/
```

Uncomment the `[server.tls]` section in `/etc/arca/config.toml` and set the paths. To rotate certificates without downtime:

```bash
# Replace cert files, then signal Arca to reload
systemctl kill --signal=HUP arca
```

## Kubernetes

### Arca Server

Arca runs well in Kubernetes as a `Deployment` (stateless, with a PersistentVolumeClaim for data) or as a `StatefulSet`. Key considerations:

- Mount a PVC at the `data_dir` path (default `/data`)
- Use a `ConfigMap` or mounted `Secret` for `/etc/arca/config.toml`
- The `/admin/health` endpoint returns `200` when healthy and `503` when draining, use it for both liveness and readiness probes
- Set `drain_timeout_seconds` to match or exceed your pod termination grace period
- For PostgreSQL metadata backend, use an external database (RDS, CloudNative-PG, etc.)

```yaml
livenessProbe:
  httpGet:
    path: /admin/health
    port: 9000
  initialDelaySeconds: 5
  periodSeconds: 30
readinessProbe:
  httpGet:
    path: /admin/health
    port: 9000
  initialDelaySeconds: 2
  periodSeconds: 10
```

### Arca Console

A ready-to-use Kubernetes manifest is provided at `deploy/kubernetes/arca-console.yaml` with Deployment, Service, and Ingress resources.

```bash
# Replace the namespace placeholder and apply
sed 's/NAMESPACE/arca/g' deploy/kubernetes/arca-console.yaml | kubectl apply -f -

# Or apply to a specific namespace directly
kubectl apply -f deploy/kubernetes/arca-console.yaml -n arca
```

Customize before applying:

| Placeholder / Setting | Description |
|----------------------|-------------|
| `NAMESPACE` | Target Kubernetes namespace |
| `image: arca-console:latest` | Your container registry and tag |
| `ARCA_ENDPOINT` | URL of the Arca server (e.g. `http://arca:9000` for in-cluster) |
| `host: console.example.com` | Ingress hostname for the console |
| `ingressClassName` | Your ingress controller class (uncomment) |
| `tls` | TLS termination at the ingress (uncomment and configure) |

!!! note
    The console is a lightweight nginx SPA, resource requests are minimal (50m CPU, 32Mi RAM). Scale replicas as needed for availability.

## Reverse Proxy

If you prefer external TLS termination, or need additional proxy-level features, place a reverse proxy in front of Arca.

!!! note
    With [native TLS](#native-tls), [built-in rate limiting](#rate-limiting), and [metadata caching](#metadata-cache) available natively, a reverse proxy is optional. Use it when you need features like geographic load balancing or WAF integration.

### nginx Example

```nginx
server {
    listen 443 ssl;
    server_name s3.example.com;

    ssl_certificate     /etc/ssl/certs/s3.example.com.pem;
    ssl_certificate_key /etc/ssl/private/s3.example.com.key;

    client_max_body_size 0;  # unlimited — Arca handles streaming

    location / {
        proxy_pass http://arca:9000;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # Required for streaming uploads
        proxy_request_buffering off;
        proxy_http_version 1.1;
    }
}
```

!!! tip
    Set `client_max_body_size 0` and `proxy_request_buffering off` to allow Arca's streaming I/O to work correctly with large objects.

## Rate Limiting

Arca has built-in rate limiting to protect against abuse and resource exhaustion. Both per-IP and per-credential limiters are available, using the GCRA (Generic Cell Rate Algorithm) for smooth rate enforcement.

```toml
[server.limits]
rate_limit_per_ip_per_second = 500   # per client IP
rate_limit_per_ip_burst = 1000
rate_limit_per_second = 100          # per S3 credential
rate_limit_burst = 200
```

When a limit is exceeded, the server returns HTTP 503 with S3 error code `SlowDown` and a `Retry-After: 1` header. AWS SDKs and most S3 clients handle this automatically with exponential backoff.

Rate limiting is **disabled by default** (rate = 0). Enable it for any deployment exposed to the internet or shared by multiple tenants. See the [configuration reference](../guide/configuration.md#rate-limiting) for all settings.

!!! tip
    Behind a reverse proxy, per-IP limiting uses the `X-Forwarded-For` header (first entry) to identify clients. Make sure your proxy sets this header.

## Graceful Shutdown

On SIGTERM or SIGINT, Arca enters **drain mode**:

1. The `/admin/health` endpoint immediately starts returning `503 {"status":"draining"}`
2. Load balancers polling health stop routing new traffic to the instance
3. In-flight requests are allowed to complete during the drain window
4. After `drain_timeout_seconds` (default: 30), the server shuts down

```toml
[server.limits]
drain_timeout_seconds = 30
```

This enables zero-downtime rolling upgrades in orchestrated environments (Kubernetes, Docker Swarm, etc.). Set the drain timeout to match or exceed your load balancer's health check interval.

## Metadata Cache

Arca caches frequently-accessed metadata (bucket existence, object HEAD results) in an in-memory LRU cache to reduce SQLite query pressure under load.

```toml
[server.cache]
enabled = true
bucket_cache_size = 1000
bucket_cache_ttl_seconds = 60
object_cache_size = 10000
object_cache_ttl_seconds = 30
```

The cache is **enabled by default** and transparent to clients. Write operations invalidate the corresponding cache entry immediately. TTL provides a safety net for eventual expiry.

For single-node deployments, the cache is purely a performance optimization. Disable it (`enabled = false`) if you need to minimize memory usage. See the [configuration reference](../guide/configuration.md#metadata-cache) for all settings.

## Storage Sizing

### Blob Prefix Depth

The `blob_prefix_depth` setting controls how blob files are distributed across directories:

| Depth | Leaf directories | Files/dir (at 100M objects) | Recommended for |
|-------|-----------------|---------------------------|-----------------|
| 1     | 256             | ~390,000                  | Tiny deployments (<100K objects) |
| 2     | 65,536          | ~1,525                    | Most deployments (default) |
| 3     | 16.7M           | ~6                        | Very large (>10M objects) |

The default depth of 2 works well for most deployments. Only increase it if you expect tens of millions of objects and observe filesystem performance issues.

### Disk Space

Arca stores each object as a blob file plus a small `.meta` sidecar (typically <1 KB). Plan for:

- Object data: sum of all stored object sizes
- Sidecar overhead: ~500 bytes per object
- SQLite database: ~1 KB per object record
- Temporary files during multipart uploads

## Security Hardening

### Request Validation

Arca validates incoming requests to reject malformed or oversized payloads early in the middleware stack:

- **Body size limit** — requests exceeding `max_body_size` (default 5 GB) are rejected with `EntityTooLarge` before data is written to disk
- **Header count limit** — requests with more than `max_header_count` (default 100) headers are rejected
- **Metadata size limit** — total `x-amz-meta-*` header size is capped at `max_metadata_size` (default 2 KB), matching S3's limit
- **URI validation** — null bytes in request URIs are rejected

All limits are configurable via `[server.limits]`. See the [configuration reference](../guide/configuration.md#request-limits).

### Credential Management

- **Disable environment overrides** — do not set `ARCA_ROOT_ACCESS_KEY` / `ARCA_ROOT_SECRET_KEY` in production
- **Use non-admin credentials for applications** — admin credentials should only be used for the web console and administrative tasks
- **Rotate credentials periodically** — create new credentials and remove old ones via `arca credential`

### Network Isolation

- Run Arca on an internal network, exposed only through a reverse proxy
- Use firewall rules to restrict access to port 9000
- Place the [web console](../guide/console.md) behind authentication if exposing it externally

### Filesystem Permissions

- The data directory should be owned by the Arca process user
- Use read-only mounts for the config file (`:ro` in Docker)
- Back up the data directory regularly (see [Disaster Recovery](recovery.md))
