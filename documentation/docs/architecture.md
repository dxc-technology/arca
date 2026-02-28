# Architecture

Arca is organized as a five-crate Cargo workspace with a strict dependency graph (no cycles).

## Crate Structure

| Crate | Role |
|-------|------|
| `arca-core` | Shared types, traits (`BlobStore`, `MetadataStore`), errors. No I/O. |
| `arca-auth` | AWS SigV4 verification. No I/O, independently testable. |
| `arca-proto` | S3 HTTP protocol adapter (Axum 0.8 + Tower). Handlers, XML, middleware. |
| `arca-storage` | Storage implementations: filesystem blobs (UUID + sidecar), SQLite metadata. |
| `arca-server` | Binary. Config, CLI, use-case layer, dependency wiring. |

## Dependency Graph

```
arca-server (binary)
  ├── arca-proto     (HTTP layer)
  ├── arca-storage   (storage impls)
  ├── arca-auth      (auth verification)
  └── arca-core      (shared types/traits)

arca-proto
  ├── arca-core
  └── arca-auth

arca-auth
  └── arca-core

arca-storage
  └── arca-core
```

Dependencies flow strictly downward. `arca-core` has zero I/O dependencies — it defines only types, traits, and error types. This keeps the core domain testable and portable.

## Request Flow

```
                 HTTP Request
                      |
             +--------v--------+
             |  Tower Middleware | (tracing, timeouts)
             +--------+--------+
                      |
             +--------v--------+
             | Virtual Host    | Rewrites bucket.s3.domain -> /bucket/path
             | Middleware      |
             +--------+--------+
                      |
             +--------v--------+
             | SigV4 Auth     | Verifies AWS Signature V4
             | Middleware      | Injects identity into extensions
             +--------+--------+
                      |
             +--------v--------+
             | Axum Router    | Routes by method + path
             | (arca-proto)   | Dispatches by query params
             +--------+--------+
                      |
             +--------v--------+
             | Use Cases      | Business logic
             | (arca-server)  | Bucket/Object/Multipart
             +--------+--------+
                      |
           +----------+----------+
           |                     |
  +--------v--------+  +--------v--------+
  | MetadataStore   |  |   BlobStore     |
  | (trait)         |  |   (trait)       |
  +--------+--------+  +--------+--------+
           |                     |
  +--------v--------+  +--------v--------+
  | SqliteMetadata  |  | FsBlobStore     |
  |                 |  | UUID + sidecar  |
  +-----------------+  +-----------------+
           |                     |
      SQLite DB          data/ab/cd/{uuid}
      (WAL mode)         data/ab/cd/{uuid}.meta
```

## Key Design Decisions

### No `s3s` crate

We build our own S3 HTTP protocol adapter with Axum rather than depending on the `s3s` crate. This was an explicit decision to avoid pre-1.0 dependency risk and maintain full control over the protocol layer.

### S3 Query-Parameter Routing

S3 overloads the same HTTP method + path with different query parameters. Since Axum doesn't support query-based routing, handlers dispatch internally:

| Route | Without query params | With query params |
|-------|---------------------|-------------------|
| `PUT /:bucket/*key` | PutObject | UploadPart (`?partNumber=&uploadId=`) |
| `GET /:bucket` | ListObjectsV2 (`?list-type=2`) | HeadBucket |
| `POST /:bucket/*key` | CreateMultipartUpload (`?uploads`) | CompleteMultipartUpload (`?uploadId=`) |
| `DELETE /:bucket/*key` | DeleteObject | AbortMultipartUpload (`?uploadId=`) |

This is the same approach used by MinIO, Ceph RGW, and RustFS.

### Streaming-First

Arca never buffers full objects in memory:

- **PutObject**: Body stream tees to MD5 hasher + file writer concurrently. Uses `UNSIGNED-PAYLOAD` for auth.
- **GetObject**: `tokio::fs::File` streams via `ReaderStream`. Range requests use seek + take.
- **UploadPart**: Same streaming as PutObject, into temporary part file.

### Storage Write Order

Write order is: blob file -> sidecar `.meta` JSON -> SQLite insert. This ordering enables disaster recovery: `arca recover` walks the data directory, reads `.meta` files, and rebuilds the database from scratch.

### Sidecar `.meta` Format

Every blob is accompanied by a JSON sidecar file:

```json
{
  "bucket": "my-bucket",
  "key": "photos/2024/vacation.jpg",
  "content_type": "image/jpeg",
  "size": 1234567,
  "etag": "d41d8cd98f00b204e9800998ecf8427e",
  "user_metadata": {"x-amz-meta-author": "pietro"},
  "created_at": "2026-02-27T14:30:00Z"
}
```

### `put_object` Returns Old Record

`MetadataStore::put_object` returns `Option<ObjectRecord>` of the overwritten object so the caller can delete the orphaned blob. Without this, blobs would accumulate unboundedly on repeated writes to the same key.

### Multipart Composite ETag

The composite ETag follows the S3 standard: `hex(MD5(binary_MD5(part1) || binary_MD5(part2) || ...))-{count}`. Note that binary MD5 bytes are concatenated, not hex strings.

### Auth Body Hash Strategy

For streaming uploads, buffering multi-gigabyte objects for SHA-256 is impractical. S3 clients send `x-amz-content-sha256: UNSIGNED-PAYLOAD`. Auth middleware verifies the request signature (headers + URI) only, not the body hash. Body integrity is separately guaranteed by `Content-MD5`.
