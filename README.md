<p align="center">
  <img src="logo.svg" width="120" alt="Arca logo">
</p>

<h1 align="center">Arca</h1>

<p align="center">
  Open source S3-compatible object storage server written in Rust.<br>
  <a href="https://dxc-technology.github.io/arca/">Documentation</a>
</p>

<p align="center">
  <a href="https://dxc-technology.github.io/arca/s3-compatibility/"><img src="s3-compatibility-badge.svg" alt="S3 Compatibility"></a>
</p>

---

Arca is a ground-up implementation of the S3 API, designed as a **drop-in replacement** for AWS S3, MinIO, and other S3-compatible storage services. It targets 100% compatibility on a focused subset of operations, starting as a single-node server with a clear path toward production scale.

## Features

- **S3 API compatible** — works with aws-cli, boto3, rclone, and any S3 client
- **Streaming-first** — never buffers full objects in memory
- **Disaster recovery** — sidecar `.meta` files alongside every blob enable full database rebuild
- **Modular storage** — metadata backend behind traits (SQLite now, Postgres later)
- **Web console** — browser-based UI for managing buckets, objects, and credentials
- **[S3 compatibility tested](https://dxc-technology.github.io/arca/s3-compatibility/)** — validated against [Ceph s3-tests](https://github.com/ceph/s3-tests) (269 passing)

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

### Web Console

Arca ships with a browser-based web console for managing buckets, objects, and credentials:

```bash
docker compose -f docker/docker-compose.yml --profile console up -d
```

Open [http://localhost:9080](http://localhost:9080), enter the Arca endpoint and your credentials to get started.

## Architecture

Five-crate Cargo workspace:

| Crate | Role |
|-------|------|
| `arca-core` | Shared types, traits (`BlobStore`, `MetadataStore`), errors. No I/O. |
| `arca-auth` | AWS SigV4 verification. No I/O, independently testable. |
| `arca-proto` | S3 HTTP protocol adapter (Axum + Tower). Handlers, XML, middleware. |
| `arca-storage` | Storage implementations: filesystem blobs (UUID + sidecar), SQLite metadata. |
| `arca-server` | Binary. Config, CLI, use-case layer, dependency wiring. |

## Test Coverage

| Suite | Tests | Details |
|-------|------:|---------|
| Unit tests (Rust) | 172 | arca-auth: 25, arca-storage: 56, arca-core: 17, arca-proto: 19, arca-server: 55 |
| Integration — boto3 | 238 | buckets, objects, list, multipart, folders, auth, admin, phases 2–3 |
| Integration — MinIO | 99 | mirrors boto3 suite + streaming, file-based, data integrity APIs |
| [Ceph s3-tests](https://dxc-technology.github.io/arca/s3-compatibility/) | 829 | 269 pass, 469 fail, 91 skip |
| **Total** | **1,338** | |

## License

Arca is licensed under the [GNU Affero General Public License](LICENSE-AGPL-3.0), either version 3 of the License or (at your option) any later version (SPDX: `AGPL-3.0-or-later`), a strong copyleft license covering network use: derivative works must remain under the same terms.
