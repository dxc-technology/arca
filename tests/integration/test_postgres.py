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
