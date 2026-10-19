"""Integration tests for Arca Phase 24 — PostgreSQL metadata backend.

These tests verify that the PostgreSQL backend produces identical behavior
to SQLite for core S3 operations. They run against an Arca server configured
with `metadata_backend = "postgres"`.

Executed via `bin/test postgres`.
"""

import hashlib
import io
import json
import os
import time

import psycopg2
import pytest
import requests
from botocore.exceptions import ClientError


# Skip entire module if PostgreSQL backend is not enabled.
pytestmark = pytest.mark.skipif(
    not os.environ.get("ARCA_POSTGRES_BACKEND"),
    reason="PostgreSQL tests require ARCA_POSTGRES_BACKEND to be set",
)


BUCKET = "test-pg-bucket"
BUCKET2 = "test-pg-bucket-2"
ADMIN_BUCKET = "test-pg-admin"

# The database Arca runs on, reached from the test container.
PG_DSN = os.environ.get("ARCA_PG_DSN", "postgresql://arca:arca@postgres:5432/arca")

# Set by `bin/test postgres` around an Arca restart (TestSeqReconciliation).
SEQ_PHASE = os.environ.get("ARCA_PG_SEQ_PHASE")
SEQ_BUCKET = "test-pg-seq"
SEQ_KEY = "raised.txt"
# A seq far above anything the suite writes, planted before the restart.
RAISED_SEQ = 10_000_000

# 5 MB minimum part size for multipart uploads.
PART_SIZE = 5 * 1024 * 1024


@pytest.fixture(autouse=True)
def setup_buckets(s3_client):
    """Ensure test buckets exist before each test, clean up after."""
    for b in [BUCKET, BUCKET2, ADMIN_BUCKET]:
        try:
            s3_client.create_bucket(Bucket=b)
        except ClientError:
            pass
    yield
    for b in [BUCKET, BUCKET2, ADMIN_BUCKET]:
        try:
            # Delete all object versions (handles versioned buckets).
            paginator = s3_client.get_paginator("list_object_versions")
            for page in paginator.paginate(Bucket=b):
                for obj in page.get("Versions", []):
                    s3_client.delete_object(
                        Bucket=b, Key=obj["Key"], VersionId=obj["VersionId"]
                    )
                for marker in page.get("DeleteMarkers", []):
                    s3_client.delete_object(
                        Bucket=b, Key=marker["Key"], VersionId=marker["VersionId"]
                    )
            s3_client.delete_bucket(Bucket=b)
        except ClientError:
            pass


# ---------- Bucket operations ----------


class TestBucketOperations:
    """Basic bucket CRUD."""

    def test_create_and_list_buckets(self, s3_client):
        buckets = s3_client.list_buckets()["Buckets"]
        names = [b["Name"] for b in buckets]
        assert BUCKET in names
        assert BUCKET2 in names

    def test_head_bucket(self, s3_client):
        resp = s3_client.head_bucket(Bucket=BUCKET)
        assert resp["ResponseMetadata"]["HTTPStatusCode"] == 200

    def test_delete_bucket(self, s3_client):
        temp = "test-pg-temp-delete"
        s3_client.create_bucket(Bucket=temp)
        s3_client.delete_bucket(Bucket=temp)
        with pytest.raises(ClientError) as exc_info:
            s3_client.head_bucket(Bucket=temp)
        assert exc_info.value.response["Error"]["Code"] in ("404", "NoSuchBucket")

    def test_bucket_not_found(self, s3_client):
        with pytest.raises(ClientError) as exc_info:
            s3_client.head_bucket(Bucket="nonexistent-bucket-xyz")
        assert exc_info.value.response["Error"]["Code"] in ("404", "NoSuchBucket")


# ---------- Object operations ----------


class TestObjectOperations:
    """Object put/get/head/delete/list."""

    def test_put_get_roundtrip(self, s3_client):
        body = b"hello PostgreSQL backend"
        s3_client.put_object(Bucket=BUCKET, Key="test.txt", Body=body)
        resp = s3_client.get_object(Bucket=BUCKET, Key="test.txt")
        assert resp["Body"].read() == body

    def test_head_object(self, s3_client):
        body = b"head test"
        s3_client.put_object(
            Bucket=BUCKET, Key="head.txt", Body=body, ContentType="text/plain"
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="head.txt")
        assert resp["ContentLength"] == len(body)
        assert resp["ContentType"] == "text/plain"

    def test_delete_object(self, s3_client):
        s3_client.put_object(Bucket=BUCKET, Key="del.txt", Body=b"delete me")
        s3_client.delete_object(Bucket=BUCKET, Key="del.txt")
        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(Bucket=BUCKET, Key="del.txt")
        assert exc_info.value.response["Error"]["Code"] == "404"

    def test_list_objects(self, s3_client):
        for i in range(5):
            s3_client.put_object(
                Bucket=BUCKET, Key=f"list/{i}.txt", Body=f"obj {i}".encode()
            )
        resp = s3_client.list_objects_v2(Bucket=BUCKET, Prefix="list/")
        assert resp["KeyCount"] == 5
        keys = [obj["Key"] for obj in resp["Contents"]]
        assert keys == [f"list/{i}.txt" for i in range(5)]

    def test_user_metadata(self, s3_client):
        meta = {"custom-key": "custom-value", "another": "meta"}
        s3_client.put_object(
            Bucket=BUCKET, Key="meta.txt", Body=b"metadata", Metadata=meta
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="meta.txt")
        assert resp["Metadata"]["custom-key"] == "custom-value"
        assert resp["Metadata"]["another"] == "meta"

    def test_etag_is_md5(self, s3_client):
        body = b"etag test data"
        s3_client.put_object(Bucket=BUCKET, Key="etag.txt", Body=body)
        resp = s3_client.head_object(Bucket=BUCKET, Key="etag.txt")
        expected_md5 = hashlib.md5(body).hexdigest()
        etag = resp["ETag"].strip('"')
        assert etag == expected_md5

    def test_overwrite_object(self, s3_client):
        s3_client.put_object(Bucket=BUCKET, Key="over.txt", Body=b"v1")
        s3_client.put_object(Bucket=BUCKET, Key="over.txt", Body=b"v2")
        resp = s3_client.get_object(Bucket=BUCKET, Key="over.txt")
        assert resp["Body"].read() == b"v2"



# ---------- Byte order ----------

# Names that a locale collation (en_US.UTF-8 on a glibc PostgreSQL) sorts
# differently from S3's byte order: it ignores case and punctuation at first.
LOCALE_TRAPS = ["a_c", "A/x", "B", "ab", "a-b", "a.c", "Zeta", "zeta", "\u00e9t\u00e9"]


class TestByteOrder:
    """Listings follow S3's byte order whatever the database locale (migration 17)."""

    def test_list_objects_in_byte_order(self, s3_client):
        for key in LOCALE_TRAPS:
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"x")
        listed = [o["Key"] for o in s3_client.list_objects_v2(Bucket=BUCKET)["Contents"]]
        # Python orders str by code point, which is UTF-8 byte order.
        assert listed == sorted(LOCALE_TRAPS)

    def test_delimiter_pages_neither_skip_nor_repeat(self, s3_client):
        keys = ["A/1", "a/1", "a/2", "a-b/1", "a_c", "ab/1", "ab/2", "b"]
        for key in keys:
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"x")
        seen = []
        kwargs = {"Bucket": BUCKET, "Delimiter": "/", "MaxKeys": 1}
        while True:
            page = s3_client.list_objects_v2(**kwargs)
            seen += [o["Key"] for o in page.get("Contents", [])]
            seen += [p["Prefix"] for p in page.get("CommonPrefixes", [])]
            if not page["IsTruncated"]:
                break
            kwargs["ContinuationToken"] = page["NextContinuationToken"]
        assert seen == ["A/", "a-b/", "a/", "a_c", "ab/", "b"]

    def test_list_multipart_uploads_in_byte_order(self, s3_client):
        keys = ["a_c", "A/x", "ab", "a-b"]
        ids = [s3_client.create_multipart_upload(Bucket=BUCKET, Key=k)["UploadId"] for k in keys]
        try:
            uploads = s3_client.list_multipart_uploads(Bucket=BUCKET)["Uploads"]
            assert [u["Key"] for u in uploads] == sorted(keys)
        finally:
            for key, upload_id in zip(keys, ids):
                s3_client.abort_multipart_upload(Bucket=BUCKET, Key=key, UploadId=upload_id)

    def test_list_buckets_in_byte_order(self, s3_client):
        names = ["test-pg-order-ab", "test-pg-order-a.c", "test-pg-order-a-b"]
        for name in names:
            s3_client.create_bucket(Bucket=name)
        try:
            listed = [b["Name"] for b in s3_client.list_buckets()["Buckets"]]
            assert [n for n in listed if n in names] == sorted(names)
        finally:
            for name in names:
                s3_client.delete_bucket(Bucket=name)

# ---------- Multipart upload ----------


class TestMultipartUpload:
    """Multipart upload lifecycle."""

    def test_multipart_roundtrip(self, s3_client):
        key = "multipart.bin"
        part1_data = b"A" * PART_SIZE
        part2_data = b"B" * 1024

        # Initiate
        mpu = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = mpu["UploadId"]

        # Upload parts
        p1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id, PartNumber=1, Body=part1_data
        )
        p2 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id, PartNumber=2, Body=part2_data
        )

        # Complete
        s3_client.complete_multipart_upload(
            Bucket=BUCKET,
            Key=key,
            UploadId=upload_id,
            MultipartUpload={
                "Parts": [
                    {"PartNumber": 1, "ETag": p1["ETag"]},
                    {"PartNumber": 2, "ETag": p2["ETag"]},
                ]
            },
        )

        # Verify
        resp = s3_client.get_object(Bucket=BUCKET, Key=key)
        data = resp["Body"].read()
        assert len(data) == PART_SIZE + 1024
        assert data[:PART_SIZE] == part1_data
        assert data[PART_SIZE:] == part2_data

    def test_abort_multipart(self, s3_client):
        mpu = s3_client.create_multipart_upload(Bucket=BUCKET, Key="abort.bin")
        upload_id = mpu["UploadId"]
        s3_client.abort_multipart_upload(
            Bucket=BUCKET, Key="abort.bin", UploadId=upload_id
        )
        # Upload should be gone.
        resp = s3_client.list_multipart_uploads(Bucket=BUCKET)
        upload_ids = [u["UploadId"] for u in resp.get("Uploads", [])]
        assert upload_id not in upload_ids


# ---------- Versioning ----------


class TestVersioning:
    """Object versioning."""

    def test_versioning_lifecycle(self, s3_client):
        # Enable versioning
        s3_client.put_bucket_versioning(
            Bucket=BUCKET,
            VersioningConfiguration={"Status": "Enabled"},
        )
        resp = s3_client.get_bucket_versioning(Bucket=BUCKET)
        assert resp["Status"] == "Enabled"

        # Put two versions
        r1 = s3_client.put_object(Bucket=BUCKET, Key="ver.txt", Body=b"v1")
        r2 = s3_client.put_object(Bucket=BUCKET, Key="ver.txt", Body=b"v2")
        assert r1["VersionId"] != r2["VersionId"]

        # Get latest
        resp = s3_client.get_object(Bucket=BUCKET, Key="ver.txt")
        assert resp["Body"].read() == b"v2"

        # Get specific version
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="ver.txt", VersionId=r1["VersionId"]
        )
        assert resp["Body"].read() == b"v1"

        # List versions
        resp = s3_client.list_object_versions(Bucket=BUCKET, Prefix="ver.txt")
        versions = resp.get("Versions", [])
        assert len(versions) >= 2


# ---------- Tags ----------


class TestTagging:
    """Object and bucket tagging."""

    def test_object_tags(self, s3_client):
        s3_client.put_object(Bucket=BUCKET, Key="tagged.txt", Body=b"tagged")
        s3_client.put_object_tagging(
            Bucket=BUCKET,
            Key="tagged.txt",
            Tagging={"TagSet": [{"Key": "env", "Value": "test"}]},
        )
        resp = s3_client.get_object_tagging(Bucket=BUCKET, Key="tagged.txt")
        tags = {t["Key"]: t["Value"] for t in resp["TagSet"]}
        assert tags == {"env": "test"}

        s3_client.delete_object_tagging(Bucket=BUCKET, Key="tagged.txt")
        resp = s3_client.get_object_tagging(Bucket=BUCKET, Key="tagged.txt")
        assert resp["TagSet"] == []

    def test_bucket_tags(self, s3_client):
        s3_client.put_bucket_tagging(
            Bucket=BUCKET,
            Tagging={"TagSet": [{"Key": "project", "Value": "arca"}]},
        )
        resp = s3_client.get_bucket_tagging(Bucket=BUCKET)
        tags = {t["Key"]: t["Value"] for t in resp["TagSet"]}
        assert tags == {"project": "arca"}

        s3_client.delete_bucket_tagging(Bucket=BUCKET)


# ---------- Bucket config ----------


class TestBucketConfig:
    """Bucket config (lifecycle, encryption)."""

    def test_lifecycle_config(self, s3_client):
        rules = {
            "Rules": [
                {
                    "ID": "expire-old",
                    "Status": "Enabled",
                    "Filter": {"Prefix": "logs/"},
                    "Expiration": {"Days": 30},
                }
            ]
        }
        s3_client.put_bucket_lifecycle_configuration(
            Bucket=BUCKET, LifecycleConfiguration=rules
        )
        resp = s3_client.get_bucket_lifecycle_configuration(Bucket=BUCKET)
        assert len(resp["Rules"]) == 1
        assert resp["Rules"][0]["ID"] == "expire-old"

        s3_client.delete_bucket_lifecycle(Bucket=BUCKET)


# ---------- Admin API ----------


class TestAdminApi:
    """Admin API endpoints over PostgreSQL backend."""

    def test_health_endpoint(self, endpoint_url):
        """Health endpoint works without auth."""
        resp = requests.get(f"{endpoint_url}/admin/health", timeout=10)
        assert resp.status_code == 200
        body = resp.json()
        assert body["status"] == "ok"


# ---------- Copy object ----------


class TestCopyObject:
    """Copy object within PostgreSQL backend."""

    def test_copy_object(self, s3_client):
        s3_client.put_object(Bucket=BUCKET, Key="src.txt", Body=b"copy me")
        s3_client.copy_object(
            Bucket=BUCKET,
            Key="dst.txt",
            CopySource=f"{BUCKET}/src.txt",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="dst.txt")
        assert resp["Body"].read() == b"copy me"


# ---------- Range read ----------


class TestRangeRead:
    """Byte range reads."""

    def test_range_get(self, s3_client):
        body = b"0123456789"
        s3_client.put_object(Bucket=BUCKET, Key="range.txt", Body=body)
        resp = s3_client.get_object(Bucket=BUCKET, Key="range.txt", Range="bytes=3-6")
        assert resp["Body"].read() == b"3456"


# ---------- Concurrent writes (commit-ordered seq counter) ----------


class TestConcurrentWrites:
    """Concurrency smoke test for the write gate (pg/write_gate.rs).

    On a single node every object write takes an advisory lock on its key
    (decision H13); in a cluster, the commit-ordered object_seq counter
    (decision H3). This exercises many concurrent writers (puts, overwrites
    and deletes mixed, matching the put/delete lock paths) to catch deadlocks
    or lost writes in the gate. The cross-node manifest ordering itself is
    covered by the cluster suite; here we pin that the gate cannot wedge or
    drop rows under the connection pool's parallelism.
    """

    def test_parallel_puts_and_deletes_all_land(self, s3_client):
        from concurrent.futures import ThreadPoolExecutor

        keys = [f"concurrent/{i:03d}.bin" for i in range(40)]

        def put(key):
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=key.encode())
            return key

        # Phase 1: 40 parallel creates.
        with ThreadPoolExecutor(max_workers=16) as pool:
            assert sorted(pool.map(put, keys)) == keys

        # Phase 2: overwrite half and delete a quarter, in parallel (mixes the
        # insert and delete lock paths against the shared counter row).
        def overwrite(key):
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v2:" + key.encode())

        def delete(key):
            s3_client.delete_object(Bucket=BUCKET, Key=key)

        with ThreadPoolExecutor(max_workers=16) as pool:
            futures = [pool.submit(overwrite, k) for k in keys[:20]]
            futures += [pool.submit(delete, k) for k in keys[30:]]
            for f in futures:
                f.result()

        # Every surviving object is present with the right content; every
        # deleted one is gone.
        listed = set()
        paginator = s3_client.get_paginator("list_objects_v2")
        for page in paginator.paginate(Bucket=BUCKET, Prefix="concurrent/"):
            listed.update(o["Key"] for o in page.get("Contents", []))
        assert listed == set(keys[:30])
        for key in keys[:20]:
            body = s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read()
            assert body == b"v2:" + key.encode()


def _pg_query(sql, params=()):
    """Runs one statement against Arca's database and returns its rows."""
    conn = psycopg2.connect(PG_DSN)
    try:
        with conn, conn.cursor() as cur:
            cur.execute(sql, params)
            return cur.fetchall() if cur.description else []
    finally:
        conn.close()


def _local_seq():
    """The last value issued by the single-node seq sequence (0 if none)."""
    is_called, last_value = _pg_query(
        "SELECT is_called, last_value FROM object_seq_local"
    )[0]
    return last_value if is_called else 0


# ---------- Single-node write gate ----------


class TestSingleNodeWriteGate:
    """The single-node write gate (decision H13): an advisory lock per key and
    seq from the object_seq_local sequence, instead of the commit-ordered
    counter every write in the database used to wait for."""

    def test_writes_take_the_sequence_not_the_counter(self, s3_client):
        counter_before = _pg_query("SELECT value FROM object_seq")[0][0]
        seq_before = _local_seq()

        s3_client.put_object(Bucket=BUCKET, Key="gate/one.txt", Body=b"x")

        assert _pg_query("SELECT value FROM object_seq")[0][0] == counter_before
        row_seq = _pg_query(
            "SELECT seq FROM objects WHERE bucket = %s AND key = %s",
            (BUCKET, "gate/one.txt"),
        )[0][0]
        assert row_seq > seq_before
        assert _local_seq() >= row_seq

    def test_concurrent_writes_get_distinct_seqs(self, s3_client):
        from concurrent.futures import ThreadPoolExecutor

        keys = [f"gate/seq-{i:03d}" for i in range(64)]
        with ThreadPoolExecutor(max_workers=16) as pool:
            list(pool.map(lambda k: s3_client.put_object(Bucket=BUCKET, Key=k, Body=b"s"), keys))

        total, distinct = _pg_query("SELECT COUNT(seq), COUNT(DISTINCT seq) FROM objects")[0]
        assert total == distinct

    def test_overlapping_delete_objects_batches(self, s3_client):
        """Batches that share keys, in different orders, all complete: the
        locks are taken in one global order, so they cannot deadlock."""
        import random
        from concurrent.futures import ThreadPoolExecutor

        keys = [f"gate/batch-{i:03d}" for i in range(200)]
        with ThreadPoolExecutor(max_workers=16) as pool:
            list(pool.map(lambda k: s3_client.put_object(Bucket=BUCKET, Key=k, Body=b"b"), keys))

        def delete_all(seed):
            order = keys[:]
            random.Random(seed).shuffle(order)
            return s3_client.delete_objects(
                Bucket=BUCKET,
                Delete={"Objects": [{"Key": k} for k in order], "Quiet": True},
            )

        with ThreadPoolExecutor(max_workers=6) as pool:
            responses = list(pool.map(delete_all, range(6)))

        for resp in responses:
            assert resp.get("Errors", []) == []
        listed = s3_client.list_objects_v2(Bucket=BUCKET, Prefix="gate/batch-")
        assert listed.get("KeyCount", 0) == 0

    def test_delete_objects_failed_precondition_fails_only_its_entry(self, s3_client):
        for key in ("gate/keep.txt", "gate/drop-a.txt", "gate/drop-b.txt"):
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=key.encode())

        resp = s3_client.delete_objects(
            Bucket=BUCKET,
            Delete={
                "Objects": [
                    {"Key": "gate/drop-a.txt"},
                    {"Key": "gate/keep.txt", "ETag": '"00000000000000000000000000000000"'},
                    {"Key": "gate/drop-b.txt"},
                ],
            },
        )

        errors = resp.get("Errors", [])
        assert [(e["Key"], e["Code"]) for e in errors] == [("gate/keep.txt", "PreconditionFailed")]
        assert sorted(d["Key"] for d in resp.get("Deleted", [])) == [
            "gate/drop-a.txt",
            "gate/drop-b.txt",
        ]
        assert s3_client.get_object(Bucket=BUCKET, Key="gate/keep.txt")["Body"].read() == b"gate/keep.txt"
        for key in ("gate/drop-a.txt", "gate/drop-b.txt"):
            with pytest.raises(ClientError):
                s3_client.head_object(Bucket=BUCKET, Key=key)


# ---------- Seq reconciliation at startup ----------


@pytest.mark.skipif(
    SEQ_PHASE not in ("plant", "verify"),
    reason="runs around an Arca restart in `bin/test postgres` (sets ARCA_PG_SEQ_PHASE)",
)
class TestSeqReconciliation:
    """When the store opens, the counter and the sequence are raised to the
    highest seq in use, so whichever generator the next run uses never issues
    a seq that is already stored (a mode switch, a migrate-db copy)."""

    @pytest.mark.skipif(SEQ_PHASE != "plant", reason="plant phase only")
    def test_plant_a_row_above_both_generators(self, s3_client):
        try:
            s3_client.create_bucket(Bucket=SEQ_BUCKET)
        except ClientError:
            pass
        s3_client.put_object(Bucket=SEQ_BUCKET, Key=SEQ_KEY, Body=b"raised")
        # What a row stamped by the other mode's generator looks like.
        _pg_query(
            "UPDATE objects SET seq = %s WHERE bucket = %s AND key = %s",
            (RAISED_SEQ, SEQ_BUCKET, SEQ_KEY),
        )
        assert _pg_query("SELECT value FROM object_seq")[0][0] < RAISED_SEQ
        assert _local_seq() < RAISED_SEQ

    @pytest.mark.skipif(SEQ_PHASE != "verify", reason="verify phase only")
    def test_both_generators_were_raised(self, s3_client):
        try:
            assert _pg_query("SELECT value FROM object_seq")[0][0] >= RAISED_SEQ
            assert _local_seq() >= RAISED_SEQ

            s3_client.put_object(Bucket=SEQ_BUCKET, Key="after.txt", Body=b"after")
            new_seq = _pg_query(
                "SELECT seq FROM objects WHERE bucket = %s AND key = %s",
                (SEQ_BUCKET, "after.txt"),
            )[0][0]
            assert new_seq > RAISED_SEQ
        finally:
            for key in (SEQ_KEY, "after.txt"):
                s3_client.delete_object(Bucket=SEQ_BUCKET, Key=key)
            s3_client.delete_bucket(Bucket=SEQ_BUCKET)
