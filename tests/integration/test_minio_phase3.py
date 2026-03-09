"""Integration tests for Phase 3 — metadata and copy directives via MinIO client.

Mirrors test_phase3.py but uses the minio_client fixture instead of boto3.
Tests user metadata (x-amz-meta-*), CopyObject metadata-directive, and
multipart metadata preservation.

Skipped test classes:
- TestSystemMetadata: MinIO put_object metadata param only handles
  x-amz-meta-* headers; system headers like Cache-Control require
  special API parameters that are not exposed.
- TestFetchOwner: MinIO list_objects does not expose owner information.
- TestListMultipartUploads: MinIO Python client does not expose a direct
  list_multipart_uploads API (the old list_incomplete_uploads was removed
  in recent versions).
"""

import io
import os

import pytest
from minio.commonconfig import CopySource
from minio.error import S3Error


BUCKET = "minio-test-phase3-bucket"

# 5 MB — MinIO client splits into multipart when data exceeds part_size
PART_SIZE = 5 * 1024 * 1024


def cleanup_bucket(minio_client, bucket_name):
    """Remove all objects from a bucket and then the bucket itself."""
    try:
        objects = minio_client.list_objects(bucket_name, recursive=True)
        for obj in objects:
            minio_client.remove_object(bucket_name, obj.object_name)
        minio_client.remove_bucket(bucket_name)
    except S3Error:
        pass


@pytest.fixture(autouse=True)
def setup_bucket(minio_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        minio_client.make_bucket(BUCKET)
    except S3Error:
        pass
    yield
    cleanup_bucket(minio_client, BUCKET)


class TestUserMetadata:
    """Tests for x-amz-meta-* custom headers via MinIO client."""

    def test_put_get_user_metadata(self, minio_client):
        """User metadata set on put_object should be returned by stat_object."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "meta.txt", io.BytesIO(data), len(data),
            metadata={"color": "blue"},
        )

        stat = minio_client.stat_object(BUCKET, "meta.txt")
        meta = stat.metadata
        # MinIO client may return keys with or without the x-amz-meta- prefix.
        color = meta.get("x-amz-meta-color") or meta.get("color")
        assert color == "blue", (
            f"Expected metadata 'color'='blue', got metadata: {dict(meta)}"
        )

    def test_head_returns_user_metadata(self, minio_client):
        """stat_object should return user metadata."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "meta2.txt", io.BytesIO(data), len(data),
            metadata={"author": "pietro"},
        )

        stat = minio_client.stat_object(BUCKET, "meta2.txt")
        meta = stat.metadata
        author = meta.get("x-amz-meta-author") or meta.get("author")
        assert author == "pietro", (
            f"Expected metadata 'author'='pietro', got metadata: {dict(meta)}"
        )

    def test_overwrite_replaces_metadata(self, minio_client):
        """Overwriting an object should replace its metadata."""
        data1 = b"v1"
        minio_client.put_object(
            BUCKET, "ow.txt", io.BytesIO(data1), len(data1),
            metadata={"version": "1"},
        )
        data2 = b"v2"
        minio_client.put_object(
            BUCKET, "ow.txt", io.BytesIO(data2), len(data2),
            metadata={"version": "2"},
        )

        stat = minio_client.stat_object(BUCKET, "ow.txt")
        meta = stat.metadata
        version = meta.get("x-amz-meta-version") or meta.get("version")
        assert version == "2", (
            f"Expected metadata 'version'='2', got metadata: {dict(meta)}"
        )

    def test_no_metadata_returns_empty(self, minio_client):
        """Object without metadata should have no x-amz-meta-* entries."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "plain.txt", io.BytesIO(data), len(data),
        )

        stat = minio_client.stat_object(BUCKET, "plain.txt")
        meta = stat.metadata
        # Filter only x-amz-meta-* keys to check for user metadata.
        user_meta = {
            k: v for k, v in meta.items() if k.startswith("x-amz-meta-")
        }
        assert user_meta == {}, (
            f"Expected no user metadata, got: {user_meta}"
        )


class TestSystemMetadata:
    """Skipped — MinIO Python client put_object metadata parameter only
    handles x-amz-meta-* headers. System headers like Cache-Control,
    Content-Disposition, Content-Encoding, and Content-Language require
    dedicated API parameters that are not available on put_object.
    These are covered by the boto3 tests in test_phase3.py."""

    @pytest.mark.skip(
        reason="MinIO Python client put_object does not expose system metadata parameters"
    )
    def test_cache_control_roundtrip(self, minio_client):
        pass


class TestCopyObjectMetadataDirective:
    """Tests for x-amz-metadata-directive on CopyObject via MinIO client."""

    def test_copy_preserves_metadata_by_default(self, minio_client):
        """COPY (default) should preserve source metadata."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "src.txt", io.BytesIO(data), len(data),
            metadata={"origin": "source"},
        )

        minio_client.copy_object(
            BUCKET, "dst.txt", CopySource(BUCKET, "src.txt"),
        )

        stat = minio_client.stat_object(BUCKET, "dst.txt")
        meta = stat.metadata
        origin = meta.get("x-amz-meta-origin") or meta.get("origin")
        assert origin == "source", (
            f"Expected metadata 'origin'='source', got metadata: {dict(meta)}"
        )

    def test_copy_with_replace_uses_new_metadata(self, minio_client):
        """REPLACE directive should use the request's metadata, not source."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "src.txt", io.BytesIO(data), len(data),
            metadata={"origin": "source"},
        )

        minio_client.copy_object(
            BUCKET,
            "dst.txt",
            CopySource(BUCKET, "src.txt"),
            metadata={"origin": "replaced"},
            metadata_directive="REPLACE",
        )

        stat = minio_client.stat_object(BUCKET, "dst.txt")
        meta = stat.metadata
        origin = meta.get("x-amz-meta-origin") or meta.get("origin")
        assert origin == "replaced", (
            f"Expected metadata 'origin'='replaced', got metadata: {dict(meta)}"
        )

    def test_copy_replace_clears_metadata(self, minio_client):
        """REPLACE with empty metadata should clear source metadata."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "src.txt", io.BytesIO(data), len(data),
            metadata={"keep": "no"},
        )

        minio_client.copy_object(
            BUCKET,
            "dst.txt",
            CopySource(BUCKET, "src.txt"),
            metadata={},
            metadata_directive="REPLACE",
        )

        stat = minio_client.stat_object(BUCKET, "dst.txt")
        meta = stat.metadata
        user_meta = {
            k: v for k, v in meta.items() if k.startswith("x-amz-meta-")
        }
        assert user_meta == {}, (
            f"Expected no user metadata after REPLACE with empty, got: {user_meta}"
        )


class TestFetchOwner:
    """Skipped — MinIO Python client list_objects does not expose owner
    information. This is covered by the boto3 tests in test_phase3.py."""

    @pytest.mark.skip(
        reason="MinIO Python client list_objects does not expose owner info"
    )
    def test_list_v2_with_fetch_owner(self, minio_client):
        pass


class TestListMultipartUploads:
    """Skipped — MinIO Python client does not expose a direct
    list_multipart_uploads API. The old list_incomplete_uploads method was
    removed in recent versions. This is covered by the boto3 tests in
    test_phase3.py."""

    @pytest.mark.skip(
        reason="MinIO Python client does not expose list_multipart_uploads"
    )
    def test_list_active_upload(self, minio_client):
        pass


class TestMultipartMetadata:
    """Tests for metadata on multipart uploads via MinIO client."""

    def test_multipart_preserves_metadata(self, minio_client):
        """Metadata set on a large (multipart) put_object should persist."""
        data = os.urandom(11 * 1024 * 1024)
        key = "multi-meta.bin"

        minio_client.put_object(
            BUCKET, key,
            data=io.BytesIO(data),
            length=len(data),
            metadata={"project": "arca", "env": "test"},
            part_size=PART_SIZE,
        )

        stat = minio_client.stat_object(BUCKET, key)
        meta = stat.metadata
        project = meta.get("x-amz-meta-project") or meta.get("project")
        env = meta.get("x-amz-meta-env") or meta.get("env")
        assert project == "arca", (
            f"Expected metadata 'project'='arca', got metadata: {dict(meta)}"
        )
        assert env == "test", (
            f"Expected metadata 'env'='test', got metadata: {dict(meta)}"
        )
