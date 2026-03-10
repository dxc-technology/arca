"""Integration tests for Arca Phase 5 — Multipart upload operations.

Tests CreateMultipartUpload, UploadPart, CompleteMultipartUpload, and
AbortMultipartUpload using boto3.
"""

import hashlib
import io
import os
import re

import pytest
from botocore.exceptions import ClientError


BUCKET = "test-multipart-bucket"

# 5 MB (minimum part size for non-last parts)
PART_SIZE = 5 * 1024 * 1024


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
        pass


class TestCreateMultipartUpload:
    def test_returns_upload_id(self, s3_client):
        """CreateMultipartUpload should return a valid upload ID."""
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key="mp-test")
        upload_id = resp["UploadId"]
        assert upload_id
        assert len(upload_id) > 0

        # Cleanup
        s3_client.abort_multipart_upload(
            Bucket=BUCKET, Key="mp-test", UploadId=upload_id,
        )


class TestUploadPart:
    def test_upload_part_returns_etag(self, s3_client):
        """UploadPart should return an ETag header."""
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key="mp-etag")
        upload_id = resp["UploadId"]

        part_data = os.urandom(PART_SIZE)
        resp = s3_client.upload_part(
            Bucket=BUCKET, Key="mp-etag", UploadId=upload_id,
            PartNumber=1, Body=part_data,
        )

        etag = resp["ETag"]
        assert etag.startswith('"') and etag.endswith('"')
        hex_part = etag.strip('"')
        assert len(hex_part) == 32  # MD5 hex

        # Cleanup
        s3_client.abort_multipart_upload(
            Bucket=BUCKET, Key="mp-etag", UploadId=upload_id,
        )


class TestCompleteMultipartUpload:
    def test_two_parts_assembles_correct_data(self, s3_client):
        """Complete with 2 parts should assemble the correct data."""
        key = "mp-complete"
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        part1_data = os.urandom(PART_SIZE)
        part2_data = os.urandom(PART_SIZE)

        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=part1_data,
        )["ETag"]
        etag2 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=2, Body=part2_data,
        )["ETag"]

        s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            MultipartUpload={
                "Parts": [
                    {"PartNumber": 1, "ETag": etag1},
                    {"PartNumber": 2, "ETag": etag2},
                ],
            },
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key=key)
        body = resp["Body"].read()
        assert body == part1_data + part2_data

    def test_composite_etag_format(self, s3_client):
        """Completed multipart ETag should be in format "hex-N"."""
        key = "mp-etag-format"
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        part1_data = os.urandom(PART_SIZE)
        part2_data = os.urandom(1024)  # Last part can be small

        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=part1_data,
        )["ETag"]
        etag2 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=2, Body=part2_data,
        )["ETag"]

        resp = s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            MultipartUpload={
                "Parts": [
                    {"PartNumber": 1, "ETag": etag1},
                    {"PartNumber": 2, "ETag": etag2},
                ],
            },
        )

        etag = resp["ETag"]
        # Should be like '"hexhexhex-2"'
        assert re.match(r'^"[0-9a-f]+-2"$', etag), f"Unexpected ETag format: {etag}"

    def test_composite_etag_value(self, s3_client):
        """Verify composite ETag matches expected MD5-of-MD5s computation."""
        key = "mp-etag-value"
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        part1_data = os.urandom(PART_SIZE)
        part2_data = os.urandom(PART_SIZE)

        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=part1_data,
        )["ETag"]
        etag2 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=2, Body=part2_data,
        )["ETag"]

        resp = s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            MultipartUpload={
                "Parts": [
                    {"PartNumber": 1, "ETag": etag1},
                    {"PartNumber": 2, "ETag": etag2},
                ],
            },
        )

        # Compute expected composite ETag
        md5_1 = bytes.fromhex(etag1.strip('"'))
        md5_2 = bytes.fromhex(etag2.strip('"'))
        expected = hashlib.md5(md5_1 + md5_2).hexdigest() + "-2"

        actual = resp["ETag"].strip('"')
        assert actual == expected

    def test_complete_nonexistent_upload(self, s3_client):
        """CompleteMultipartUpload with nonexistent upload ID should fail."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.complete_multipart_upload(
                Bucket=BUCKET, Key="no-such", UploadId="nonexistent-id",
                MultipartUpload={
                    "Parts": [{"PartNumber": 1, "ETag": '"abc"'}],
                },
            )
        assert exc_info.value.response["Error"]["Code"] == "NoSuchUpload"

    def test_complete_wrong_part_order_succeeds(self, s3_client):
        """CompleteMultipartUpload with wrong part order succeeds (server sorts)."""
        key = "mp-bad-order"
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=os.urandom(PART_SIZE),
        )["ETag"]
        etag2 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=2, Body=os.urandom(PART_SIZE),
        )["ETag"]

        # S3 sorts parts by number — wrong order in the request still succeeds.
        resp = s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            MultipartUpload={
                "Parts": [
                    {"PartNumber": 2, "ETag": etag2},
                    {"PartNumber": 1, "ETag": etag1},
                ],
            },
        )
        assert "ETag" in resp

        # Cleanup
        s3_client.delete_object(Bucket=BUCKET, Key=key)

    def test_part_too_small(self, s3_client):
        """Non-last parts smaller than 5MB should fail at complete time."""
        key = "mp-too-small"
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        # Part 1 is too small (not last)
        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=b"tiny",
        )["ETag"]
        etag2 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=2, Body=os.urandom(PART_SIZE),
        )["ETag"]

        with pytest.raises(ClientError) as exc_info:
            s3_client.complete_multipart_upload(
                Bucket=BUCKET, Key=key, UploadId=upload_id,
                MultipartUpload={
                    "Parts": [
                        {"PartNumber": 1, "ETag": etag1},
                        {"PartNumber": 2, "ETag": etag2},
                    ],
                },
            )
        assert exc_info.value.response["Error"]["Code"] == "EntityTooSmall"

        # Cleanup
        s3_client.abort_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
        )

    def test_single_part_can_be_small(self, s3_client):
        """A single part (which is also the last) can be < 5MB."""
        key = "mp-single-small"
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        small_data = b"just a little data"
        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=small_data,
        )["ETag"]

        s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            MultipartUpload={
                "Parts": [{"PartNumber": 1, "ETag": etag1}],
            },
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key=key)
        assert resp["Body"].read() == small_data

    def test_overwrite_via_multipart(self, s3_client):
        """Multipart upload should overwrite an existing object."""
        key = "mp-overwrite"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"old data")

        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        new_data = os.urandom(PART_SIZE)
        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=new_data,
        )["ETag"]

        s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            MultipartUpload={
                "Parts": [{"PartNumber": 1, "ETag": etag1}],
            },
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key=key)
        assert resp["Body"].read() == new_data

    def test_reupload_part_overwrites(self, s3_client):
        """Re-uploading a part number should replace the previous one."""
        key = "mp-reupload"
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        # Upload part 1 first time
        s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=os.urandom(PART_SIZE),
        )

        # Re-upload part 1 with different data
        final_data = os.urandom(PART_SIZE)
        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=final_data,
        )["ETag"]

        s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            MultipartUpload={
                "Parts": [{"PartNumber": 1, "ETag": etag1}],
            },
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key=key)
        assert resp["Body"].read() == final_data


class TestAbortMultipartUpload:
    def test_abort_cleans_up(self, s3_client):
        """After abort, completing the same upload should fail."""
        key = "mp-abort"
        resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = resp["UploadId"]

        etag1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=os.urandom(PART_SIZE),
        )["ETag"]

        s3_client.abort_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
        )

        # Trying to complete should fail with NoSuchUpload
        with pytest.raises(ClientError) as exc_info:
            s3_client.complete_multipart_upload(
                Bucket=BUCKET, Key=key, UploadId=upload_id,
                MultipartUpload={
                    "Parts": [{"PartNumber": 1, "ETag": etag1}],
                },
            )
        assert exc_info.value.response["Error"]["Code"] == "NoSuchUpload"

    def test_abort_nonexistent_returns_no_such_upload(self, s3_client):
        """Aborting a nonexistent upload should return NoSuchUpload."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.abort_multipart_upload(
                Bucket=BUCKET, Key="no-such-key", UploadId="nonexistent-id",
            )
        assert exc_info.value.response["Error"]["Code"] == "NoSuchUpload"
