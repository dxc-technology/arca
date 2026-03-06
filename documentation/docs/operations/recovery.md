# Disaster Recovery

Arca is designed so that the filesystem is the **source of truth**. The SQLite database is a queryable index that can always be rebuilt from the `.meta` sidecar files stored alongside every blob. This page explains the recovery architecture and tools.

## Filesystem as Source of Truth

Every object stored in Arca produces two files:

1. **Blob file** — the raw object bytes, named by UUID
2. **`.meta` sidecar** — a JSON file containing all metadata needed to reconstruct the database entry

The deliberate write order ensures recoverability:

```
1. Write blob file  →  2. Write .meta sidecar  →  3. Insert into SQLite
```

If the server crashes at any point:

- **After step 1 only**: orphaned blob, no sidecar — `arca fsck` detects and reports it
- **After step 2**: blob + sidecar exist but DB doesn't know — `arca recover` rebuilds the DB entry
- **After step 3**: fully consistent

## Sidecar `.meta` Format

Each sidecar file is a JSON document:

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

## Backup Strategy

To back up an Arca instance, you need:

| What | Path | Why |
|------|------|-----|
| Blob files + sidecars | `{data_dir}/blobs/` | Object data + metadata for recovery |
| SQLite database | `{data_dir}/arca.db` | Credentials, metadata index (can be rebuilt) |
| Config file | `/etc/arca/config.toml` | Server settings |

!!! tip
    Even if you lose the SQLite database, you can recover all objects from the blobs directory alone using `arca recover`. The only data stored exclusively in the database is **credentials** — these are preserved during recovery when the old database is still readable.

## `arca recover`

Rebuild the database from sidecar files.

```bash
arca recover [--config-path <PATH>] [--dry-run] [--skip-verify]
```

### Step-by-Step Process

1. Walks `{data_dir}/blobs/` recursively, reading all `.meta` sidecar files
2. Verifies each blob file exists and its MD5 matches the sidecar ETag (unless `--skip-verify`)
3. Preserves credentials from the existing database (if any)
4. Deletes the old database and creates a fresh one
5. Recreates all buckets and objects from sidecar data

### Options

| Option | Description |
|--------|-------------|
| `--dry-run` | Print what would be recovered without modifying the database |
| `--skip-verify` | Skip MD5 checksum verification of blob files (faster) |
| `--config-path` | Path to the configuration file (default: `/etc/arca/config.toml`) |

### Usage

Always start with a dry run:

```bash
# Preview recovery
arca recover --dry-run

# Full recovery with checksum verification
arca recover

# Fast recovery (skip checksums — useful for very large datasets)
arca recover --skip-verify
```

### Credential Preservation

During recovery, credentials are read from the existing database before it is replaced. If the old database is readable, credentials survive the recovery. If the database is completely lost, you'll need to let Arca generate new root credentials on the next startup.

### Edge Cases

- **Multipart objects** (ETag contains `-`): checksum verification is skipped because the composite ETag is not a simple MD5 of the assembled blob
- **Orphaned sidecars** (no blob file): skipped with a warning
- **Malformed JSON**: skipped with a warning
- **Checksum mismatches**: skipped with a warning (blob may be corrupted)
- **Duplicate keys**: when multiple sidecars claim the same `bucket + key`, the newest (by `created_at`) wins

## `arca fsck`

Check consistency between the database and filesystem without modifying anything.

```bash
arca fsck [--config-path <PATH>] [--verify-checksums]
```

### Check Types

| Check | Description |
|-------|-------------|
| `ORPHANED_BLOB` | Blob file on disk with no corresponding database record |
| `MISSING_BLOB` | Database record references a blob file that doesn't exist |
| `SIDECAR_MISMATCH` | `.meta` sidecar data doesn't match database record (bucket, key, size, or etag) |
| `ORPHANED_SIDECAR` | `.meta` file exists without a corresponding blob file |
| `STALE_TMP` | Leftover `.tmp` file from an interrupted write |
| `CORRUPT` | Blob file MD5 doesn't match stored ETag (only with `--verify-checksums`) |

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | No issues found |
| 1 | One or more issues detected |

### Usage

```bash
# Quick consistency check
arca fsck

# Full check including blob checksums (slow for large datasets)
arca fsck --verify-checksums
```

### Data Integrity

Arca uses two complementary checksums:

| What | Hash | Stored in | Purpose |
|------|------|-----------|---------|
| Blob content | MD5 | `objects.etag` + `.meta` sidecar | Required by S3 protocol — ETag for non-multipart objects is the MD5 hex digest |
| `.meta` sidecar | SHA-256 | `objects.sidecar_sha256` | Internal integrity — detects sidecar corruption or tampering |

This gives `arca fsck` full coverage: blob corruption is caught by MD5 comparison, sidecar corruption by SHA-256 comparison.

!!! note
    These checks require the database to be intact. During `arca recover`, sidecars are trusted by necessity since there is nothing to compare against.
