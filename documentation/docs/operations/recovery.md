# Disaster Recovery

Arca writes a `.meta` sidecar file alongside every blob, so that the object index can be rebuilt from the filesystem if the metadata database is lost. This page explains the recovery architecture, the tools, and what a rebuild from sidecars can and cannot restore.

!!! warning "Known limitations of `arca recover`"
    Until [TD-014 and TD-032](../tech-debt.md) are fixed, a database rebuilt by `arca recover` is **not** equivalent to the one it replaces:

    - **Only buckets, objects and credentials are restored.** Users, teams, grants, every bucket configuration (versioning, encryption, lifecycle, Object Lock, policy, notifications, replication), bucket and object tags, retention and legal hold are lost, and every object's owner is set to `root`.
    - **The oldest version of each key wins**, not the newest (see [Duplicate keys and versions](#duplicate-keys-and-versions)).
    - **Multipart objects are dropped**, and their parts reappear as bogus `key#uploadId#n` objects.
    - **Compressed objects are skipped** unless you pass `--skip-verify`.
    - **It always writes a SQLite database**, even on a PostgreSQL deployment.

    Treat `arca recover` as a last resort for salvaging object data, and prefer restoring a backup of the metadata database (see [Backup Strategy](#backup-strategy)).

## Filesystem as Source of Object Data

Every object stored in Arca produces two files:

1. **Blob file**: the object bytes (encrypted and/or compressed when those features are enabled), named by UUID
2. **`.meta` sidecar**: a JSON file describing the object (bucket, key, size, ETag, content type, metadata, encryption and compression parameters)

The deliberate write order ensures recoverability:

```
1. Write blob file  →  2. Write .meta sidecar  →  3. Insert into the metadata database
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
  "size": 1234567,
  "etag": "d41d8cd98f00b204e9800998ecf8427e",
  "content_type": "image/jpeg",
  "last_modified": "2026-02-27T14:30:00Z",
  "metadata": {"x-amz-meta-author": "pietro"}
}
```

| Field | Description |
|-------|-------------|
| `bucket`, `key` | Object location |
| `size` | Plaintext size in bytes |
| `etag` | S3 ETag (the MD5 of the plaintext for single-part objects) |
| `content_type` | Content type, or `null` |
| `last_modified` | RFC 3339 timestamp |
| `metadata` | User and system metadata (`x-amz-meta-*`, `cache-control`, ...); empty when absent |
| `encryption` | *Optional.* Present on encrypted blobs: `algorithm` (`AES256` or `SSE-C`), `encrypted_dek`, `dek_nonce`, `nonce_prefix`, `key_id` |
| `compression` | *Optional.* Present on compressed blobs: `algorithm`, `chunk_size`, `original_size`, `compressed_size` |
| `version_id` | *Optional.* Version ID of an object written to a versioned bucket |
| `composite` | *Optional.* Present on a multipart object assembled without copying its parts: the list of parts (`blob_id`, `plaintext_size`, `plaintext_etag`, optional `encryption`). No blob file exists for a composite sidecar; reads stream from the parts. |

## Backup Strategy

To back up an Arca instance, you need:

| What | Path | Why |
|------|------|-----|
| Blob files + sidecars | `{data_dir}/blobs/` | Object data + metadata for recovery |
| Metadata database | `{data_dir}/arca.db` (SQLite), or your PostgreSQL database | Everything else: users, teams, grants, credentials, bucket configuration, tags, versions, retention, and the object index |
| Config file | `/etc/arca/config.toml` | Server settings |

!!! tip
    Back up the metadata database together with the blobs directory. `arca recover` can salvage the objects from the blobs directory alone, but most of the database content cannot be rebuilt from sidecars (see the warning at the top of this page).

## `arca recover`

Rebuild the database from sidecar files.

```bash
arca recover [--config-path <PATH>] [--dry-run] [--skip-verify]
```

### Step-by-Step Process

1. Walks `{data_dir}/blobs/` recursively, reading all `.meta` sidecar files
2. Verifies each blob file exists and its MD5 matches the sidecar ETag (unless `--skip-verify`)
3. Preserves credentials from the existing SQLite database at `{data_dir}/arca.db` (if any)
4. Deletes that SQLite database and creates a fresh one (which contains only the root user and the built-in grants)
5. Restores the saved credentials
6. Recreates every bucket found in the sidecars (unversioned, with no configuration) and inserts one object per sidecar, owned by `root`

`arca recover` always works on the SQLite file in `data_dir`, regardless of `metadata_backend`. On a PostgreSQL deployment it does not touch the PostgreSQL database: it writes an SQLite file the server will not use (TD-032).

### What Is Restored and What Is Lost

| Restored | Lost |
|----------|------|
| Buckets (names only) | Users, teams and grants (only the root user and the built-in grants of a fresh database exist afterwards; credentials of other users are restored, their owning users are not) |
| Objects: bucket, key, size, ETag, content type, last-modified, metadata, encryption parameters | Bucket configuration: versioning, encryption, lifecycle, Object Lock, policy, notifications, replication |
| Credentials, when the old SQLite database is still readable | Bucket and object tags |
| | Retention and legal hold |
| | Object ownership (every object is owned by `root`) |
| | Older versions of versioned objects, and version IDs |
| | Multipart (composite) objects |

### Options

| Option | Description |
|--------|-------------|
| `--dry-run` | Print what would be recovered without modifying the database |
| `--skip-verify` | Skip MD5 checksum verification of blob files (faster, and required to keep compressed objects) |
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

### Duplicate Keys and Versions

When several sidecars claim the same `bucket + key` (the versions of a versioned object, or overwritten objects whose old blob was not yet reclaimed), the **oldest** one wins. This is a known bug (TD-032 in [Technical Debt](../tech-debt.md)): sidecars are inserted newest first by `last_modified`, the buckets are re-created unversioned, and every insert overwrites the previous one, so the last insert (the oldest sidecar) is what remains. The version IDs are discarded.

### Edge Cases

- **Multipart objects**: a multipart object assembled as a composite has a sidecar but no blob file, so it is reported as an orphaned sidecar, skipped with a warning, and missing from the rebuilt database. Its part blobs carry ordinary sidecars whose key is `{key}#{upload_id}#{part_number}`, so each part is re-created as a separate, bogus object under that key (TD-014 in [Technical Debt](../tech-debt.md)). Other objects with a multipart ETag (containing `-`) skip checksum verification, because the composite ETag is not the MD5 of the blob.
- **Encrypted objects**: checksum verification is skipped (with a note), because the ciphertext on disk cannot be compared with the plaintext ETag
- **Compressed objects**: the ETag is the MD5 of the plaintext but recover hashes the compressed bytes on disk, so verification fails and the object is skipped with a checksum-mismatch warning. Run with `--skip-verify` to keep them (TD-032).
- **Orphaned sidecars** (no blob file): skipped with a warning
- **Malformed JSON**: skipped with a warning
- **Checksum mismatches**: skipped with a warning (blob may be corrupted)

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

`arca fsck` relies on two checks:

| What | How | Check |
|------|-----|-------|
| Blob content | MD5 of the blob file compared with the ETag stored in the database (the ETag of a single-part object is its MD5 hex digest, as the S3 protocol requires) | `CORRUPT` (only with `--verify-checksums`) |
| `.meta` sidecar | Field-by-field comparison of `bucket`, `key`, `size` and `etag` with the database record; an unreadable or malformed sidecar is also reported | `SIDECAR_MISMATCH` |

!!! note
    These checks require the database to be intact. During `arca recover`, sidecars are trusted by necessity since there is nothing to compare against.
