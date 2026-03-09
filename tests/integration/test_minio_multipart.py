"""Integration tests for multipart upload operations using the MinIO Python client.

Mirrors test_multipart.py but uses the minio_client fixture. The MinIO Python
client does not expose low-level multipart APIs (create/upload_part/complete)
publicly, so we test multipart behavior through the high-level put_object()
with data larger than the part_size threshold, which triggers multipart
internally. The s3_client fixture is used alongside for setup/cleanup.
"""

import io
import os
import re

import pytest
from minio.error import S3Error


BUCKET = "minio-test-multipart-bucket"

# 5 MB — MinIO client splits into multipart when data exceeds part_size
PART_SIZE = 5 * 1024 * 1024


@pytest.fixture(autouse=True)
def setup_bucket(minio_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    if not minio_client.bucket_exists(BUCKET):
        minio_client.make_bucket(BUCKET)
    yield
    # Cleanup: delete all objects then the bucket
    try:
        for obj in minio_client.list_objects(BUCKET, recursive=True):
            minio_client.remove_object(BUCKET, obj.object_name)
        minio_client.remove_bucket(BUCKET)
    except S3Error:
        pass


class TestMultipartViaLargeUpload:
    def test_large_upload_roundtrip(self, minio_client):
        """put_object with 11MB should trigger multipart and round-trip correctly."""
        data = os.urandom(11 * 1024 * 1024)
        key = "mp-large-roundtrip"

        minio_client.put_object(
            BUCKET, key,
            data=io.BytesIO(data),
            length=len(data),
            part_size=PART_SIZE,
        )

        response = minio_client.get_object(BUCKET, key)
        try:
            body = response.read()
        finally:
            response.close()
            response.release_conn()

        assert body == data

    def test_large_upload_etag_is_composite(self, minio_client):
        """put_object with 11MB should produce a composite ETag containing '-'."""
        data = os.urandom(11 * 1024 * 1024)
        key = "mp-large-etag"

        minio_client.put_object(
            BUCKET, key,
            data=io.BytesIO(data),
            length=len(data),
            part_size=PART_SIZE,
        )

        stat = minio_client.stat_object(BUCKET, key)
        etag = stat.etag
        # Composite multipart ETags have the format "hex-N"
        assert "-" in etag, f"Expected composite ETag with '-', got: {etag}"
        assert re.match(r'^[0-9a-f]+-\d+$', etag), (
            f"Unexpected ETag format: {etag}"
        )

    def test_large_upload_preserves_content_type(self, minio_client):
        """put_object with content_type should preserve it through multipart."""
        data = os.urandom(11 * 1024 * 1024)
        key = "mp-large-ctype"

        minio_client.put_object(
            BUCKET, key,
            data=io.BytesIO(data),
            length=len(data),
            content_type="application/octet-stream",
            part_size=PART_SIZE,
        )

        stat = minio_client.stat_object(BUCKET, key)
        assert stat.content_type == "application/octet-stream"

    def test_large_upload_preserves_metadata(self, minio_client):
        """put_object with user metadata should preserve it through multipart."""
        data = os.urandom(11 * 1024 * 1024)
        key = "mp-large-meta"

        minio_client.put_object(
            BUCKET, key,
            data=io.BytesIO(data),
            length=len(data),
            metadata={"project": "arca"},
            part_size=PART_SIZE,
        )

        stat = minio_client.stat_object(BUCKET, key)
        # MinIO client returns metadata with x-amz-meta- prefix stripped
        meta = stat.metadata
        # The key may appear as "x-amz-meta-project" or "project" depending
        # on the client version; check for either.
        project_value = meta.get("x-amz-meta-project") or meta.get("project")
        assert project_value == "arca", (
            f"Expected metadata 'project'='arca', got metadata: {dict(meta)}"
        )


class TestMultipartOverwrite:
    def test_overwrite_with_large_upload(self, minio_client):
        """A large multipart upload should overwrite an existing small object."""
        key = "mp-overwrite"

        # Put a small object first
        small_data = b"old small data"
        minio_client.put_object(
            BUCKET, key,
            data=io.BytesIO(small_data),
            length=len(small_data),
        )

        # Verify small object is there
        response = minio_client.get_object(BUCKET, key)
        try:
            assert response.read() == small_data
        finally:
            response.close()
            response.release_conn()

        # Overwrite with a large multipart upload
        large_data = os.urandom(11 * 1024 * 1024)
        minio_client.put_object(
            BUCKET, key,
            data=io.BytesIO(large_data),
            length=len(large_data),
            part_size=PART_SIZE,
        )

        # Verify the new content
        response = minio_client.get_object(BUCKET, key)
        try:
            body = response.read()
        finally:
            response.close()
            response.release_conn()

        assert body == large_data

        # Confirm the ETag is now composite (multipart)
        stat = minio_client.stat_object(BUCKET, key)
        assert "-" in stat.etag, (
            f"Expected composite ETag after large overwrite, got: {stat.etag}"
        )
