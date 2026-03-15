"""Integration tests for per-bucket encryption.

These tests run against an Arca server with a master key configured
but global encryption DISABLED (enabled = false). They verify that
PutBucketEncryption enables encryption on a per-bucket basis.

Executed via: bin/test per-bucket-encryption
"""

import hashlib
import os

import pytest
from botocore.exceptions import ClientError


# Skip entire module if not running in per-bucket encryption mode.
pytestmark = pytest.mark.skipif(
    not os.environ.get("ARCA_PER_BUCKET_ENCRYPTION"),
    reason="Per-bucket encryption tests require ARCA_PER_BUCKET_ENCRYPTION to be set",
)


BUCKET_PLAIN = "test-pbe-plain"
BUCKET_ENCRYPTED = "test-pbe-encrypted"


@pytest.fixture(autouse=True)
def setup_buckets(s3_client):
    """Create test buckets before each test, clean up after."""
    for b in [BUCKET_PLAIN, BUCKET_ENCRYPTED]:
        try:
            s3_client.create_bucket(Bucket=b)
        except ClientError:
            pass
    yield
    for b in [BUCKET_PLAIN, BUCKET_ENCRYPTED]:
        try:
            response = s3_client.list_objects_v2(Bucket=b)
            for obj in response.get("Contents", []):
                s3_client.delete_object(Bucket=b, Key=obj["Key"])
            s3_client.delete_bucket(Bucket=b)
        except ClientError:
            pass


class TestGlobalDefault:
    """When global encryption is off, objects should NOT be encrypted by default."""

    def test_plain_object_no_encryption_header(self, s3_client):
        """PutObject on a non-encrypted bucket should not return SSE header."""
        s3_client.put_object(Bucket=BUCKET_PLAIN, Key="plain.txt", Body=b"hello")
        resp = s3_client.get_object(Bucket=BUCKET_PLAIN, Key="plain.txt")
        body = resp["Body"].read()
        assert body == b"hello"
        assert resp.get("ServerSideEncryption") is None

    def test_get_bucket_encryption_no_config(self, s3_client):
        """GetBucketEncryption should return error when no config exists."""
        with pytest.raises(ClientError) as exc:
            s3_client.get_bucket_encryption(Bucket=BUCKET_PLAIN)
        assert "ServerSideEncryptionConfigurationNotFoundError" in str(exc.value)


class TestPerBucketEncryption:
    """Enable encryption on a specific bucket via PutBucketEncryption."""

    def test_put_bucket_encryption(self, s3_client):
        """PutBucketEncryption should succeed when master key is configured."""
        s3_client.put_bucket_encryption(
            Bucket=BUCKET_ENCRYPTED,
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
        resp = s3_client.get_bucket_encryption(Bucket=BUCKET_ENCRYPTED)
        rules = resp["ServerSideEncryptionConfiguration"]["Rules"]
        assert rules[0]["ApplyServerSideEncryptionByDefault"]["SSEAlgorithm"] == "AES256"

    def test_encrypted_bucket_put_get(self, s3_client):
        """Objects in an encrypted bucket should be encrypted and readable."""
        # Enable encryption on the bucket.
        s3_client.put_bucket_encryption(
            Bucket=BUCKET_ENCRYPTED,
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

        data = b"per-bucket encrypted data"
        s3_client.put_object(Bucket=BUCKET_ENCRYPTED, Key="secret.txt", Body=data)

        # GetObject should return SSE header and correct data.
        resp = s3_client.get_object(Bucket=BUCKET_ENCRYPTED, Key="secret.txt")
        body = resp["Body"].read()
        assert body == data
        assert resp.get("ServerSideEncryption") == "AES256"

    def test_encrypted_bucket_etag_is_plaintext_md5(self, s3_client):
        """ETag should be the MD5 of the plaintext for encrypted objects."""
        s3_client.put_bucket_encryption(
            Bucket=BUCKET_ENCRYPTED,
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

        data = b"etag verification for per-bucket encryption"
        expected_md5 = hashlib.md5(data).hexdigest()
        s3_client.put_object(Bucket=BUCKET_ENCRYPTED, Key="etag.txt", Body=data)

        resp = s3_client.head_object(Bucket=BUCKET_ENCRYPTED, Key="etag.txt")
        etag = resp["ETag"].strip('"')
        assert etag == expected_md5

    def test_head_encrypted_object(self, s3_client):
        """HeadObject should return SSE header for per-bucket encrypted objects."""
        s3_client.put_bucket_encryption(
            Bucket=BUCKET_ENCRYPTED,
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

        data = b"head check"
        s3_client.put_object(Bucket=BUCKET_ENCRYPTED, Key="head.txt", Body=data)

        resp = s3_client.head_object(Bucket=BUCKET_ENCRYPTED, Key="head.txt")
        assert resp.get("ServerSideEncryption") == "AES256"
        assert resp["ContentLength"] == len(data)

    def test_delete_bucket_encryption_reverts(self, s3_client):
        """After DeleteBucketEncryption, new objects should not be encrypted."""
        # Enable encryption.
        s3_client.put_bucket_encryption(
            Bucket=BUCKET_ENCRYPTED,
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

        # Write an encrypted object.
        s3_client.put_object(Bucket=BUCKET_ENCRYPTED, Key="before.txt", Body=b"encrypted")
        resp = s3_client.get_object(Bucket=BUCKET_ENCRYPTED, Key="before.txt")
        assert resp.get("ServerSideEncryption") == "AES256"
        resp["Body"].read()

        # Disable encryption.
        s3_client.delete_bucket_encryption(Bucket=BUCKET_ENCRYPTED)

        # New object should NOT be encrypted.
        s3_client.put_object(Bucket=BUCKET_ENCRYPTED, Key="after.txt", Body=b"plaintext")
        resp = s3_client.get_object(Bucket=BUCKET_ENCRYPTED, Key="after.txt")
        body = resp["Body"].read()
        assert body == b"plaintext"
        assert resp.get("ServerSideEncryption") is None

        # Old encrypted object should still be readable.
        resp = s3_client.get_object(Bucket=BUCKET_ENCRYPTED, Key="before.txt")
        body = resp["Body"].read()
        assert body == b"encrypted"

    def test_plain_bucket_stays_plain(self, s3_client):
        """A bucket without per-bucket encryption should stay unencrypted."""
        s3_client.put_bucket_encryption(
            Bucket=BUCKET_ENCRYPTED,
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

        # The PLAIN bucket should not be affected.
        s3_client.put_object(Bucket=BUCKET_PLAIN, Key="still-plain.txt", Body=b"no encryption")
        resp = s3_client.get_object(Bucket=BUCKET_PLAIN, Key="still-plain.txt")
        body = resp["Body"].read()
        assert body == b"no encryption"
        assert resp.get("ServerSideEncryption") is None
