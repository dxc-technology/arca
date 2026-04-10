# Troubleshooting

Common issues and their solutions when deploying and operating Arca.

## Web Console

### "crypto.subtle is undefined" or SigV4 signing fails

**Symptom**: The console login screen accepts credentials but nothing happens. The browser console shows:

```
undefined is not an object (evaluating 'crypto.subtle.digest')
```

**Cause**: The Web Crypto API (`crypto.subtle`) is only available in [secure contexts](https://developer.mozilla.org/en-US/docs/Web/Security/Secure_Contexts) — that is, pages served over HTTPS or from `localhost`. If the console is served over plain HTTP on a non-localhost address, the browser blocks the API entirely, and SigV4 request signing fails silently.

**Solution**: Serve the console over HTTPS. Options:

- **Kubernetes Ingress with TLS** — enable TLS termination on the Ingress resource (recommended for K8s):

    ```yaml
    spec:
      tls:
        - hosts:
            - console.example.com
          secretName: arca-console-tls
    ```

- **Console with native TLS** — mount certificate files into the console container at `/etc/nginx/certs/`. The entrypoint auto-detects them and switches to the HTTPS nginx config.

- **Reverse proxy** — place nginx, Traefik, or another proxy with TLS in front of the console.

!!! note
    No errors appear in Arca server logs or console Pod logs because the failure happens entirely in the browser, before any HTTP request is sent.

### Console cannot reach the Arca server (CORS errors)

**Symptom**: The console login succeeds (no `crypto.subtle` error) but all API calls fail. The browser console shows errors like:

```
Access to fetch at 'http://arca:9000' from origin 'https://console.example.com'
has been blocked by CORS policy
```

**Cause**: The `ARCA_ENDPOINT` environment variable in the console deployment points to an internal URL (e.g., `http://arca:9000`) that is not reachable from the user's browser. The console is a client-side SPA — all S3 and Admin API requests are made directly from the browser, not from the console's nginx container.

**Solution**: Set `ARCA_ENDPOINT` to a URL that the browser can reach:

```yaml
env:
  - name: ARCA_ENDPOINT
    value: "https://s3.example.com"   # Must be reachable from the user's browser
```

!!! warning
    `http://arca:9000` (a Kubernetes internal service name) works for server-to-server communication but is not resolvable from a user's browser. Use the external/ingress URL instead.

### Console shows "Network Error" or requests time out

**Symptom**: The console loads and the login form appears, but after entering credentials, requests hang or fail with a generic network error.

**Cause**: Common reasons:

- The Arca server is not running or not reachable at the configured endpoint.
- A firewall or network policy blocks the browser's connection to the Arca server port.
- The Arca server is behind a reverse proxy that buffers requests and breaks streaming.

**Solution**:

1. Verify the Arca server is running: `curl http://<arca-host>:9000/admin/health`
2. Verify the endpoint is reachable from the browser's network (not just from the server's network).
3. If behind a reverse proxy, ensure `proxy_request_buffering off` and `client_max_body_size 0` are set (see the [reverse proxy section](deployment.md#reverse-proxy)).

## TLS

### Certificate auto-detection fails

**Symptom**: Arca or the console fails to start with a TLS error about not finding certificates, even though PEM files are in the cert directory.

**Cause**: The auto-detection logic expects exactly one certificate file and one key file. It fails when:

- Multiple `.pem` files exist (e.g., a CA cert alongside the server cert)
- Files have unexpected extensions (e.g., `.cer` instead of `.crt` or `.pem`)

**Solution**: Use explicit filenames in the config instead of relying on auto-detection:

```toml
[server.tls]
cert_dir = "/etc/arca/certs"
cert_file = "server.crt"
key_file = "server.key"
```

For the console, ensure only the server cert and key are in `/etc/nginx/certs/`, not CA certificates (the entrypoint skips files with `ca` in the name, but other extra files may cause ambiguity).

### Self-signed certificate rejected by S3 clients

**Symptom**: `aws-cli` or `mc` commands fail with SSL verification errors:

```
SSL: CERTIFICATE_VERIFY_FAILED
```

**Cause**: S3 clients verify TLS certificates by default. Self-signed certificates are not trusted.

**Solution**:

=== "aws-cli"

    ```bash
    # Option 1: skip verification (development only)
    aws s3 ls --endpoint-url https://arca:9000 --no-verify-ssl

    # Option 2: trust your CA (recommended)
    export AWS_CA_BUNDLE=/path/to/ca.crt
    aws s3 ls --endpoint-url https://arca:9000
    ```

=== "MinIO Client (mc)"

    ```bash
    # Trust your CA
    mc alias set arca https://arca:9000 ACCESS_KEY SECRET_KEY --insecure
    ```

=== "boto3 (Python)"

    ```python
    import boto3
    s3 = boto3.client('s3',
        endpoint_url='https://arca:9000',
        verify='/path/to/ca.crt',  # or verify=False for dev
    )
    ```

### Certificate rotation has no effect

**Symptom**: After replacing certificate files, Arca still serves the old certificate.

**Cause**: Arca caches the TLS certificate in memory. It only reloads on SIGHUP.

**Solution**: Signal Arca to reload:

```bash
# systemd
systemctl kill --signal=HUP arca

# Docker
docker compose kill --signal=HUP arca
```

Verify by checking the certificate:

```bash
echo | openssl s_client -connect arca:9000 2>/dev/null | openssl x509 -noout -dates
```

## Networking

### Health checks fail behind a load balancer

**Symptom**: The load balancer marks Arca as unhealthy, even though the server is running fine.

**Cause**: Common reasons:

- The health check is using the wrong path (e.g., `/health` instead of `/admin/health`).
- The load balancer is checking over HTTPS but Arca is configured for plain HTTP (or vice versa).
- The health check port is misconfigured.

**Solution**: The health endpoint is `GET /admin/health` on the same port as the S3 API (default 9000). It returns:

- `200 OK` with `{"status":"ok"}` when healthy
- `503 Service Unavailable` with `{"status":"draining"}` during graceful shutdown

```bash
# Verify manually
curl -s http://arca:9000/admin/health | jq .
```

### Clients get "SignatureDoesNotMatch" after adding a reverse proxy

**Symptom**: S3 requests fail with `SignatureDoesNotMatch` after putting Arca behind a reverse proxy.

**Cause**: SigV4 signatures include the `Host` header. If the proxy changes the `Host` header (e.g., from `s3.example.com` to `arca:9000`), the signature computed by the client no longer matches.

**Solution**: Ensure the proxy forwards the original `Host` header:

```nginx
proxy_set_header Host $host;
```

### Rate limiting returns 503 unexpectedly

**Symptom**: Clients receive `503 SlowDown` errors under moderate load.

**Cause**: Rate limits may be set too low for the workload, or behind a proxy all traffic appears to come from a single IP, triggering per-IP limits.

**Solution**:

1. Ensure the proxy sets `X-Forwarded-For` so Arca sees real client IPs.
2. Adjust limits in `[server.limits]`:

    ```toml
    [server.limits]
    rate_limit_per_ip_per_second = 500
    rate_limit_per_ip_burst = 1000
    rate_limit_per_second = 100
    rate_limit_burst = 200
    ```

3. Set to `0` to disable rate limiting entirely (default).

## Storage

### "Permission denied" writing to data directory

**Symptom**: Arca fails to start with a permission error on the data directory.

**Cause**:

- **systemd**: The data directory is not owned by the `arca` user.
- **Docker**: The container runs as a non-root user but the mounted volume has root ownership.
- **Kubernetes**: The PVC mount has restrictive permissions.

**Solution**:

```bash
# systemd
chown -R arca:arca /var/lib/arca

# Docker — ensure the volume is writable
docker compose exec arca ls -la /data

# Kubernetes — add a securityContext to the pod spec
# securityContext:
#   fsGroup: 1000
```

### "Too many open files" under load

**Symptom**: Arca logs errors about file descriptor limits under heavy workload.

**Cause**: The default OS limit on open files (typically 1024) is too low. Each concurrent request can hold multiple file descriptors (blob file, sidecar, SQLite connection).

**Solution**:

```bash
# systemd — already set in the provided unit file
# Verify: systemctl show arca | grep LimitNOFILE
# Should show: LimitNOFILE=65536

# Docker — add to docker-compose.yml
services:
  arca:
    ulimits:
      nofile:
        soft: 65536
        hard: 65536
```

### SQLite "database is locked" errors

**Symptom**: Occasional `database is locked` errors under concurrent writes.

**Cause**: SQLite WAL mode handles concurrency well, but extremely high write throughput can still cause contention.

**Solution**:

- For moderate loads, these errors are transient and self-resolve (Arca retries internally).
- For high-write workloads, switch to the PostgreSQL metadata backend:

    ```toml
    [storage]
    metadata_backend = "postgres"

    [storage.postgres]
    connection_string = "postgresql://arca:secret@db:5432/arca"
    ```

## Encryption

### Cannot read objects after changing master key

**Symptom**: `GetObject` returns errors for objects that were encrypted with a previous master key.

**Cause**: When you rotate the master key, existing objects are still wrapped with the old key. Without the previous key configured, Arca cannot unwrap the per-object DEKs.

**Solution**: Set both the new and old keys in the config:

```toml
[encryption]
enabled = true
master_key = "new-base64-key"
previous_master_key = "old-base64-key"      # keeps old objects readable
```

Then run `arca migrate-encryption` to re-wrap all DEKs with the new key. After migration completes, `previous_master_key` can be removed.

### Mixed encrypted and unencrypted objects

**Symptom**: Some objects in a bucket are encrypted and others are not, causing confusion.

**Cause**: This is expected behavior. Arca supports mixed-mode coexistence: objects created before encryption was enabled remain unencrypted, while new objects are encrypted. Encryption status is tracked per-object.

**Solution**: This is by design. To encrypt existing objects, re-upload them (e.g., copy-in-place with `aws s3 cp`). The console shows encryption status per object, so you can identify which objects are unencrypted.

## systemd

### Arca exits immediately after start

**Symptom**: `systemctl start arca` succeeds but the service immediately stops. `journalctl -u arca` shows no useful output.

**Cause**: Common reasons:

- The config file is missing or has syntax errors.
- The binary is not executable or is for the wrong architecture.
- The data directory does not exist.

**Solution**:

```bash
# 1. Check config syntax by running manually
/usr/local/bin/arca serve --config-path /etc/arca/config.toml

# 2. Verify binary architecture
file /usr/local/bin/arca
# Should show: ELF 64-bit LSB ... x86-64 (or aarch64)

# 3. Verify data directory exists
ls -la /var/lib/arca
```

### Credentials lost after restart

**Symptom**: After restarting Arca, the root credentials shown in logs are different.

**Cause**: Root credentials are generated on first startup and stored in the SQLite database. If the data directory was not persisted (e.g., Docker volume was removed, or `data_dir` pointed to a temporary location), the database was lost.

**Solution**: Ensure `data_dir` points to persistent storage and verify the SQLite database file exists:

```bash
ls -la /var/lib/arca/metadata.db
```

## Kubernetes-Specific

### Pod stuck in CrashLoopBackOff

**Symptom**: The Arca pod repeatedly crashes and restarts.

**Cause**: Check the logs for the specific error:

```bash
kubectl logs -n arca deploy/arca --previous
```

Common reasons:

- ConfigMap or Secret not mounted correctly (missing config file).
- PVC not bound (pending storage provisioning).
- Liveness probe too aggressive (Arca still initializing).

**Solution**: Increase `initialDelaySeconds` on the liveness probe if the database migration takes time on first start:

```yaml
livenessProbe:
  httpGet:
    path: /admin/health
    port: 9000
  initialDelaySeconds: 15   # increase if first-start migration is slow
  periodSeconds: 30
```

### Console Pod healthy but page is blank

**Symptom**: The console Pod is running, health checks pass, but the browser shows a blank page.

**Cause**: The `ARCA_ENDPOINT` environment variable is not set, so the console cannot inject the server URL. The login form renders but the endpoint field is empty and requests fail silently.

**Solution**: Verify the environment variable is set in the deployment:

```bash
kubectl exec -n arca deploy/arca-console -- printenv ARCA_ENDPOINT
```

If empty, update the deployment to set it to the browser-accessible Arca URL.

## Debugging Tips

### Enable debug logging

Set the `ARCA_LOG` environment variable for detailed output:

```bash
# systemd
systemctl edit arca
# Add: Environment=ARCA_LOG=debug

# Docker
ARCA_LOG=debug bin/arca start -d

# Kubernetes — add to the container env
env:
  - name: ARCA_LOG
    value: "debug"
```

For targeted logging (e.g., only auth-related):

```bash
ARCA_LOG=info,arca_auth=debug
```

### Verify S3 connectivity

Quick smoke test from the command line:

```bash
# Health check (no auth required)
curl -s http://arca:9000/admin/health | jq .

# List buckets (requires credentials)
aws s3 ls --endpoint-url http://arca:9000

# With debug output
aws s3 ls --endpoint-url http://arca:9000 --debug 2>&1 | head -50
```

### Inspect SigV4 signature issues

If clients get `SignatureDoesNotMatch`, enable debug logging and look for:

```
ARCA_LOG=debug,arca_auth=trace
```

The auth module logs the canonical request and string-to-sign, which can be compared against the client's computed values. Common causes:

- Clock skew between client and server (>15 minutes)
- `Host` header mismatch (see [reverse proxy section](#clients-get-signaturedoesnotmatch-after-adding-a-reverse-proxy))
- Client using SigV2 instead of SigV4 (boto3 defaults to SigV2 for presigned URLs, use `Config(signature_version='s3v4')`)
