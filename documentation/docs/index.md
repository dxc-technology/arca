<p align="center">
  <img src="assets/logo.svg" width="120" alt="Arca logo">
</p>

# Arca

**Open source S3-compatible object storage server written in Rust.**

Arca is a ground-up implementation of the S3 API, designed for 100% compatibility on a focused subset of operations. It starts as a single-node server with a clear path toward production scale.

## Key Features

- **S3 API compatible** — works with aws-cli, boto3, rclone, and any S3 client
- **Streaming-first** — never buffers full objects in memory
- **Disaster recovery** — sidecar `.meta` files alongside every blob enable full database rebuild
- **Modular storage** — metadata backend behind traits (SQLite now, Postgres later)
- **Written in Rust** — memory safety, performance, zero-cost abstractions

## Quick Start

```bash
# Build and run with Docker
docker compose -f docker/docker-compose.yml up --build
```

Arca listens on port **9000** (MinIO-compatible convention).

```bash
# Create a bucket
aws s3 mb s3://my-bucket --endpoint-url http://localhost:9000

# Upload a file
aws s3 cp myfile.txt s3://my-bucket/ --endpoint-url http://localhost:9000

# List objects
aws s3 ls s3://my-bucket --endpoint-url http://localhost:9000
```

## MVP API Surface

| Category  | Operations                                                            |
|-----------|-----------------------------------------------------------------------|
| Bucket    | CreateBucket, DeleteBucket, HeadBucket, ListBuckets                   |
| Object    | PutObject, GetObject, DeleteObject, HeadObject, CopyObject            |
| Listing   | ListObjectsV2                                                         |
| Multipart | CreateMultipartUpload, UploadPart, CompleteMultipartUpload, AbortMultipartUpload |
| Auth      | AWS Signature V4                                                      |

See the full [API Reference](api.md) for details on each operation, and the [Architecture](architecture.md) for how the system is designed.

## License

Arca is licensed under the [GNU Affero General Public License](https://github.com/dxc-technology/arca/blob/main/LICENSE-AGPL-3.0), either version 3 of the License or (at your option) any later version (SPDX: `AGPL-3.0-or-later`).
