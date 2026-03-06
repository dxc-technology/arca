<p align="center">
  <img src="assets/logo.svg" width="120" alt="Arca logo">
</p>

# Arca

**Open source S3-compatible object storage server written in Rust.**

Arca is a ground-up implementation of the S3 API, designed for 100% compatibility on a focused subset of operations. It starts as a single-node server with a clear path toward production scale.

## Why "Arca"?

**Arca** is a Latin word meaning *chest*, *ark*, and *archive* — a container built to preserve what matters most. Four letters, no ambiguity, universally accessible: your data, preserved.

## Key Features

- **S3 API compatible** — works with aws-cli, boto3, rclone, and any S3 client
- **Streaming-first** — never buffers full objects in memory
- **Disaster recovery** — sidecar `.meta` files alongside every blob enable full database rebuild
- **Modular storage** — metadata backend behind traits (SQLite now, Postgres later)
- **Web console** — browser-based UI for managing buckets, objects, and credentials
- **Admin API** — JSON endpoints for monitoring and credential management
- **Written in Rust** — memory safety, performance, zero-cost abstractions

## MVP API Surface

| Category  | Operations                                                            |
|-----------|-----------------------------------------------------------------------|
| Bucket    | CreateBucket, DeleteBucket, HeadBucket, ListBuckets, GetBucketLocation |
| Object    | PutObject, GetObject, DeleteObject, HeadObject, CopyObject, DeleteObjects |
| Listing   | ListObjectsV1, ListObjectsV2                                          |
| Multipart | CreateMultipartUpload, UploadPart, CompleteMultipartUpload, AbortMultipartUpload |
| Auth      | AWS Signature V4                                                      |

## Documentation

<div class="grid cards" markdown>

- **[Getting Started](getting-started/installation.md)** — install, build, and run Arca in minutes
- **[User Guide](guide/configuration.md)** — configuration, web console, CLI reference
- **[Reference](reference/s3-api.md)** — S3 API, Admin API, and architecture details
- **[Operations](operations/recovery.md)** — disaster recovery, monitoring, production deployment

</div>

## License

Arca is licensed under the [GNU Affero General Public License](https://github.com/dxc-technology/arca/blob/main/LICENSE-AGPL-3.0), either version 3 of the License or (at your option) any later version (SPDX: `AGPL-3.0-or-later`).
