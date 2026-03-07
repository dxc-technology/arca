"""Integration tests for Phase 2 — S3 compatibility fixes.

Tests range requests (suffix, open-ended, 416), copy-to-self,
UploadPartCopy, conditional headers, encoding-type=url, and
delimiter+prefix pagination.
"""

import io
import time

import pytest
from botocore.exceptions import ClientError


BUCKET = "test-phase2-bucket"


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass
    yield
    try:
        response = s3_client.list_objects_v2(Bucket=BUCKET)
        for obj in response.get("Contents", []):
            s3_client.delete_object(Bucket=BUCKET, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass


class TestRangeRequests:
    """Tests for Range header variants and 416 responses."""

    def test_suffix_range(self, s3_client):
        """bytes=-N should return the last N bytes."""
        data = b"0123456789"
        s3_client.put_object(Bucket=BUCKET, Key="range", Body=data)

        resp = s3_client.get_object(Bucket=BUCKET, Key="range", Range="bytes=-4")
        assert resp["ResponseMetadata"]["HTTPStatusCode"] == 206
        body = resp["Body"].read()
        assert body == b"6789"

    def test_open_ended_range(self, s3_client):
        """bytes=N- should return from byte N to end."""
        data = b"0123456789"
        s3_client.put_object(Bucket=BUCKET, Key="range", Body=data)

        resp = s3_client.get_object(Bucket=BUCKET, Key="range", Range="bytes=7-")
        assert resp["ResponseMetadata"]["HTTPStatusCode"] == 206
        body = resp["Body"].read()
        assert body == b"789"

    def test_range_past_end_clamped(self, s3_client):
        """End byte past file size should be clamped to last byte."""
        data = b"0123456789"
        s3_client.put_object(Bucket=BUCKET, Key="range", Body=data)

        resp = s3_client.get_object(Bucket=BUCKET, Key="range", Range="bytes=5-999")
        assert resp["ResponseMetadata"]["HTTPStatusCode"] == 206
        body = resp["Body"].read()
        assert body == b"56789"

    def test_invalid_range_416(self, s3_client):
        """Start byte past file size should return 416."""
        data = b"0123456789"
        s3_client.put_object(Bucket=BUCKET, Key="range", Body=data)

        with pytest.raises(ClientError) as exc_info:
            s3_client.get_object(Bucket=BUCKET, Key="range", Range="bytes=100-200")
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 416


class TestCopyToSelf:
    """Tests for copy-to-self validation."""

    def test_copy_to_self_without_replace_fails(self, s3_client):
        """Copying object to itself without REPLACE directive should fail."""
        s3_client.put_object(Bucket=BUCKET, Key="self-copy", Body=b"data")

        with pytest.raises(ClientError) as exc_info:
            s3_client.copy_object(
                Bucket=BUCKET,
                Key="self-copy",
                CopySource=f"{BUCKET}/self-copy",
            )
        assert exc_info.value.response["Error"]["Code"] == "InvalidRequest"

    def test_copy_to_self_with_replace_succeeds(self, s3_client):
        """Copying object to itself with REPLACE directive should succeed."""
        s3_client.put_object(
            Bucket=BUCKET, Key="self-copy", Body=b"data",
            ContentType="text/plain",
        )

        s3_client.copy_object(
            Bucket=BUCKET,
            Key="self-copy",
            CopySource=f"{BUCKET}/self-copy",
            MetadataDirective="REPLACE",
            ContentType="application/json",
        )
        # Should succeed (no exception)


class TestUploadPartCopy:
    """Tests for UploadPartCopy operation."""

    def test_upload_part_copy_basic(self, s3_client):
        """UploadPartCopy should copy source object data into a part."""
        # Create source object.
        src_data = b"A" * (6 * 1024 * 1024)  # 6 MB
        s3_client.put_object(Bucket=BUCKET, Key="copy-src", Body=src_data)

        # Start multipart upload.
        mpu = s3_client.create_multipart_upload(Bucket=BUCKET, Key="copy-dest")
        upload_id = mpu["UploadId"]

        # Copy source as part 1.
        resp = s3_client.upload_part_copy(
            Bucket=BUCKET,
            Key="copy-dest",
            PartNumber=1,
            UploadId=upload_id,
            CopySource=f"{BUCKET}/copy-src",
        )
        etag = resp["CopyPartResult"]["ETag"]
        assert etag.startswith('"') and etag.endswith('"')

        # Complete.
        s3_client.complete_multipart_upload(
            Bucket=BUCKET,
            Key="copy-dest",
            UploadId=upload_id,
            MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": etag}]},
        )

        # Verify.
        resp = s3_client.get_object(Bucket=BUCKET, Key="copy-dest")
        assert resp["Body"].read() == src_data

    def test_upload_part_copy_with_range(self, s3_client):
        """UploadPartCopy with CopySourceRange should copy partial data."""
        src_data = b"A" * (6 * 1024 * 1024) + b"B" * (6 * 1024 * 1024)  # 12 MB
        s3_client.put_object(Bucket=BUCKET, Key="range-src", Body=src_data)

        mpu = s3_client.create_multipart_upload(Bucket=BUCKET, Key="range-dest")
        upload_id = mpu["UploadId"]

        # Copy first 6 MB as part 1.
        r1 = s3_client.upload_part_copy(
            Bucket=BUCKET,
            Key="range-dest",
            PartNumber=1,
            UploadId=upload_id,
            CopySource=f"{BUCKET}/range-src",
            CopySourceRange="bytes=0-6291455",  # first 6 MB
        )
        # Copy second 6 MB as part 2.
        r2 = s3_client.upload_part_copy(
            Bucket=BUCKET,
            Key="range-dest",
            PartNumber=2,
            UploadId=upload_id,
            CopySource=f"{BUCKET}/range-src",
            CopySourceRange="bytes=6291456-12582911",  # second 6 MB
        )

        s3_client.complete_multipart_upload(
            Bucket=BUCKET,
            Key="range-dest",
            UploadId=upload_id,
            MultipartUpload={
                "Parts": [
                    {"PartNumber": 1, "ETag": r1["CopyPartResult"]["ETag"]},
                    {"PartNumber": 2, "ETag": r2["CopyPartResult"]["ETag"]},
                ]
            },
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key="range-dest")
        assert resp["Body"].read() == src_data


class TestConditionalHeaders:
    """Tests for If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since."""

    def test_get_if_match_success(self, s3_client):
        """GET with matching If-Match should succeed."""
        s3_client.put_object(Bucket=BUCKET, Key="cond", Body=b"data")
        head = s3_client.head_object(Bucket=BUCKET, Key="cond")
        etag = head["ETag"]

        resp = s3_client.get_object(Bucket=BUCKET, Key="cond", IfMatch=etag)
        assert resp["ResponseMetadata"]["HTTPStatusCode"] == 200
        resp["Body"].read()

    def test_get_if_match_fails_412(self, s3_client):
        """GET with non-matching If-Match should return 412."""
        s3_client.put_object(Bucket=BUCKET, Key="cond", Body=b"data")

        with pytest.raises(ClientError) as exc_info:
            s3_client.get_object(Bucket=BUCKET, Key="cond", IfMatch='"wrong-etag"')
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 412

    def test_get_if_none_match_304(self, s3_client):
        """GET with matching If-None-Match should return 304."""
        s3_client.put_object(Bucket=BUCKET, Key="cond", Body=b"data")
        head = s3_client.head_object(Bucket=BUCKET, Key="cond")
        etag = head["ETag"]

        with pytest.raises(ClientError) as exc_info:
            s3_client.get_object(Bucket=BUCKET, Key="cond", IfNoneMatch=etag)
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 304

    def test_get_if_none_match_succeeds_different_etag(self, s3_client):
        """GET with non-matching If-None-Match should succeed."""
        s3_client.put_object(Bucket=BUCKET, Key="cond", Body=b"data")

        resp = s3_client.get_object(
            Bucket=BUCKET, Key="cond", IfNoneMatch='"wrong-etag"',
        )
        assert resp["ResponseMetadata"]["HTTPStatusCode"] == 200
        resp["Body"].read()

    def test_head_if_match_fails_412(self, s3_client):
        """HEAD with non-matching If-Match should return 412."""
        s3_client.put_object(Bucket=BUCKET, Key="cond", Body=b"data")

        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(Bucket=BUCKET, Key="cond", IfMatch='"wrong-etag"')
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 412

    def test_head_if_none_match_304(self, s3_client):
        """HEAD with matching If-None-Match should return 304."""
        s3_client.put_object(Bucket=BUCKET, Key="cond", Body=b"data")
        head = s3_client.head_object(Bucket=BUCKET, Key="cond")
        etag = head["ETag"]

        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(Bucket=BUCKET, Key="cond", IfNoneMatch=etag)
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 304

    def test_get_if_modified_since_304(self, s3_client):
        """GET with If-Modified-Since in the future should return 304."""
        s3_client.put_object(Bucket=BUCKET, Key="cond", Body=b"data")
        head = s3_client.head_object(Bucket=BUCKET, Key="cond")
        # Use a date far in the future.
        from datetime import datetime, timezone, timedelta
        future = datetime.now(timezone.utc) + timedelta(days=365)

        with pytest.raises(ClientError) as exc_info:
            s3_client.get_object(
                Bucket=BUCKET, Key="cond", IfModifiedSince=future,
            )
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 304

    def test_get_if_unmodified_since_412(self, s3_client):
        """GET with If-Unmodified-Since in the past should return 412."""
        s3_client.put_object(Bucket=BUCKET, Key="cond", Body=b"data")
        from datetime import datetime, timezone, timedelta
        past = datetime(2020, 1, 1, tzinfo=timezone.utc)

        with pytest.raises(ClientError) as exc_info:
            s3_client.get_object(
                Bucket=BUCKET, Key="cond", IfUnmodifiedSince=past,
            )
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 412

    def test_copy_if_match_fails_412(self, s3_client):
        """CopyObject with non-matching CopySourceIfMatch should return 412."""
        s3_client.put_object(Bucket=BUCKET, Key="copy-src", Body=b"data")

        with pytest.raises(ClientError) as exc_info:
            s3_client.copy_object(
                Bucket=BUCKET,
                Key="copy-dest",
                CopySource=f"{BUCKET}/copy-src",
                CopySourceIfMatch='"wrong-etag"',
            )
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 412


class TestEncodingType:
    """Tests for encoding-type=url in list operations."""

    def test_list_v2_encoding_type_url(self, s3_client):
        """ListObjectsV2 with encoding-type=url should URL-encode keys."""
        key_with_spaces = "folder/my file.txt"
        s3_client.put_object(Bucket=BUCKET, Key=key_with_spaces, Body=b"data")

        resp = s3_client.list_objects_v2(
            Bucket=BUCKET, EncodingType="url",
        )
        assert resp["EncodingType"] == "url"
        keys = [obj["Key"] for obj in resp.get("Contents", [])]
        # The key should be URL-encoded.
        assert "folder/my%20file.txt" in keys or "folder%2Fmy%20file.txt" in keys

    def test_list_v1_encoding_type_url(self, s3_client):
        """ListObjects V1 with encoding-type=url should URL-encode keys."""
        key_with_spaces = "my dir/file name.txt"
        s3_client.put_object(Bucket=BUCKET, Key=key_with_spaces, Body=b"data")

        resp = s3_client.list_objects(
            Bucket=BUCKET, EncodingType="url",
        )
        assert resp["EncodingType"] == "url"
        keys = [obj["Key"] for obj in resp.get("Contents", [])]
        # Should be encoded.
        assert any("my%20dir" in k for k in keys)


class TestDelimiterPrefixPagination:
    """Tests for delimiter+prefix pagination that collapses records into CommonPrefixes."""

    def test_delimiter_groups_with_max_keys(self, s3_client):
        """With max-keys=2 and delimiter, two groups should paginate correctly."""
        # Create objects in 3 "folders".
        for folder in ["a", "b", "c"]:
            for i in range(3):
                s3_client.put_object(
                    Bucket=BUCKET,
                    Key=f"{folder}/file{i}.txt",
                    Body=b"data",
                )

        # First page: max-keys=2 with delimiter.
        resp = s3_client.list_objects_v2(
            Bucket=BUCKET, Delimiter="/", MaxKeys=2,
        )
        assert resp["IsTruncated"] is True
        prefixes = [p["Prefix"] for p in resp.get("CommonPrefixes", [])]
        assert len(prefixes) == 2
        assert "a/" in prefixes
        assert "b/" in prefixes

        # Second page using continuation token.
        resp2 = s3_client.list_objects_v2(
            Bucket=BUCKET,
            Delimiter="/",
            MaxKeys=2,
            ContinuationToken=resp["NextContinuationToken"],
        )
        prefixes2 = [p["Prefix"] for p in resp2.get("CommonPrefixes", [])]
        assert "c/" in prefixes2

    def test_delimiter_groups_not_overcounted(self, s3_client):
        """Multiple records in one common prefix should count as one result item."""
        # Create 5 objects under "dir/" and 1 at root.
        for i in range(5):
            s3_client.put_object(
                Bucket=BUCKET, Key=f"dir/file{i}.txt", Body=b"data",
            )
        s3_client.put_object(Bucket=BUCKET, Key="root.txt", Body=b"data")

        resp = s3_client.list_objects_v2(
            Bucket=BUCKET, Delimiter="/", MaxKeys=2,
        )
        # "dir/" counts as 1 CommonPrefix, "root.txt" as 1 Content → total 2.
        total = len(resp.get("Contents", [])) + len(resp.get("CommonPrefixes", []))
        assert total == 2
        assert resp["IsTruncated"] is False
