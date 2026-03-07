# Production Deployment

## Docker Compose — Development

The default setup is designed for local development:

```bash
bin/arca start -d --build
```

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
      - RUST_LOG=info
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

## Reverse Proxy

For TLS termination, place a reverse proxy in front of Arca.

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
