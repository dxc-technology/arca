# Quick Start

Get Arca running and perform your first S3 operations.

## Start the Server

=== "HTTP"

    ```bash
    bin/arca start -d --build
    ```

=== "Published image"

    No build needed. With the repository checked out:

    ```bash
    bin/arca start -d --release latest
    ```

    Or with plain Docker, without even cloning the repository:

    ```bash
    docker run -d --name arca -p 9000:9000 -v arca-data:/data \
        ghcr.io/dxc-technology/arca:latest
    docker logs arca | grep "Access Key"
    ```

    The image is public and multi-arch. See [Installation](installation.md#pre-built-images) for the available tags.

=== "HTTPS"

    ```bash
    bin/arca start -d --build --tls
    ```

    Arca serves your certificate and key if you put their PEM files in `certs/`
    at the repository root. Otherwise it creates a local CA and a certificate
    for `localhost` on the first start, and keeps them valid. See the
    [TLS guide](../guide/tls.md) for how to trust the local CA.

!!! tip
    Drop the `-d` flag to run in the foreground and see logs in real time. Press ++ctrl+c++ to stop the server.

On first startup, Arca auto-generates a root credential and prints it to the logs:

```bash
bin/arca logs | grep "Access Key"
```

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

Configure your S3 client:

=== "aws-cli"

    ```bash
    export AWS_ACCESS_KEY_ID=<your-access-key>
    export AWS_SECRET_ACCESS_KEY=<your-secret-key>
    ```

=== "MinIO Client (mc)"

    ```bash
    mc alias set arca http://localhost:9000 <your-access-key> <your-secret-key>
    ```

## Your First Bucket

=== "aws-cli"

    ```bash
    # Create a bucket
    aws s3 mb s3://my-bucket --endpoint-url http://localhost:9000

    # List buckets
    aws s3 ls --endpoint-url http://localhost:9000
    ```

=== "MinIO Client (mc)"

    ```bash
    # Create a bucket
    mc mb arca/my-bucket

    # List buckets
    mc ls arca
    ```

## Upload and Download Objects

=== "aws-cli"

    ```bash
    # Upload a file
    aws s3 cp myfile.txt s3://my-bucket/ --endpoint-url http://localhost:9000

    # List objects
    aws s3 ls s3://my-bucket --endpoint-url http://localhost:9000

    # Download a file
    aws s3 cp s3://my-bucket/myfile.txt downloaded.txt --endpoint-url http://localhost:9000
    ```

=== "MinIO Client (mc)"

    ```bash
    # Upload a file
    mc cp myfile.txt arca/my-bucket/

    # List objects
    mc ls arca/my-bucket

    # Download a file
    mc cp arca/my-bucket/myfile.txt downloaded.txt
    ```

## Start the Web Console

Arca includes a browser-based web console for managing buckets, objects, and credentials:

=== "HTTP"

    ```bash
    bin/console start -d --build
    ```

    Open [http://localhost:9080](http://localhost:9080), enter the Arca endpoint (`http://localhost:9000`) and your credentials.

=== "Published image"

    No build needed. With the repository checked out:

    ```bash
    bin/console start -d --release latest
    ```

    Or with plain Docker, without even cloning the repository:

    ```bash
    docker run -d --name arca-console -p 9080:8080 \
        -e ARCA_ENDPOINT=http://localhost:9000 \
        ghcr.io/dxc-technology/arca-console:latest
    ```

    The image is public and multi-arch. The container listens on port 8080 (mapped to 9080 here) and runs as the unprivileged user `10001:10001`. `ARCA_ENDPOINT` is the Arca URL the browser connects to. See [Installation](installation.md#pre-built-images) for the available tags.

=== "HTTPS"

    ```bash
    bin/console start -d --build
    ```

    The console has no `--tls` flag: it inherits TLS from the Arca server started with `bin/arca start -d --tls`.

    Open `https://your-domain:9443`, then enter the Arca HTTPS endpoint (e.g. `https://your-domain:9000`) and your credentials.

!!! tip
    As with `bin/arca`, drop the `-d` flag to see console container logs in real time. Stop it with `bin/console stop` (`docker stop arca-console` for the plain Docker run).

Admin credentials unlock additional features like credential management and server stats.

See the [Web Console guide](../guide/console.md) for details.

## Development Scripts

Convenience scripts in `bin/` wrap docker compose commands:

| Script | Description |
|--------|-------------|
| `bin/build` | Build the Docker image |
| `bin/arca` | Manage the Arca server (`start`, `stop`, `status`, `logs`). Use `--tls` for HTTPS, `-c <path>` for a custom config. |
| `bin/console` | Manage the web console (`start`, `stop`, `status`, `logs`). Inherits TLS and the other features from the running Arca. |
| `bin/test` | Run unit + integration tests (`unit`, `integration`, or both) |
| `bin/screenshots` | Take automated console screenshots for documentation |
| `bin/s3-tests` | Run Ceph s3-tests compatibility suite |
| `bin/perf-test` | Run performance tests (`suite` benchmarks Arca against MinIO, see [Performance Testing](../operations/performance.md)) |
| `bin/docs-build` | Build the documentation site |
| `bin/docs-serve` | Serve docs locally with live reload |
| `bin/docs-publish` | Build, commit, and push docs to update GitHub Pages |

## Next Steps

- [Configuration](../guide/configuration.md) — customize server settings and manage credentials
- [S3 API Reference](../reference/s3-api.md) — full details on supported operations
- [Web Console](../guide/console.md) — browser-based bucket and credential management
