"""Integration tests for Arca Phase 14 — SSE-KMS with Vault/OpenBAO.

These tests run against an Arca server with encryption enabled via
a Vault/OpenBAO KV v2 backend (master key fetched at startup).
They are executed via `bin/test kms`.
"""

import hashlib
import os

import pytest
import requests
from botocore.exceptions import ClientError


# Skip entire module if KMS is not enabled.
pytestmark = pytest.mark.skipif(
    not os.environ.get("ARCA_KMS_ENABLED"),
    reason="KMS tests require ARCA_KMS_ENABLED to be set",
)


BUCKET = "test-kms-bucket"
BUCKET_CFG = "test-kms-config-bucket"

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


class TestKmsPutGet:
    """Basic encrypted object put/get roundtrip with KMS key source."""

    def test_put_get_encrypted_object(self, s3_client):
        """Put an object on a KMS-encrypted server and get it back."""
        data = b"hello kms encrypted world"
        s3_client.put_object(Bucket=BUCKET, Key="test.txt", Body=data)

        resp = s3_client.get_object(Bucket=BUCKET, Key="test.txt")
        body = resp["Body"].read()
        assert body == data

    def test_put_returns_encryption_header(self, s3_client):
        """PutObject should return x-amz-server-side-encryption: AES256."""
        data = b"header check"
        resp = s3_client.put_object(Bucket=BUCKET, Key="hdr-put", Body=data)
        assert resp.get("ServerSideEncryption") == "AES256"

    def test_head_returns_encryption_header(self, s3_client):
        """HeadObject should return encryption header."""
        data = b"head check"
        s3_client.put_object(Bucket=BUCKET, Key="hdr-head", Body=data)

        resp = s3_client.head_object(Bucket=BUCKET, Key="hdr-head")
        assert resp.get("ServerSideEncryption") == "AES256"
        assert resp["ContentLength"] == len(data)

    def test_get_returns_encryption_header(self, s3_client):
        """GetObject should return encryption header."""
        data = b"get header check"
        s3_client.put_object(Bucket=BUCKET, Key="hdr-get", Body=data)

        resp = s3_client.get_object(Bucket=BUCKET, Key="hdr-get")
        assert resp.get("ServerSideEncryption") == "AES256"
        resp["Body"].read()

    def test_etag_is_plaintext_md5(self, s3_client):
        """ETag should be the MD5 of the plaintext, not the ciphertext."""
        data = b"etag verification data for kms"
        expected_md5 = hashlib.md5(data).hexdigest()

        s3_client.put_object(Bucket=BUCKET, Key="etag-check", Body=data)

        resp = s3_client.head_object(Bucket=BUCKET, Key="etag-check")
        etag = resp["ETag"].strip('"')
        assert etag == expected_md5


class TestKmsMultipart:
    """Multipart uploads produce encrypted objects with KMS key source."""

    def test_large_object_multipart(self, s3_client):
        """Multipart upload should produce an encrypted object."""
        key = "multipart-kms"
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

        resp = s3_client.get_object(Bucket=BUCKET, Key=key)
        assert resp.get("ServerSideEncryption") == "AES256"
        body = resp["Body"].read()
        assert body == part1_data + part2_data


class TestKmsCopyAndRange:
    """CopyObject and range reads with KMS key source."""

    def test_copy_encrypted_object(self, s3_client):
        """CopyObject should produce an encrypted copy."""
        data = b"copy me kms encrypted"
        s3_client.put_object(Bucket=BUCKET, Key="original", Body=data)

        s3_client.copy_object(
            Bucket=BUCKET, Key="copy",
            CopySource=f"{BUCKET}/original",
        )

        resp = s3_client.get_object(Bucket=BUCKET, Key="copy")
        assert resp.get("ServerSideEncryption") == "AES256"
        body = resp["Body"].read()
        assert body == data

    def test_range_read_encrypted(self, s3_client):
        """Byte range on a KMS-encrypted object should return correct data."""
        data = b"abcdefghijklmnopqrstuvwxyz"
        s3_client.put_object(Bucket=BUCKET, Key="range-test", Body=data)

        resp = s3_client.get_object(
            Bucket=BUCKET, Key="range-test", Range="bytes=3-6",
        )
        body = resp["Body"].read()
        assert body == b"defg"


class TestKmsAdminInfo:
    """Admin API reflects KMS provider information."""

    def test_admin_info_kms_provider(self, endpoint_url):
        """GET /admin/info should return kms_provider: vault."""
        from botocore.auth import S3SigV4Auth
        from botocore.awsrequest import AWSRequest
        from botocore.credentials import Credentials

        creds = Credentials(
            access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"),
            secret_key=os.environ.get("AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"),
        )
        url = f"{endpoint_url}/admin/info"
        aws_req = AWSRequest(method="GET", url=url, data="")
        S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)

        resp = requests.get(url, headers=dict(aws_req.headers), timeout=10)
        assert resp.status_code == 200
        info = resp.json()
        assert info["encryption_enabled"] is True
        assert info["kms_provider"] == "vault"
        assert "openbao" in info["kms_endpoint"]


class TestKmsPerBucketEncryption:
    """Per-bucket encryption works with KMS key source."""

    def test_per_bucket_encryption_with_kms(self, s3_client):
        """PutBucketEncryption / GetBucketEncryption works with KMS backend."""
        # Since global encryption is ON, bucket already has encryption.
        resp = s3_client.get_bucket_encryption(Bucket=BUCKET_CFG)
        rules = resp["ServerSideEncryptionConfiguration"]["Rules"]
        assert len(rules) == 1
        assert rules[0]["ApplyServerSideEncryptionByDefault"]["SSEAlgorithm"] == "AES256"

        # Explicitly set per-bucket encryption.
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

        # Verify it persists.
        resp = s3_client.get_bucket_encryption(Bucket=BUCKET_CFG)
        rules = resp["ServerSideEncryptionConfiguration"]["Rules"]
        assert rules[0]["ApplyServerSideEncryptionByDefault"]["SSEAlgorithm"] == "AES256"
