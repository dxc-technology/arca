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
        response = s3_client.list_objects_v2(Bucket=BUCKET)
        for obj in response.get("Contents", []):
            s3_client.delete_object(Bucket=BUCKET, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass  # Bucket may already be deleted


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


COPY_BUCKET_2 = "test-objects-copy-bucket-2"


class TestCopyObject:
    def test_copy_within_bucket(self, s3_client):
        """Copy an object within the same bucket."""
        data = b"copy me"
        s3_client.put_object(Bucket=BUCKET, Key="original", Body=data)

        s3_client.copy_object(
            Bucket=BUCKET, Key="copy",
            CopySource=f"{BUCKET}/original",
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key="copy")
        assert resp["Body"].read() == data

    def test_copy_preserves_content_type(self, s3_client):
        """CopyObject should preserve the source object's content type."""
        s3_client.put_object(
            Bucket=BUCKET, Key="typed", Body=b"<html></html>",
            ContentType="text/html",
        )

        s3_client.copy_object(
            Bucket=BUCKET, Key="typed-copy",
            CopySource=f"{BUCKET}/typed",
        )

        resp = s3_client.head_object(Bucket=BUCKET, Key="typed-copy")
        assert resp["ContentType"] == "text/html"

    def test_copy_returns_etag(self, s3_client):
        """CopyObject response should include a valid ETag."""
        s3_client.put_object(Bucket=BUCKET, Key="for-etag", Body=b"hello")

        resp = s3_client.copy_object(
            Bucket=BUCKET, Key="etag-copy",
            CopySource=f"{BUCKET}/for-etag",
        )

        etag = resp["CopyObjectResult"]["ETag"]
        assert etag.startswith('"') and etag.endswith('"')
        hex_part = etag.strip('"')
        assert len(hex_part) == 32

    def test_copy_nonexistent_source(self, s3_client):
        """Copying from a nonexistent source key should return NoSuchKey."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.copy_object(
                Bucket=BUCKET, Key="copy-dest",
                CopySource=f"{BUCKET}/nonexistent",
            )
        assert exc_info.value.response["Error"]["Code"] == "NoSuchKey"

    def test_copy_cross_bucket(self, s3_client):
        """Copy an object to a different bucket."""
        try:
            s3_client.create_bucket(Bucket=COPY_BUCKET_2)
        except ClientError:
            pass

        data = b"cross-bucket data"
        s3_client.put_object(Bucket=BUCKET, Key="cross-src", Body=data)

        s3_client.copy_object(
            Bucket=COPY_BUCKET_2, Key="cross-dest",
            CopySource=f"{BUCKET}/cross-src",
        )

        resp = s3_client.get_object(Bucket=COPY_BUCKET_2, Key="cross-dest")
        assert resp["Body"].read() == data

        # Cleanup
        s3_client.delete_object(Bucket=COPY_BUCKET_2, Key="cross-dest")
        s3_client.delete_bucket(Bucket=COPY_BUCKET_2)

    def test_copy_overwrite(self, s3_client):
        """Copying over an existing key should overwrite it."""
        s3_client.put_object(Bucket=BUCKET, Key="src", Body=b"new data")
        s3_client.put_object(Bucket=BUCKET, Key="dest", Body=b"old data")

        s3_client.copy_object(
            Bucket=BUCKET, Key="dest",
            CopySource=f"{BUCKET}/src",
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key="dest")
        assert resp["Body"].read() == b"new data"
