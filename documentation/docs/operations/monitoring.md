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

### Server errors (5xx) and their levels

A 5xx is not always a fault. A node that is syncing after a restart, a cluster without write quorum, a maintenance drain or a client over its rate limit all answer 5xx on purpose. Arca logs every 5xx response with its status, method, path (never the query string, which can carry a presigned signature), latency and request id, at the level its cause deserves:

| Level | When | Examples |
|-------|------|----------|
| `ERROR` | An unexpected failure, and any 5xx not explicitly classified | `500 InternalError` |
| `WARN` | Not a server fault, but worth a look | a cluster peer that did not acknowledge a write (503), a peer failing behind `?node=` (502), a peer calling the inter-node API of a node that runs without `[cluster]` |
| `DEBUG` | A deliberate answer whose cause is logged on its own (below) or by design | `/admin/health` 503 while syncing or draining, writes refused without quorum, the maintenance drain, `SlowDown`, `NotImplemented` |

The causes of the deliberate refusals are logged once per transition by the server, so the per-request lines can stay at `DEBUG`, off in production, without losing what happened. In a cluster:

| State | Start | While it lasts | End |
|-------|-------|----------------|-----|
| No write quorum | `WARN` (`INFO` at startup, while the peers are coming up) | `ERROR` after 1 minute, repeated every 5, with the writes refused so far and the missing peers | `INFO`, with the writes refused meanwhile |
| Syncing (first anti-entropy pass) | `INFO` | `WARN` after 5 minutes, repeated every 5 | `INFO` |
| A known peer unreachable | `WARN` | `ERROR` after 5 minutes, repeated every 5 | `INFO` |
| A seed not reached since startup | `INFO` | `ERROR` after 5 minutes, repeated every 5 | `INFO` when reached |
| Writes refused for lack of space (507) | `ERROR` | `ERROR` at most every 5 minutes, with the count | |

The maintenance drain logs `INFO` when it starts and ends, and the shutdown drain when it starts. A normal restart of a node therefore logs `INFO` lines only: an `ERROR` means a state that lasted too long or a real failure.

## Health Checks

### Endpoint

```
GET /admin/health
```

No authentication required, designed for load balancer and container orchestrator probes.

| Status | Code | Body | Meaning |
|--------|------|------|---------|
| Healthy | 200 | `{"status":"ok"}` | Server is running and accepting requests |
| Draining | 503 | `{"status":"draining"}` | Server received SIGTERM/SIGINT, draining in-flight connections before shutdown |

During [graceful shutdown](deployment.md#graceful-shutdown), the health endpoint returns 503 for `drain_timeout_seconds` (default: 30), giving load balancers time to remove the instance from rotation before connections are closed.

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
- **Expected healthy status**: `200`
- **Expected healthy body**: `{"status":"ok"}`
- **Drain status**: `503` with `{"status":"draining"}` — remove instance from rotation
- **Interval**: 10–30 seconds (should be ≤ `drain_timeout_seconds`)

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
