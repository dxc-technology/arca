# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Arca is an open source S3-compatible object storage server written in Rust. It targets 100% S3 API compatibility for a focused MVP subset (15 operations), starting as single-node with eventual production scale in mind. Licensed under AGPL-3.0-or-later.

The full architecture plan lives in `.claude/plans/arca-s3-mvp-architecture.md`.

## Build & Test

All development happens inside Docker containers — never install libraries on the host.

```bash
# Build and run
docker compose -f docker/docker-compose.yml up --build

# Run all Rust unit tests (inside container)
docker compose -f docker/docker-compose.yml run --rm unit-test

# Run a single crate's tests
docker compose -f docker/docker-compose.yml run --rm unit-test test -p arca-core
docker compose -f docker/docker-compose.yml run --rm unit-test test -p arca-auth

# Run a single test by name
docker compose -f docker/docker-compose.yml run --rm unit-test test -p arca-auth sigv4_test

# Integration tests (boto3/pytest, run against live Arca — server must be up)
docker compose -f docker/docker-compose.yml run --rm test
docker compose -f docker/docker-compose.yml run --rm test pytest integration/test_smoke.py -k test_root

# Manual S3 CLI verification against running Arca
aws s3 ls --endpoint-url http://localhost:9000

# Build documentation site (output to docs/)
docker compose -f docker/docker-compose.docs.yml run --rm docs-build

# Serve documentation locally with live reload (http://localhost:8000)
docker compose -f docker/docker-compose.docs.yml up docs-serve
```

## Architecture

Five-crate Cargo workspace with strict dependency graph (no cycles):

- **arca-core** — Shared types, traits (`BlobStore`, `MetadataStore`), error types. Zero I/O dependencies.
- **arca-auth** — AWS SigV4 verification. Zero I/O dependencies, independently testable against AWS test vectors.
- **arca-proto** — S3 HTTP protocol adapter (Axum 0.8 + Tower). Handlers, XML ser/de, middleware (virtual-host rewrite, auth).
- **arca-storage** — Storage implementations: `FsBlobStore` (UUID + sidecar), `SqliteMetadataStore` (tokio-rusqlite, WAL mode).
- **arca-server** — Binary crate. Wires dependencies, config (TOML + env), CLI (clap: `serve`, `recover`, `fsck`), use-case layer (`BucketUsecase`, `ObjectUsecase`, `MultipartUsecase`).

Dependency direction: `arca-server` -> `arca-proto`, `arca-storage`, `arca-auth` -> `arca-core`.

## Key Design Constraints

**No `s3s` crate** — we build our own S3 HTTP protocol adapter with Axum. This was an explicit decision to avoid pre-1.0 dependency risk.

**S3 query-parameter routing** — S3 overloads the same HTTP method+path with different query params. Handlers dispatch internally (e.g., `PUT /:bucket/*key` -> PutObject vs UploadPart based on `?partNumber=&uploadId=`).

**Streaming-first** — never buffer full objects in memory. PutObject streams through MD5 hasher + file writer concurrently. GetObject streams from tokio::fs::File via ReaderStream.

**Storage write order** — blob file -> sidecar `.meta` JSON -> SQLite insert. This ordering enables disaster recovery: `arca recover` walks the data directory, reads `.meta` files, and rebuilds the DB from scratch.

**`put_object` returns old record** — `MetadataStore::put_object` returns `Option<ObjectRecord>` of the overwritten object so the caller can delete the orphaned blob.

**Multipart composite ETag** — `hex(MD5(binary_MD5(part1) || binary_MD5(part2) || ...))-{count}`. Note: concat the binary MD5 bytes, not hex strings.

**Auth body hash** — Accept `UNSIGNED-PAYLOAD` for streaming uploads. Verify request signature only (headers + URI), not body hash. Body integrity relies on Content-MD5.

## Not in MVP

Object versioning, ACLs/bucket policies, server-side encryption, object tagging, lifecycle rules, CORS, object lock, presigned URLs, admin API, metrics, replication, multi-node.
