<p align="center">
  <img src="assets/logo.svg" width="120" alt="Arca logo">
</p>

# Arca

**Open source S3-compatible object storage server written in Rust.**

Arca is a ground-up implementation of the S3 API, designed for 100% compatibility on a focused subset of operations. It starts as a single-node server with a clear path toward production scale.

## Why "Arca"?

**Arca** is a Latin word meaning *chest*, *ark*, and *archive* — a container built to preserve what matters most.

This single word captures the essence of object storage: a resilient, secure place where data is kept safe over time.

The name carries multiple layers of meaning:

- **Chest** — a sturdy container for valuable objects, just as Arca stores your data reliably and durably.
- **Ark** — evoking Noah's Ark, a symbol of survival, resilience, and safeguarding through adversity.
- **Archive** — directly tied to the purpose of long-term data preservation and retrieval.

There is also a subtle architectural hint hidden in plain sight: *arca* contains the word **arc**, suggesting the solid foundation and architecture on which the project is built — and the community-driven governance that sustains it.

Finally, *arca* is a word that travels well. It is immediately recognizable across Romance languages — Italian, Spanish, Portuguese, Romanian — while remaining short, clear, and easy to pronounce in English, German, French, and beyond. Four letters, no ambiguity, universally accessible.

Arca is not just a name. It is a promise: your data, preserved.

## Key Features

- **S3 API compatible** — works with aws-cli, boto3, rclone, and any S3 client
- **Streaming-first** — never buffers full objects in memory
- **Disaster recovery** — sidecar `.meta` files alongside every blob enable full database rebuild
- **Modular storage** — metadata backend behind traits (SQLite now, Postgres later)
- **Web console** — browser-based UI for managing buckets, objects, and credentials
- **Written in Rust** — memory safety, performance, zero-cost abstractions

## Quick Start

```bash
# Build and run with Docker
bin/run --build -d

# Check logs for auto-generated root credentials
docker compose -f docker/docker-compose.yml logs arca | grep "Access Key"

# Run tests
bin/test
```

On first startup, Arca auto-generates a root credential and prints it to the logs. See [Configuration](configuration.md#credentials-database) for details on managing credentials.

Arca listens on port **9000** (MinIO-compatible convention). All requests require AWS SigV4 authentication.

```bash
# Set credentials (from server logs or ARCA_ROOT_ACCESS_KEY/ARCA_ROOT_SECRET_KEY env vars)
export AWS_ACCESS_KEY_ID=<your-access-key>
export AWS_SECRET_ACCESS_KEY=<your-secret-key>

# Create a bucket
aws s3 mb s3://my-bucket --endpoint-url http://localhost:9000

# Upload a file
aws s3 cp myfile.txt s3://my-bucket/ --endpoint-url http://localhost:9000

# List objects
aws s3 ls s3://my-bucket --endpoint-url http://localhost:9000
```

### Web Console

Arca includes a browser-based web console for managing buckets, objects, and credentials. Start it alongside the server:

```bash
bin/console --build -d
```

Open [http://localhost:9080](http://localhost:9080), enter the Arca endpoint (`http://localhost:9000`) and your credentials to get started. Admin credentials unlock additional features like credential management and server stats.

## Development Scripts

Convenience scripts in `bin/` wrap docker compose commands:

| Script | Description |
|--------|-------------|
| `bin/build` | Build the Docker image |
| `bin/run` | Start the server (flags passed through to docker compose) |
| `bin/stop` | Stop the server |
| `bin/test` | Run unit + integration tests (`unit`, `integration`, or both) |
| `bin/console` | Start the web console (flags passed through to docker compose) |
| `bin/s3-tests` | Run Ceph s3-tests compatibility suite |
| `bin/perf-test` | Run performance tests |
| `bin/docs-build` | Build the documentation site |
| `bin/docs-serve` | Serve docs locally with live reload |
| `bin/docs-publish` | Build, commit, and push docs to update GitHub Pages |

## MVP API Surface

| Category  | Operations                                                            |
|-----------|-----------------------------------------------------------------------|
| Bucket    | CreateBucket, DeleteBucket, HeadBucket, ListBuckets, GetBucketLocation |
| Object    | PutObject, GetObject, DeleteObject, HeadObject, CopyObject, DeleteObjects |
| Listing   | ListObjectsV1, ListObjectsV2                                          |
| Multipart | CreateMultipartUpload, UploadPart, CompleteMultipartUpload, AbortMultipartUpload |
| Auth      | AWS Signature V4                                                      |

See the full [API Reference](api.md) for details on each operation, and the [Architecture](architecture.md) for how the system is designed.

## License

Arca is licensed under the [GNU Affero General Public License](https://github.com/dxc-technology/arca/blob/main/LICENSE-AGPL-3.0), either version 3 of the License or (at your option) any later version (SPDX: `AGPL-3.0-or-later`).
