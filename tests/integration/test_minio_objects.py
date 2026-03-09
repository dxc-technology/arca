"""Integration tests for object operations using the MinIO Python client.

Mirrors test_objects.py but uses the minio_client fixture instead of boto3.
Uses s3_client (boto3) for setup/cleanup where the MinIO client API differs.
"""

import io

import pytest
from minio.commonconfig import CopySource
from minio.error import S3Error


BUCKET = "minio-test-objects-bucket"
COPY_BUCKET_2 = "minio-test-objects-copy-bucket-2"


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
        pass  # Bucket may already exist
    yield
    cleanup_bucket(minio_client, BUCKET)


class TestPutGetRoundtrip:
    def test_put_and_get_object(self, minio_client):
        """Put an object and get it back."""
        data = b"hello world"
        minio_client.put_object(
            BUCKET, "test.txt", io.BytesIO(data), len(data),
        )

        resp = minio_client.get_object(BUCKET, "test.txt")
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == data

    def test_content_type_preserved(self, minio_client):
        """Content-Type should be preserved on round-trip."""
        data = b"<html></html>"
        minio_client.put_object(
            BUCKET, "page.html", io.BytesIO(data), len(data),
            content_type="text/html",
        )

        stat = minio_client.stat_object(BUCKET, "page.html")
        assert stat.content_type == "text/html"

    def test_default_content_type(self, minio_client):
        """Without explicit Content-Type, should default to application/octet-stream."""
        data = b"\x00\x01\x02"
        minio_client.put_object(
            BUCKET, "blob", io.BytesIO(data), len(data),
        )

        stat = minio_client.stat_object(BUCKET, "blob")
        assert stat.content_type == "application/octet-stream"


class TestETag:
    def test_etag_format(self, minio_client):
        """ETag should be a 32-char hex MD5 hash."""
        data = b"hello world"
        minio_client.put_object(
            BUCKET, "etag-test", io.BytesIO(data), len(data),
        )

        stat = minio_client.stat_object(BUCKET, "etag-test")
        etag = stat.etag
        # MinIO client returns the ETag without surrounding quotes
        assert len(etag) == 32  # MD5 produces 32 hex chars
        int(etag, 16)  # Should be valid hex

        # Verify known MD5 of "hello world"
        assert etag == "5eb63bbbe01eeed093cb22bb8f5acdc3"


class TestHeadObject:
    def test_head_returns_metadata(self, minio_client):
        """stat_object should return size, ETag, Content-Type, Last-Modified."""
        data = b"test content here"
        minio_client.put_object(
            BUCKET, "head-test", io.BytesIO(data), len(data),
            content_type="text/plain",
        )

        stat = minio_client.stat_object(BUCKET, "head-test")
        assert stat.size == len(data)
        assert stat.content_type == "text/plain"
        assert stat.etag is not None
        assert stat.last_modified is not None


class TestDeleteObject:
    def test_delete_existing(self, minio_client):
        """Delete an existing object, then verify it's gone."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "to-delete", io.BytesIO(data), len(data),
        )
        minio_client.remove_object(BUCKET, "to-delete")

        # Verify it's gone
        with pytest.raises(S3Error) as exc_info:
            minio_client.stat_object(BUCKET, "to-delete")
        assert exc_info.value.code == "NoSuchKey"

    def test_delete_idempotent(self, minio_client):
        """Deleting a nonexistent object should succeed (S3 behavior)."""
        # Should not raise
        minio_client.remove_object(BUCKET, "nonexistent-key")


class TestOverwrite:
    def test_overwrite_same_key(self, minio_client):
        """Putting to the same key should overwrite the previous object."""
        data1 = b"version1"
        data2 = b"version2"
        minio_client.put_object(
            BUCKET, "overwrite", io.BytesIO(data1), len(data1),
        )
        minio_client.put_object(
            BUCKET, "overwrite", io.BytesIO(data2), len(data2),
        )

        resp = minio_client.get_object(BUCKET, "overwrite")
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == b"version2"

        # Size should reflect the new content
        stat = minio_client.stat_object(BUCKET, "overwrite")
        assert stat.size == len(b"version2")


class TestErrorCases:
    def test_get_nonexistent_object(self, minio_client):
        """GET on a nonexistent key should return NoSuchKey."""
        with pytest.raises(S3Error) as exc_info:
            minio_client.get_object(BUCKET, "no-such-key")
        assert exc_info.value.code == "NoSuchKey"

    def test_put_to_nonexistent_bucket(self, minio_client):
        """PUT to a nonexistent bucket should return NoSuchBucket."""
        data = b"data"
        with pytest.raises(S3Error) as exc_info:
            minio_client.put_object(
                "no-such-bucket-for-put", "key", io.BytesIO(data), len(data),
            )
        assert exc_info.value.code == "NoSuchBucket"

    def test_delete_nonempty_bucket(self, minio_client):
        """DELETE on a bucket with objects should return BucketNotEmpty."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "blocker", io.BytesIO(data), len(data),
        )

        with pytest.raises(S3Error) as exc_info:
            minio_client.remove_bucket(BUCKET)
        assert exc_info.value.code == "BucketNotEmpty"


class TestRangeRequest:
    def test_range_request(self, minio_client):
        """GET with offset and length should return partial content."""
        data = b"0123456789"
        minio_client.put_object(
            BUCKET, "range-test", io.BytesIO(data), len(data),
        )

        resp = minio_client.get_object(BUCKET, "range-test", offset=3, length=4)
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == b"3456"


class TestCopyObject:
    def test_copy_within_bucket(self, minio_client):
        """Copy an object within the same bucket."""
        data = b"copy me"
        minio_client.put_object(
            BUCKET, "original", io.BytesIO(data), len(data),
        )

        minio_client.copy_object(
            BUCKET, "copy", CopySource(BUCKET, "original"),
        )

        resp = minio_client.get_object(BUCKET, "copy")
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == data

    def test_copy_preserves_content_type(self, minio_client):
        """CopyObject should preserve the source object's content type."""
        data = b"<html></html>"
        minio_client.put_object(
            BUCKET, "typed", io.BytesIO(data), len(data),
            content_type="text/html",
        )

        minio_client.copy_object(
            BUCKET, "typed-copy", CopySource(BUCKET, "typed"),
        )

        stat = minio_client.stat_object(BUCKET, "typed-copy")
        assert stat.content_type == "text/html"

    def test_copy_returns_etag(self, minio_client):
        """CopyObject response should include a valid ETag."""
        data = b"hello"
        minio_client.put_object(
            BUCKET, "for-etag", io.BytesIO(data), len(data),
        )

        result = minio_client.copy_object(
            BUCKET, "etag-copy", CopySource(BUCKET, "for-etag"),
        )

        # copy_object returns an ObjectWriteResult with etag
        etag = result.etag
        assert len(etag) == 32
        int(etag, 16)  # Should be valid hex

    def test_copy_nonexistent_source(self, minio_client):
        """Copying from a nonexistent source key should return NoSuchKey."""
        with pytest.raises(S3Error) as exc_info:
            minio_client.copy_object(
                BUCKET, "copy-dest", CopySource(BUCKET, "nonexistent"),
            )
        assert exc_info.value.code == "NoSuchKey"

    def test_copy_cross_bucket(self, minio_client):
        """Copy an object to a different bucket."""
        try:
            minio_client.make_bucket(COPY_BUCKET_2)
        except S3Error:
            pass

        data = b"cross-bucket data"
        minio_client.put_object(
            BUCKET, "cross-src", io.BytesIO(data), len(data),
        )

        minio_client.copy_object(
            COPY_BUCKET_2, "cross-dest", CopySource(BUCKET, "cross-src"),
        )

        resp = minio_client.get_object(COPY_BUCKET_2, "cross-dest")
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == data

        # Cleanup
        cleanup_bucket(minio_client, COPY_BUCKET_2)

    def test_copy_overwrite(self, minio_client):
        """Copying over an existing key should overwrite it."""
        src_data = b"new data"
        dest_data = b"old data"
        minio_client.put_object(
            BUCKET, "src", io.BytesIO(src_data), len(src_data),
        )
        minio_client.put_object(
            BUCKET, "dest", io.BytesIO(dest_data), len(dest_data),
        )

        minio_client.copy_object(
            BUCKET, "dest", CopySource(BUCKET, "src"),
        )

        resp = minio_client.get_object(BUCKET, "dest")
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == b"new data"
