# Monitoring & Logging

## Logging

Arca uses the `tracing` framework for structured logging. The log format and level are configured at server startup.

### Log Format

```bash
arca serve --log-format text    # Human-readable (default)
arca serve --log-format json    # Structured JSON (for log aggregation)
```

JSON format outputs one JSON object per line, suitable for ingestion by log aggregation tools (ELK, Loki, Datadog, etc.).

### Log Level

Control verbosity with the `ARCA_LOG` environment variable:

```bash
ARCA_LOG=info     # Default — requests, startup, errors
ARCA_LOG=debug    # Detailed internal operations
ARCA_LOG=warn     # Warnings and errors only
ARCA_LOG=trace    # Maximum verbosity (very noisy)
```

In Docker Compose:

```yaml
services:
  arca:
    environment:
      - ARCA_LOG=info
```

### Per-Module Filtering

`ARCA_LOG` supports per-module filtering:

```bash
# Debug logging for auth, info for everything else
ARCA_LOG=info,arca_auth=debug

# Trace storage operations
ARCA_LOG=info,arca_storage=trace
```

## Health Checks

### Endpoint

```
GET /admin/health
```

Returns `200 OK` with `{"status": "ok"}`. No authentication required — designed for load balancer and container orchestrator probes.

### Docker HEALTHCHECK

Add a health check to your Docker Compose configuration:

```yaml
services:
  arca:
    healthcheck:
      test: ["CMD", "wget", "-q", "--spider", "http://localhost:9000/admin/health"]
      interval: 10s
      timeout: 5s
      retries: 3
      start_period: 5s
```

!!! note
    The production image is built from `scratch` and does not include `curl` or `wget`. Use `wget` from the development image, or check health from a sidecar container or external probe.

### Load Balancer Probes

Point your load balancer's health check at `/admin/health`:

- **Path**: `/admin/health`
- **Expected status**: `200`
- **Expected body**: `{"status":"ok"}`
- **Interval**: 10–30 seconds

## Server Metrics

Arca exposes server information and storage statistics via the Admin API. These endpoints require SigV4 authentication with admin credentials.

### Server Info

```
GET /admin/info
```

```json
{
    "version": "0.1.0",
    "uptime_seconds": 3600
}
```

### Storage Stats

```
GET /admin/stats
```

```json
{
    "bucket_count": 5,
    "object_count": 142,
    "total_size_bytes": 1073741824
}
```

### Polling Example

Use a cron job or monitoring agent to poll stats periodically:

```bash
#!/usr/bin/env bash
# Poll Arca stats using aws-cli for SigV4 signing
aws s3api --endpoint-url http://localhost:9000 \
  list-buckets --query 'Buckets | length(@)' --output text
```

Or use the Admin API directly with any HTTP client that supports SigV4 signing. See the [Admin API reference](../reference/admin-api.md) for the full endpoint list.
