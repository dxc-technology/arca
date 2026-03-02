"""Integration tests for Arca Phase 3 — Object operations.

Tests PutObject, GetObject, HeadObject, and DeleteObject using boto3.
"""

import io

import pytest
from botocore.exceptions import ClientError


BUCKET = "test-objects-bucket"


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass  # Bucket may already exist
    yield
    # Cleanup: delete all objects then the bucket
    try:
        # List and delete objects (simple, no pagination needed for tests)
        # Since ListObjectsV2 is not implemented yet, we can't list.
        # Tests are responsible for cleaning up their own objects.
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass  # Bucket may have objects or already be deleted


class TestPutGetRoundtrip:
    def test_put_and_get_object(self, s3_client):
        """Put an object and get it back."""
        data = b"hello world"
        s3_client.put_object(Bucket=BUCKET, Key="test.txt", Body=data)

        resp = s3_client.get_object(Bucket=BUCKET, Key="test.txt")
        body = resp["Body"].read()
        assert body == data

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key="test.txt")

    def test_content_type_preserved(self, s3_client):
        """Content-Type should be preserved on round-trip."""
        s3_client.put_object(
            Bucket=BUCKET, Key="page.html", Body=b"<html></html>",
            ContentType="text/html",
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key="page.html")
        assert resp["ContentType"] == "text/html"
        resp["Body"].read()

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key="page.html")

    def test_default_content_type(self, s3_client):
        """Without explicit Content-Type, should default to application/octet-stream."""
        s3_client.put_object(Bucket=BUCKET, Key="blob", Body=b"\x00\x01\x02")

        resp = s3_client.get_object(Bucket=BUCKET, Key="blob")
        assert resp["ContentType"] == "application/octet-stream"
        resp["Body"].read()

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key="blob")


class TestETag:
    def test_etag_format(self, s3_client):
        """ETag should be a quoted hex MD5 hash."""
        s3_client.put_object(Bucket=BUCKET, Key="etag-test", Body=b"hello world")

        resp = s3_client.head_object(Bucket=BUCKET, Key="etag-test")
        etag = resp["ETag"]
        # ETag should be quoted hex string like "5eb63bbbe01eeed093cb22bb8f5acdc3"
        assert etag.startswith('"') and etag.endswith('"')
        hex_part = etag.strip('"')
        assert len(hex_part) == 32  # MD5 produces 32 hex chars
        int(hex_part, 16)  # Should be valid hex

        # Verify known MD5 of "hello world"
        assert hex_part == "5eb63bbbe01eeed093cb22bb8f5acdc3"

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key="etag-test")


class TestHeadObject:
    def test_head_returns_metadata(self, s3_client):
        """HEAD should return size, ETag, Content-Type, Last-Modified."""
        data = b"test content here"
        s3_client.put_object(
            Bucket=BUCKET, Key="head-test", Body=data,
            ContentType="text/plain",
        )

        resp = s3_client.head_object(Bucket=BUCKET, Key="head-test")
        assert resp["ContentLength"] == len(data)
        assert resp["ContentType"] == "text/plain"
        assert "ETag" in resp
        assert "LastModified" in resp

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key="head-test")


class TestDeleteObject:
    def test_delete_existing(self, s3_client):
        """Delete an existing object should return 204."""
        s3_client.put_object(Bucket=BUCKET, Key="to-delete", Body=b"data")
        s3_client.delete_object(Bucket=BUCKET, Key="to-delete")

        # Verify it's gone
        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(Bucket=BUCKET, Key="to-delete")
        assert exc_info.value.response["Error"]["Code"] == "404"

    def test_delete_idempotent(self, s3_client):
        """Deleting a nonexistent object should succeed (S3 behavior)."""
        # Should not raise
        s3_client.delete_object(Bucket=BUCKET, Key="nonexistent-key")


class TestOverwrite:
    def test_overwrite_same_key(self, s3_client):
        """Putting to the same key should overwrite the previous object."""
        s3_client.put_object(Bucket=BUCKET, Key="overwrite", Body=b"version1")
        s3_client.put_object(Bucket=BUCKET, Key="overwrite", Body=b"version2")

        resp = s3_client.get_object(Bucket=BUCKET, Key="overwrite")
        body = resp["Body"].read()
        assert body == b"version2"

        # Size should reflect the new content
        resp = s3_client.head_object(Bucket=BUCKET, Key="overwrite")
        assert resp["ContentLength"] == len(b"version2")

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key="overwrite")


class TestErrorCases:
    def test_get_nonexistent_object(self, s3_client):
        """GET on a nonexistent key should return NoSuchKey."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.get_object(Bucket=BUCKET, Key="no-such-key")
        assert exc_info.value.response["Error"]["Code"] == "NoSuchKey"

    def test_put_to_nonexistent_bucket(self, s3_client):
        """PUT to a nonexistent bucket should return NoSuchBucket."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.put_object(
                Bucket="no-such-bucket-for-put", Key="key", Body=b"data",
            )
        assert exc_info.value.response["Error"]["Code"] == "NoSuchBucket"

    def test_delete_nonempty_bucket(self, s3_client):
        """DELETE on a bucket with objects should return BucketNotEmpty."""
        s3_client.put_object(Bucket=BUCKET, Key="blocker", Body=b"data")

        with pytest.raises(ClientError) as exc_info:
            s3_client.delete_bucket(Bucket=BUCKET)
        assert exc_info.value.response["Error"]["Code"] == "BucketNotEmpty"

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key="blocker")


class TestRangeRequest:
    def test_range_request(self, s3_client):
        """GET with Range header should return partial content."""
        data = b"0123456789"
        s3_client.put_object(Bucket=BUCKET, Key="range-test", Body=data)

        resp = s3_client.get_object(
            Bucket=BUCKET, Key="range-test", Range="bytes=3-6",
        )
        assert resp["ResponseMetadata"]["HTTPStatusCode"] == 206
        body = resp["Body"].read()
        assert body == b"3456"
        assert resp["ContentLength"] == 4

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key="range-test")
