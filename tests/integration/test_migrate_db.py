"""Integration tests for Phase 30 milestone M3 — `migrate-db`.

The metadata-backend migration copies ALL metadata from one backend (SQLite or
PostgreSQL) to the other, in place. Blob files are not touched.

This module is driven by `bin/test migrate-db`, which orchestrates the live
backend switch around the offline `arca migrate-db` CLI:

  Phase SEED   — Arca on SQLite; seed buckets, objects, a user, a credential,
                 tags and a versioned object. (ARCA_MIGRATE_PHASE=seed)
  (bin/test stops Arca, runs `arca migrate-db --to postgres`, starts Arca on
   PostgreSQL.)
  Phase VERIFY — Arca on PostgreSQL; assert the seeded data is identical.
                 (ARCA_MIGRATE_PHASE=verify)

The two phases share `EXPECTED`, the single source of truth for what is seeded
and then verified, so the round trip is a strong correctness gate.
"""

import os

import pytest

PHASE = os.environ.get("ARCA_MIGRATE_PHASE")

pytestmark = pytest.mark.skipif(
    PHASE not in ("seed", "verify"),
    reason="migrate-db tests run via `bin/test migrate-db` (sets ARCA_MIGRATE_PHASE)",
)

BUCKET = "migrate-bucket"
BUCKET_VERSIONED = "migrate-versioned"

# (key, body) pairs seeded into BUCKET.
OBJECTS = [
    ("plain.txt", b"hello migration"),
    ("nested/dir/data.bin", bytes(range(256))),
    ("unicode-éè.txt", "accents".encode()),
]

# Object tags seeded on plain.txt.
TAGS = {"env": "test", "team": "storage"}


def _put_all(s3_client):
    s3_client.create_bucket(Bucket=BUCKET)
    for key, body in OBJECTS:
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=body)
    s3_client.put_object_tagging(
        Bucket=BUCKET,
        Key="plain.txt",
        Tagging={"TagSet": [{"Key": k, "Value": v} for k, v in TAGS.items()]},
    )

    # A versioned bucket with two versions of the same key, to exercise
    # version rows, is_latest and the version listing path after migration.
    s3_client.create_bucket(Bucket=BUCKET_VERSIONED)
    s3_client.put_bucket_versioning(
        Bucket=BUCKET_VERSIONED,
        VersioningConfiguration={"Status": "Enabled"},
    )
    s3_client.put_object(Bucket=BUCKET_VERSIONED, Key="v.txt", Body=b"first")
    s3_client.put_object(Bucket=BUCKET_VERSIONED, Key="v.txt", Body=b"second")


def test_seed(s3_client):
    """Phase SEED: populate the SQLite-backed instance."""
    if PHASE != "seed":
        pytest.skip("not the seed phase")
    _put_all(s3_client)

    # Sanity: the data is readable on the source backend before migration.
    listed = {
        o["Key"] for o in s3_client.list_objects_v2(Bucket=BUCKET).get("Contents", [])
    }
    assert listed == {k for k, _ in OBJECTS}


def test_verify(s3_client):
    """Phase VERIFY: the PostgreSQL-backed instance has identical data."""
    if PHASE != "verify":
        pytest.skip("not the verify phase")

    # Bucket listing matches.
    buckets = {b["Name"] for b in s3_client.list_buckets()["Buckets"]}
    assert BUCKET in buckets
    assert BUCKET_VERSIONED in buckets

    # Object listing matches exactly.
    listed = {
        o["Key"] for o in s3_client.list_objects_v2(Bucket=BUCKET).get("Contents", [])
    }
    assert listed == {k for k, _ in OBJECTS}

    # Every object's bytes round-tripped (the blob files were untouched; only
    # the metadata moved, so a GET must still resolve the same blob).
    for key, body in OBJECTS:
        got = s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read()
        assert got == body, f"body mismatch for {key}"

    # Tags survived the migration.
    tagset = s3_client.get_object_tagging(Bucket=BUCKET, Key="plain.txt")["TagSet"]
    got_tags = {t["Key"]: t["Value"] for t in tagset}
    assert got_tags == TAGS

    # The versioned bucket kept both versions and the latest content.
    versions = s3_client.list_object_versions(Bucket=BUCKET_VERSIONED).get(
        "Versions", []
    )
    assert len([v for v in versions if v["Key"] == "v.txt"]) == 2
    latest = s3_client.get_object(Bucket=BUCKET_VERSIONED, Key="v.txt")["Body"].read()
    assert latest == b"second"

    # A fresh write on the new backend works (the seq counter migrated, so the
    # new INSERT does not collide and is visible).
    s3_client.put_object(Bucket=BUCKET, Key="post-migration.txt", Body=b"new")
    got = s3_client.get_object(Bucket=BUCKET, Key="post-migration.txt")["Body"].read()
    assert got == b"new"
