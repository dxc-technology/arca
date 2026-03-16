"""Integration tests for SSE-C (Server-Side Encryption with Customer-provided keys).

SSE-C tests run against a regular Arca server (no special encryption config needed).
The customer provides the key on each request.
"""

import base64
import hashlib
import os

import pytest
import requests
from botocore.exceptions import ClientError


BUCKET = "test-ssec-bucket"


def generate_key():
    """Generate a random 32-byte SSE-C key and its base64-encoded MD5."""
    key = os.urandom(32)
    key_md5 = base64.b64encode(hashlib.md5(key).digest()).decode()
    return key, key_md5


# Two distinct keys for testing wrong-key scenarios.
KEY_A = os.urandom(32)
KEY_B = os.urandom(32)


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    """Create and clean up test bucket."""
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass
    yield
    # Clean up: we can't list SSE-C objects without keys, but delete doesn't need them
    try:
        response = s3_client.list_objects_v2(Bucket=BUCKET)
        for obj in response.get("Contents", []):
            s3_client.delete_object(Bucket=BUCKET, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass


class TestSsecPutGet:
    """Basic SSE-C put/get roundtrip tests."""

    def test_put_get_roundtrip(self, s3_client):
        """Put an SSE-C encrypted object and get it back with the same key."""
        data = b"SSE-C encrypted data"
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-basic.txt",
            Body=data,
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        resp = s3_client.get_object(
            Bucket=BUCKET,
            Key="ssec-basic.txt",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )
        assert resp["Body"].read() == data

    def test_put_response_headers(self, s3_client):
        """PutObject with SSE-C returns SSE-C response headers."""
        data = b"check headers"
        resp = s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-headers.txt",
            Body=data,
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )
        assert resp["SSECustomerAlgorithm"] == "AES256"
        assert "SSECustomerKeyMD5" in resp

    def test_large_object(self, s3_client):
        """SSE-C works with larger objects (multiple encryption chunks)."""
        # 256 KB - spans multiple 64 KB chunks
        data = os.urandom(256 * 1024)
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-large.bin",
            Body=data,
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        resp = s3_client.get_object(
            Bucket=BUCKET,
            Key="ssec-large.bin",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )
        assert resp["Body"].read() == data

    def test_empty_object(self, s3_client):
        """SSE-C works with empty objects."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-empty.txt",
            Body=b"",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        resp = s3_client.get_object(
            Bucket=BUCKET,
            Key="ssec-empty.txt",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )
        assert resp["Body"].read() == b""


class TestSsecGetErrors:
    """Tests for SSE-C error handling on GET."""

    def test_get_without_key_fails(self, s3_client):
        """GET on an SSE-C object without providing the key returns an error."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-nokey.txt",
            Body=b"needs key",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        with pytest.raises(ClientError) as exc_info:
            s3_client.get_object(Bucket=BUCKET, Key="ssec-nokey.txt")
        assert exc_info.value.response["Error"]["Code"] in (
            "InvalidRequest",
            "400",
        )

    def test_get_with_wrong_key_fails(self, s3_client):
        """GET on an SSE-C object with wrong key fails."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-wrongkey.txt",
            Body=b"wrong key test",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        # This should fail because the decryption will fail with wrong key.
        # The server might return an error during streaming or immediately.
        with pytest.raises(Exception):
            resp = s3_client.get_object(
                Bucket=BUCKET,
                Key="ssec-wrongkey.txt",
                SSECustomerAlgorithm="AES256",
                SSECustomerKey=KEY_B,
            )
            # Force reading the body to trigger decryption error
            resp["Body"].read()


class TestSsecHead:
    """Tests for SSE-C HeadObject."""

    def test_head_with_key(self, s3_client):
        """HeadObject with SSE-C key returns metadata."""
        data = b"head test"
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-head.txt",
            Body=data,
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        resp = s3_client.head_object(
            Bucket=BUCKET,
            Key="ssec-head.txt",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )
        assert resp["ContentLength"] == len(data)
        assert resp["SSECustomerAlgorithm"] == "AES256"
        assert "SSECustomerKeyMD5" in resp

    def test_head_without_key_fails(self, s3_client):
        """HeadObject on SSE-C object without key returns error."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-head-nokey.txt",
            Body=b"needs key for head",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(Bucket=BUCKET, Key="ssec-head-nokey.txt")
        # HeadObject returns status codes, not XML error bodies
        assert exc_info.value.response["Error"]["Code"] in (
            "InvalidRequest",
            "400",
        )


class TestSsecCopy:
    """Tests for SSE-C CopyObject."""

    def test_copy_ssec_to_plain(self, s3_client):
        """Copy an SSE-C object to a plain (unencrypted) object."""
        data = b"copy from ssec"
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-src.txt",
            Body=data,
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        s3_client.copy_object(
            Bucket=BUCKET,
            Key="plain-dest.txt",
            CopySource={"Bucket": BUCKET, "Key": "ssec-src.txt"},
            CopySourceSSECustomerAlgorithm="AES256",
            CopySourceSSECustomerKey=KEY_A,
        )

        # Read the plain copy (no SSE-C key needed)
        resp = s3_client.get_object(Bucket=BUCKET, Key="plain-dest.txt")
        assert resp["Body"].read() == data

    def test_copy_plain_to_ssec(self, s3_client):
        """Copy a plain object to an SSE-C encrypted object."""
        data = b"copy to ssec"
        s3_client.put_object(Bucket=BUCKET, Key="plain-src.txt", Body=data)

        s3_client.copy_object(
            Bucket=BUCKET,
            Key="ssec-dest.txt",
            CopySource={"Bucket": BUCKET, "Key": "plain-src.txt"},
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_B,
        )

        # Read with the SSE-C key
        resp = s3_client.get_object(
            Bucket=BUCKET,
            Key="ssec-dest.txt",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_B,
        )
        assert resp["Body"].read() == data

    def test_copy_ssec_to_ssec(self, s3_client):
        """Copy SSE-C object to another SSE-C object with different key."""
        data = b"ssec to ssec"
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-src2.txt",
            Body=data,
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        s3_client.copy_object(
            Bucket=BUCKET,
            Key="ssec-dest2.txt",
            CopySource={"Bucket": BUCKET, "Key": "ssec-src2.txt"},
            CopySourceSSECustomerAlgorithm="AES256",
            CopySourceSSECustomerKey=KEY_A,
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_B,
        )

        resp = s3_client.get_object(
            Bucket=BUCKET,
            Key="ssec-dest2.txt",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_B,
        )
        assert resp["Body"].read() == data

    def test_copy_ssec_without_source_key_fails(self, s3_client):
        """Copy SSE-C source without providing source key fails."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-copy-nokey.txt",
            Body=b"need source key",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        with pytest.raises(ClientError) as exc_info:
            s3_client.copy_object(
                Bucket=BUCKET,
                Key="dest-nokey.txt",
                CopySource={"Bucket": BUCKET, "Key": "ssec-copy-nokey.txt"},
            )
        assert exc_info.value.response["Error"]["Code"] in (
            "InvalidRequest",
            "400",
        )


class TestSsecRange:
    """Tests for SSE-C range reads."""

    def test_range_read(self, s3_client):
        """Range read on SSE-C object returns correct bytes."""
        data = b"0123456789abcdefghijklmnopqrstuvwxyz"
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-range.txt",
            Body=data,
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        resp = s3_client.get_object(
            Bucket=BUCKET,
            Key="ssec-range.txt",
            Range="bytes=10-19",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )
        assert resp["Body"].read() == b"abcdefghij"
        assert resp["ContentLength"] == 10


class TestSsecDelete:
    """Tests for SSE-C delete."""

    def test_delete_no_key_needed(self, s3_client):
        """Delete SSE-C object does not require the encryption key."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="ssec-delete.txt",
            Body=b"delete me",
            SSECustomerAlgorithm="AES256",
            SSECustomerKey=KEY_A,
        )

        # Delete without SSE-C headers
        s3_client.delete_object(Bucket=BUCKET, Key="ssec-delete.txt")

        # Verify it's gone
        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(
                Bucket=BUCKET,
                Key="ssec-delete.txt",
                SSECustomerAlgorithm="AES256",
                SSECustomerKey=KEY_A,
            )
        assert exc_info.value.response["Error"]["Code"] in ("404", "NoSuchKey")


class TestSsecValidation:
    """Tests for SSE-C header validation."""

    def test_invalid_algorithm_rejected(self, s3_client, endpoint_url):
        """Non-AES256 algorithm is rejected."""
        # boto3 won't let us send an invalid algorithm, so use raw requests
        from botocore.auth import S3SigV4Auth
        from botocore.awsrequest import AWSRequest
        from botocore.credentials import Credentials

        creds = Credentials(
            access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
            secret_key=os.environ.get(
                "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
            ),
        )

        key = base64.b64encode(KEY_A).decode()
        key_md5 = base64.b64encode(hashlib.md5(KEY_A).digest()).decode()

        url = f"{endpoint_url}/{BUCKET}/invalid-algo.txt"
        headers = {
            "x-amz-server-side-encryption-customer-algorithm": "DES",
            "x-amz-server-side-encryption-customer-key": key,
            "x-amz-server-side-encryption-customer-key-md5": key_md5,
            "x-amz-content-sha256": "UNSIGNED-PAYLOAD",
        }
        aws_req = AWSRequest(method="PUT", url=url, data=b"test", headers=headers)
        S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)

        resp = requests.put(url, headers=dict(aws_req.headers), data=b"test", timeout=10)
        assert resp.status_code == 400

    def test_key_md5_mismatch_rejected(self, s3_client, endpoint_url):
        """Wrong key MD5 is rejected."""
        from botocore.auth import S3SigV4Auth
        from botocore.awsrequest import AWSRequest
        from botocore.credentials import Credentials

        creds = Credentials(
            access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
            secret_key=os.environ.get(
                "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
            ),
        )

        key = base64.b64encode(KEY_A).decode()
        wrong_md5 = base64.b64encode(b"wrong" + b"\x00" * 11).decode()

        url = f"{endpoint_url}/{BUCKET}/bad-md5.txt"
        headers = {
            "x-amz-server-side-encryption-customer-algorithm": "AES256",
            "x-amz-server-side-encryption-customer-key": key,
            "x-amz-server-side-encryption-customer-key-md5": wrong_md5,
            "x-amz-content-sha256": "UNSIGNED-PAYLOAD",
        }
        aws_req = AWSRequest(method="PUT", url=url, data=b"test", headers=headers)
        S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)

        resp = requests.put(url, headers=dict(aws_req.headers), data=b"test", timeout=10)
        assert resp.status_code == 400


class TestSsecMultipartRejection:
    """Tests that SSE-C is properly rejected for multipart uploads."""

    def test_create_multipart_with_ssec_rejected(self, s3_client):
        """CreateMultipartUpload with SSE-C headers is rejected."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.create_multipart_upload(
                Bucket=BUCKET,
                Key="ssec-multipart.txt",
                SSECustomerAlgorithm="AES256",
                SSECustomerKey=KEY_A,
            )
        assert exc_info.value.response["Error"]["Code"] == "InvalidArgument"
