"""Integration tests for Arca Phase 13 — Server-Side Encryption (SSE-S3).

These tests run against an Arca server with encryption enabled.
They are executed via `bin/test encryption`.
"""

import hashlib
import io
import os

import pytest
import requests
from botocore.exceptions import ClientError


# Skip entire module if encryption is not enabled.
pytestmark = pytest.mark.skipif(
    not os.environ.get("ARCA_ENCRYPTION_ENABLED"),
    reason="Encryption tests require ARCA_ENCRYPTION_ENABLED to be set",
)


BUCKET = "test-encryption-bucket"
BUCKET_CFG = "test-encryption-config-bucket"

# 5 MB minimum part size for multipart uploads.
PART_SIZE = 5 * 1024 * 1024


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    for b in [BUCKET, BUCKET_CFG]:
        try:
            s3_client.create_bucket(Bucket=b)
        except ClientError:
            pass
    yield
    for b in [BUCKET, BUCKET_CFG]:
        try:
            response = s3_client.list_objects_v2(Bucket=b)
            for obj in response.get("Contents", []):
                s3_client.delete_object(Bucket=b, Key=obj["Key"])
            s3_client.delete_bucket(Bucket=b)
        except ClientError:
            pass


class TestEncryptedPutGet:
    """Basic encrypted object put/get roundtrip tests."""

    def test_put_get_encrypted_object(self, s3_client):
        """Put an object on an encrypted server and get it back."""
        data = b"hello encrypted world"
        s3_client.put_object(Bucket=BUCKET, Key="test.txt", Body=data)

        resp = s3_client.get_object(Bucket=BUCKET, Key="test.txt")
        body = resp["Body"].read()
        assert body == data

    def test_encrypted_object_etag_matches_plaintext_md5(self, s3_client):
        """ETag should be the MD5 of the plaintext, not the ciphertext."""
        data = b"etag verification data"
        expected_md5 = hashlib.md5(data).hexdigest()

        s3_client.put_object(Bucket=BUCKET, Key="etag-check", Body=data)

        resp = s3_client.head_object(Bucket=BUCKET, Key="etag-check")
        etag = resp["ETag"].strip('"')
        assert etag == expected_md5

    def test_encrypted_object_response_headers(self, s3_client):
        """PutObject and GetObject should return x-amz-server-side-encryption header."""
        data = b"check headers"
        put_resp = s3_client.put_object(Bucket=BUCKET, Key="hdr-test", Body=data)
        assert put_resp.get("ServerSideEncryption") == "AES256"

        get_resp = s3_client.get_object(Bucket=BUCKET, Key="hdr-test")
        assert get_resp.get("ServerSideEncryption") == "AES256"
        get_resp["Body"].read()

    def test_head_encrypted_object(self, s3_client):
        """HeadObject returns encryption header."""
        data = b"head check"
        s3_client.put_object(Bucket=BUCKET, Key="head-test", Body=data)

        resp = s3_client.head_object(Bucket=BUCKET, Key="head-test")
        assert resp.get("ServerSideEncryption") == "AES256"
        assert resp["ContentLength"] == len(data)

    def test_empty_object_encrypted(self, s3_client):
        """Empty object should round-trip through encryption."""
        s3_client.put_object(Bucket=BUCKET, Key="empty", Body=b"")

        resp = s3_client.get_object(Bucket=BUCKET, Key="empty")
        body = resp["Body"].read()
        assert body == b""

    def test_large_object_encrypted(self, s3_client):
        """200 KB object should round-trip through encryption (multiple chunks)."""
        data = os.urandom(200 * 1024)
        s3_client.put_object(Bucket=BUCKET, Key="large", Body=data)

        resp = s3_client.get_object(Bucket=BUCKET, Key="large")
        body = resp["Body"].read()
        assert body == data


class TestEncryptedRangeRead:
    """Byte range reads on encrypted objects."""

    def test_encrypted_range_read(self, s3_client):
        """Byte range on an encrypted object should return correct data."""
        data = b"abcdefghijklmnopqrstuvwxyz"
        s3_client.put_object(Bucket=BUCKET, Key="range-test", Body=data)

        resp = s3_client.get_object(
            Bucket=BUCKET, Key="range-test", Range="bytes=3-6",
        )
        body = resp["Body"].read()
        assert body == b"defg"

    def test_encrypted_range_cross_chunk(self, s3_client):
        """Range spanning a chunk boundary should return correct data."""
        # 100 KB — spans at least one 64 KB chunk boundary.
        data = os.urandom(100 * 1024)
        s3_client.put_object(Bucket=BUCKET, Key="cross-chunk", Body=data)

        # Read bytes spanning the chunk boundary (around 64 KB mark).
        start = 65000
        end = 66000
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="cross-chunk", Range=f"bytes={start}-{end}",
        )
        body = resp["Body"].read()
        assert body == data[start:end + 1]

    def test_encrypted_suffix_range(self, s3_client):
        """Suffix range read (bytes=-N) on encrypted object."""
        data = b"suffix range test data here"
        s3_client.put_object(Bucket=BUCKET, Key="suffix-range", Body=data)

        resp = s3_client.get_object(
            Bucket=BUCKET, Key="suffix-range", Range="bytes=-4",
        )
        body = resp["Body"].read()
        assert body == data[-4:]


class TestEncryptedMultipart:
    """Multipart uploads produce encrypted objects."""

    def test_encrypted_multipart_upload(self, s3_client):
        """Multipart upload should produce an encrypted object."""
        key = "multipart-encrypted"
        create_resp = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
        upload_id = create_resp["UploadId"]

        part1_data = os.urandom(PART_SIZE)
        part2_data = os.urandom(1024)  # small last part

        part1 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=1, Body=part1_data,
        )
        part2 = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            PartNumber=2, Body=part2_data,
        )

        s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=upload_id,
            MultipartUpload={
                "Parts": [
                    {"PartNumber": 1, "ETag": part1["ETag"]},
                    {"PartNumber": 2, "ETag": part2["ETag"]},
                ],
            },
        )

        # Verify the assembled object is readable and encrypted.
        resp = s3_client.get_object(Bucket=BUCKET, Key=key)
        assert resp.get("ServerSideEncryption") == "AES256"
        body = resp["Body"].read()
        assert body == part1_data + part2_data


class TestCopyEncrypted:
    """CopyObject with encrypted objects."""

    def test_copy_encrypted_object(self, s3_client):
        """CopyObject should produce an encrypted copy."""
        data = b"copy me encrypted"
        s3_client.put_object(Bucket=BUCKET, Key="original", Body=data)

        s3_client.copy_object(
            Bucket=BUCKET, Key="copy",
            CopySource=f"{BUCKET}/original",
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key="copy")
        assert resp.get("ServerSideEncryption") == "AES256"
        body = resp["Body"].read()
        assert body == data


class TestDeleteEncrypted:
    """Deleting encrypted objects."""

    def test_delete_encrypted_object(self, s3_client):
        """Delete should work normally for encrypted objects."""
        s3_client.put_object(Bucket=BUCKET, Key="delete-me", Body=b"bye")
        s3_client.delete_object(Bucket=BUCKET, Key="delete-me")

        with pytest.raises(ClientError) as exc:
            s3_client.head_object(Bucket=BUCKET, Key="delete-me")
        assert exc.value.response["Error"]["Code"] == "404"


class TestBucketEncryptionConfig:
    """PutBucketEncryption / GetBucketEncryption / DeleteBucketEncryption."""

    def test_get_bucket_encryption_default(self, s3_client):
        """GetBucketEncryption returns server default when no per-bucket config."""
        resp = s3_client.get_bucket_encryption(Bucket=BUCKET_CFG)
        rules = resp["ServerSideEncryptionConfiguration"]["Rules"]
        assert len(rules) == 1
        assert rules[0]["ApplyServerSideEncryptionByDefault"]["SSEAlgorithm"] == "AES256"

    def test_put_bucket_encryption(self, s3_client):
        """PutBucketEncryption sets per-bucket encryption config."""
        s3_client.put_bucket_encryption(
            Bucket=BUCKET_CFG,
            ServerSideEncryptionConfiguration={
                "Rules": [
                    {
                        "ApplyServerSideEncryptionByDefault": {
                            "SSEAlgorithm": "AES256",
                        },
                    },
                ],
            },
        )

        resp = s3_client.get_bucket_encryption(Bucket=BUCKET_CFG)
        rules = resp["ServerSideEncryptionConfiguration"]["Rules"]
        assert rules[0]["ApplyServerSideEncryptionByDefault"]["SSEAlgorithm"] == "AES256"

    def test_delete_bucket_encryption(self, s3_client):
        """DeleteBucketEncryption removes per-bucket config; falls back to global."""
        # Set per-bucket config first.
        s3_client.put_bucket_encryption(
            Bucket=BUCKET_CFG,
            ServerSideEncryptionConfiguration={
                "Rules": [
                    {
                        "ApplyServerSideEncryptionByDefault": {
                            "SSEAlgorithm": "AES256",
                        },
                    },
                ],
            },
        )

        # Delete it.
        s3_client.delete_bucket_encryption(Bucket=BUCKET_CFG)

        # Should still return AES256 via global default.
        resp = s3_client.get_bucket_encryption(Bucket=BUCKET_CFG)
        rules = resp["ServerSideEncryptionConfiguration"]["Rules"]
        assert rules[0]["ApplyServerSideEncryptionByDefault"]["SSEAlgorithm"] == "AES256"


class TestOverwriteEncrypted:
    """Overwriting objects with encryption."""

    def test_overwrite_object(self, s3_client):
        """Overwriting an encrypted object should produce a new encrypted object."""
        s3_client.put_object(Bucket=BUCKET, Key="overwrite", Body=b"version1")
        s3_client.put_object(Bucket=BUCKET, Key="overwrite", Body=b"version2")

        resp = s3_client.get_object(Bucket=BUCKET, Key="overwrite")
        body = resp["Body"].read()
        assert body == b"version2"
        assert resp.get("ServerSideEncryption") == "AES256"
