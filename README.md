<p align="center">
  <img src="logo.svg" width="120" alt="Arca logo">
</p>

<h1 align="center">Arca</h1>

<p align="center">
  Open source S3-compatible object storage server written in Rust.<br>
  <a href="https://dxc-technology.github.io/arca/">Documentation</a>
</p>

---

Arca is a ground-up implementation of the S3 API, designed for 100% compatibility on a focused subset of operations. It starts as a single-node server with a clear path toward production scale.

## Features

- **S3 API compatible** — works with aws-cli, boto3, rclone, and any S3 client
- **Streaming-first** — never buffers full objects in memory
- **Disaster recovery** — sidecar `.meta` files alongside every blob enable full database rebuild
- **Modular storage** — metadata backend behind traits (SQLite now, Postgres later)
- **[S3 compatibility tested](https://dxc-technology.github.io/arca/s3-compatibility/)** — validated against Ceph s3-tests (198 passing)

## MVP API Surface

| Category  | Operations                                                            |
|-----------|-----------------------------------------------------------------------|
| Bucket    | CreateBucket, DeleteBucket, HeadBucket, ListBuckets                   |
| Object    | PutObject, GetObject, DeleteObject, HeadObject, CopyObject            |
| Listing   | ListObjectsV1, ListObjectsV2                                          |
| Multipart | CreateMultipartUpload, UploadPart, CompleteMultipartUpload, AbortMultipartUpload |
| Auth      | AWS Signature V4                                                      |

## Quick Start

```bash
docker compose -f docker/docker-compose.yml up --build
```

Arca listens on port **9000** (MinIO-compatible convention).

```bash
aws s3 mb s3://my-bucket --endpoint-url http://localhost:9000
aws s3 cp myfile.txt s3://my-bucket/ --endpoint-url http://localhost:9000
aws s3 ls s3://my-bucket --endpoint-url http://localhost:9000
```

## Architecture

Five-crate Cargo workspace:

| Crate | Role |
|-------|------|
| `arca-core` | Shared types, traits (`BlobStore`, `MetadataStore`), errors. No I/O. |
| `arca-auth` | AWS SigV4 verification. No I/O, independently testable. |
| `arca-proto` | S3 HTTP protocol adapter (Axum + Tower). Handlers, XML, middleware. |
| `arca-storage` | Storage implementations: filesystem blobs (UUID + sidecar), SQLite metadata. |
| `arca-server` | Binary. Config, CLI, use-case layer, dependency wiring. |

## License

Arca is licensed under the [GNU Affero General Public License](LICENSE-AGPL-3.0), either version 3 of the License or (at your option) any later version (SPDX: `AGPL-3.0-or-later`), a strong copyleft license covering network use: derivative works must remain under the same terms.
