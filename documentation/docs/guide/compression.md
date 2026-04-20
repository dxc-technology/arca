# Compression

Arca supports transparent at-rest compression. When a bucket has compression configured, new uploads are compressed before writing to disk and transparently decompressed on read. The S3 wire format is unchanged: `ETag` remains the MD5 of the plaintext and `Content-Length` reports the plaintext size. Compression sits below encryption in the stack, so `compress → encrypt → store` applies when both features are active.

Compression is a **per-bucket**, **console-managed** setting. There is no instance-wide switch and no TOML configuration — the wrapper is always present at runtime and only activates on buckets that explicitly opt in.

## Quick Start

### 1. Enable compression on a bucket

Open the Arca console, navigate to **Buckets → *your bucket* → Settings**, then in the **Compression** card pick an algorithm (and optionally a level) and click **Enable Compression**.

Or use the Arca-specific S3 subresource directly:

```bash
curl -X PUT "http://localhost:9000/my-bucket?compression" \
  --aws-sigv4 "aws:amz:us-east-1:s3" \
  --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY" \
  -H "Content-Type: application/xml" \
  -d '<CompressionConfiguration>
        <Algorithm>zstd</Algorithm>
        <Level>5</Level>
      </CompressionConfiguration>'
```

### 2. Verify

Upload a compressible file and check the on-disk size is smaller than the advertised `Content-Length`:

```bash
aws s3 cp large-log.json s3://my-bucket/key --endpoint-url http://localhost:9000
aws s3api head-object --bucket my-bucket --key key --endpoint-url http://localhost:9000
```

`head-object` returns the plaintext size and original ETag; the blob file on disk is smaller.

The Prometheus endpoint exposes `arca_storage_compression_ratio` and per-algorithm byte counters:

```bash
curl -s http://localhost:9000/admin/metrics | grep compression
```

## Algorithms

Six algorithms are shipped. Pick the one that matches your workload.

| Algorithm | Ratio | CPU (encode) | CPU (decode) | Notes |
|---|---|---|---|---|
| `zstd` | High | Medium | Fast | Best overall balance; default for structured text |
| `lz4` | Low–Medium | Very fast | Very fast | Use when CPU is precious |
| `snappy` | Low | Very fast | Very fast | Similar to lz4 |
| `gzip` | Medium | Medium | Medium | Ubiquitous, good for interop tooling |
| `brotli` | Very high (text) | Slow | Medium | Best ratio for web assets (HTML/CSS/JS) |
| `xz` | Highest | Slow | Medium | Archive-like workloads where ratio dominates |

### Auto mode

With `Algorithm = auto` the wrapper picks per-object using a small deterministic rule table based on `Content-Type` and size. No sampling, no benchmarking — a plain match:

| # | Condition | Choice |
|---|---|---|
| 1 | Size < 4 KiB | `lz4` (CPU-friendly) |
| 2 | `text/html`, `text/css`, `application/javascript`, `image/svg+xml`, `application/xhtml+xml` | `brotli` level 4 |
| 3 | `text/*`, `application/json`, `application/xml`, `application/yaml`, `application/x-ndjson`, `application/x-log`, `application/graphql` | `zstd` level 3 |
| 4 | Anything else | `zstd` level 3 |

The sidecar always records the concrete algorithm chosen; the read path does not depend on the rule table.

## Managing per-bucket compression

### Get current config

```bash
curl -s -X GET "http://localhost:9000/my-bucket?compression" \
  --aws-sigv4 "aws:amz:us-east-1:s3" \
  --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY"
```

```xml
<?xml version="1.0" encoding="UTF-8"?>
<CompressionConfiguration>
  <Algorithm>zstd</Algorithm>
  <Level>5</Level>
</CompressionConfiguration>
```

### Disable compression for a bucket

```bash
curl -X DELETE "http://localhost:9000/my-bucket?compression" \
  --aws-sigv4 "aws:amz:us-east-1:s3" \
  --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY"
```

Presence of the configuration = compression enabled for the bucket. `DELETE` removes the configuration; subsequent uploads pass through uncompressed. Existing compressed objects remain readable (mixed-mode).

## MIME and size filters

To avoid wasting CPU on already-compressed content, the write path always skips objects whose `Content-Type` matches a set of prefixes (`image/`, `video/`, `audio/`, `application/zip`, `application/gzip`, `application/x-7z-compressed`, `application/x-bzip2`, `application/x-xz`, `application/x-rar-compressed`, `application/pdf`) or whose `Content-Length` is below **1 KiB**. These thresholds are baked-in defaults — change the bucket's algorithm to suit your workload, but the MIME / size filters are not tunable.

## Mixed-mode coexistence

Enabling compression affects future uploads only. Existing objects keep their on-disk form and are read back transparently: each sidecar records whether its blob is compressed and with which algorithm, so the read path dispatches per-object.

## Offline retrofit

Two CLI tools let operators compress (or decompress) a data directory after the fact:

```bash
# Preview what would change
arca compress-existing --config /etc/arca/config.toml --dry-run

# Compress everything matching the MIME/size filters
arca compress-existing --config /etc/arca/config.toml

# Limit to one bucket, force an algorithm
arca compress-existing --bucket my-bucket --algorithm brotli

# Reverse the operation
arca decompress-existing --config /etc/arca/config.toml
```

Both tools are atomic per-blob (writes a `.compressing.tmp` then renames) and resumable: re-running skips already-processed blobs because the sidecar is the source of truth. `arca fsck` recognizes and removes abandoned `.compressing.tmp` files.

Default when no `--algorithm` is passed: `auto` (same rule table as the live path).

## Metrics

The Prometheus endpoint always exposes these series:

- `arca_compression_plaintext_bytes_total{algorithm}` — counter, plaintext bytes seen per algorithm.
- `arca_compression_compressed_bytes_total{algorithm}` — counter, on-disk bytes after compression.
- `arca_compression_skipped_total{reason="disabled"|"mime"|"size"}` — counter of writes that bypassed compression.
- `arca_storage_compression_ratio` — gauge, aggregated ratio (plaintext / compressed).

`reason="disabled"` counts uploads to buckets with no compression configuration.

## Troubleshooting

- **ETag mismatches:** not caused by compression — the ETag is always MD5 of plaintext. If they differ, check whether a proxy is re-encoding the body.
- **Smaller-than-plaintext download:** a misconfigured client may have requested `Content-Encoding: gzip` or similar HTTP-level compression. Arca's at-rest compression is transparent to the client and never touches `Content-Encoding`.
- **Slow writes with brotli/xz:** those algorithms trade CPU for ratio. Switch to `zstd` or `lz4` if throughput matters more than disk footprint.
- **Compression not triggering on uploads:** the bucket may have no compression configured, or the content-type matches the built-in MIME skip list, or the object is below 1 KiB. The Prometheus `arca_compression_skipped_total` counter records the reason.
