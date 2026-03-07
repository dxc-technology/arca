# Quick Start

Get Arca running and perform your first S3 operations.

## Start the Server

```bash
bin/arca start -d --build
```

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

Set the credentials in your shell:

```bash
export AWS_ACCESS_KEY_ID=<your-access-key>
export AWS_SECRET_ACCESS_KEY=<your-secret-key>
```

## Your First Bucket

```bash
# Create a bucket
aws s3 mb s3://my-bucket --endpoint-url http://localhost:9000

# List buckets
aws s3 ls --endpoint-url http://localhost:9000
```

## Upload and Download Objects

```bash
# Upload a file
aws s3 cp myfile.txt s3://my-bucket/ --endpoint-url http://localhost:9000

# List objects
aws s3 ls s3://my-bucket --endpoint-url http://localhost:9000

# Download a file
aws s3 cp s3://my-bucket/myfile.txt downloaded.txt --endpoint-url http://localhost:9000
```

## Start the Web Console

Arca includes a browser-based web console for managing buckets, objects, and credentials:

```bash
bin/console start -d --build
```

Open [http://localhost:9080](http://localhost:9080), enter the Arca endpoint (`http://localhost:9000`) and your credentials to get started. Admin credentials unlock additional features like credential management and server stats.

See the [Web Console guide](../guide/console.md) for details.

## Development Scripts

Convenience scripts in `bin/` wrap docker compose commands:

| Script | Description |
|--------|-------------|
| `bin/build` | Build the Docker image |
| `bin/arca` | Manage the Arca server (`start`, `stop`, `status`, `logs`) |
| `bin/console` | Manage the web console (`start`, `stop`, `status`, `logs`) |
| `bin/test` | Run unit + integration tests (`unit`, `integration`, or both) |
| `bin/screenshots` | Take automated console screenshots for documentation |
| `bin/s3-tests` | Run Ceph s3-tests compatibility suite |
| `bin/perf-test` | Run performance tests |
| `bin/docs-build` | Build the documentation site |
| `bin/docs-serve` | Serve docs locally with live reload |
| `bin/docs-publish` | Build, commit, and push docs to update GitHub Pages |

## Next Steps

- [Configuration](../guide/configuration.md) — customize server settings and manage credentials
- [S3 API Reference](../reference/s3-api.md) — full details on supported operations
- [Web Console](../guide/console.md) — browser-based bucket and credential management
