# Encryption

Arca supports server-side encryption at rest (SSE-S3) using AES-256-GCM. When enabled, objects are transparently encrypted before writing to disk and decrypted on read. Encryption uses an envelope scheme: each object gets a random data encryption key (DEK) that is wrapped by a master key (KEK) from the server configuration.

## Quick Start

### 1. Generate a master key

```bash
bin/arca start -d --build    # ensure the image is built
docker compose -f docker/docker-compose.yml run --rm arca arca encryption generate-key
```

This outputs a base64-encoded 256-bit key. Save it securely.

### 2. Configure encryption

Add the `[encryption]` section to your `config.toml`:

```toml
[encryption]
enabled = true
master_key = "K+GgqdCNLvpsInhk8NWVJtXbfXt78A1zUc7QO343SJA="
```

### 3. Start the server

```bash
bin/arca start -d --build -c path/to/your-config.toml
```

The `-c` (`--config`) flag mounts the specified file as the server config inside the container. Without it, the server uses the default `config/default.toml`.

Check the logs to confirm encryption is active:

```
INFO arca: Server-side encryption enabled (AES-256-GCM) key_id="6602a944"
```

### 4. Verify

```bash
aws s3 cp myfile.txt s3://my-bucket/ --endpoint-url http://localhost:9000

# Check the response header
aws s3api head-object --bucket my-bucket --key myfile.txt --endpoint-url http://localhost:9000
```

The `ServerSideEncryption` field in the response will show `AES256`.

## How It Works

### Envelope Encryption

Each object is encrypted with a unique, randomly generated 256-bit DEK (data encryption key). The DEK is then wrapped (encrypted) using the master key (KEK) from the config file and stored alongside the object in the sidecar `.meta` file. The master key never touches the object data directly.

```
Master Key (KEK)              Per-Object DEK
  from config.toml              random 32 bytes
  |                              |
  | [AES-256-GCM wrap]         [AES-256-GCM encrypt]
  +-------> Wrapped DEK         Object Data (chunked)
            (in .meta)
```

### Chunk-Based Streaming

Objects are encrypted in fixed-size 64 KiB chunks. Each chunk is independently encrypted and authenticated with its own GCM tag. This enables:

- **Streaming**: no need to buffer the entire object in memory
- **Byte range reads**: only the chunks overlapping the requested range are decrypted
- **Integrity**: corruption in one chunk is detected without reading the entire file

### ETag Preservation

The ETag (MD5) is computed on the **plaintext** data, not the ciphertext. This ensures S3 clients that compare ETags continue to work correctly.

## Configuration Reference

```toml
[encryption]
enabled = true                 # Enable server-side encryption (default: false)
master_key = "base64..."       # 256-bit master key (required when enabled)
# previous_master_key = "..."  # Previous key for rotation (optional)
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `enabled` | bool | No | Enable encryption for new objects. Default `false`. |
| `master_key` | string | When enabled | Base64-encoded 256-bit key. Generate with `arca encryption generate-key`. |
| `previous_master_key` | string | No | Previous master key, used for reading objects encrypted with an older key during key rotation. |

## Per-Bucket Encryption

In addition to the global `[encryption]` config, encryption can be configured per-bucket using standard S3 APIs:

```bash
# Set bucket encryption
aws s3api put-bucket-encryption \
  --bucket my-bucket \
  --server-side-encryption-configuration '{"Rules":[{"ApplyServerSideEncryptionByDefault":{"SSEAlgorithm":"AES256"}}]}' \
  --endpoint-url http://localhost:9000

# Check bucket encryption
aws s3api get-bucket-encryption --bucket my-bucket --endpoint-url http://localhost:9000

# Remove per-bucket config (falls back to global default)
aws s3api delete-bucket-encryption --bucket my-bucket --endpoint-url http://localhost:9000
```

## Mixed Mode

Encrypted and unencrypted objects coexist transparently:

- **Enabling encryption**: add `[encryption]` to config and restart. New objects are encrypted; existing objects remain unencrypted but readable.
- **Disabling encryption**: set `enabled = false`. New objects are stored unencrypted. Existing encrypted objects remain readable as long as `master_key` is in the config.
- **Detection**: the sidecar `.meta` file records whether an object is encrypted. No magic bytes or guessing required.

## CLI Commands

### Generate a master key

```bash
arca encryption generate-key
```

Outputs a random 256-bit key encoded as base64. Suitable for the `master_key` config field.

## Recovery and Integrity

### `arca recover`

The recovery tool reads sidecar `.meta` files to rebuild the database. Encrypted objects are imported with their encryption metadata intact. Checksum verification is skipped for encrypted blobs (the on-disk ciphertext MD5 differs from the plaintext ETag stored in the sidecar).

### `arca fsck`

The filesystem check tool skips checksum verification for encrypted objects when using `--verify-checksums`, since the on-disk data is ciphertext.

## Testing

Run the encryption integration tests:

```bash
bin/test encryption
```

This starts an Arca server with encryption enabled and runs 16 tests covering:

- Put/get roundtrip with plaintext verification
- ETag correctness (plaintext MD5)
- SSE response headers on put/get/head/copy
- Empty and large object encryption
- Byte range reads (including cross-chunk boundary)
- Multipart upload with encrypted final blob
- PutBucketEncryption / GetBucketEncryption / DeleteBucketEncryption
- Object overwrite and deletion

## Security Notes

- The master key is stored in the config file. Protect this file with filesystem permissions (e.g., `chmod 600`).
- For production deployments requiring external key management, see Phase 14 (SSE-KMS with HashiCorp Vault/OpenBAO) in the [roadmap](../roadmap.md).
- AES-256-GCM provides both confidentiality and integrity. Each encrypted chunk includes a 16-byte authentication tag that detects tampering.
- Nonces are constructed from a random 4-byte prefix (unique per object) and an 8-byte chunk counter, ensuring no nonce reuse.
