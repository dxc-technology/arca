"""TLS integration tests for Arca.

These tests run against an Arca server with TLS enabled.
They are executed via `bin/test tls`.
"""

import os
import ssl
import uuid

import pytest
import requests
import urllib3


# Skip entire module if not running in TLS mode (no CA bundle configured).
pytestmark = pytest.mark.skipif(
    not os.environ.get("AWS_CA_BUNDLE"),
    reason="TLS tests require AWS_CA_BUNDLE to be set",
)


@pytest.fixture
def ca_bundle():
    return os.environ["AWS_CA_BUNDLE"]


@pytest.fixture
def endpoint_url():
    return os.environ.get("ARCA_ENDPOINT", "https://arca:9000")


class TestHTTPSBasic:
    """Basic HTTPS connectivity tests."""

    def test_https_health_check(self, endpoint_url, ca_bundle):
        """Health endpoint accessible over HTTPS."""
        resp = requests.get(f"{endpoint_url}/admin/health", verify=ca_bundle)
        assert resp.status_code == 200
        assert resp.json()["status"] == "ok"

    def test_https_info_tls_enabled(self, endpoint_url, ca_bundle):
        """/admin/info reports tls_enabled: true."""
        from botocore.auth import S3SigV4Auth
        from botocore.credentials import Credentials
        from botocore.awsrequest import AWSRequest

        creds = Credentials(
            os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"),
            os.environ.get("AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"),
        )
        url = f"{endpoint_url}/admin/info"
        req = AWSRequest(method="GET", url=url, headers={"Host": "arca:9000"})
        S3SigV4Auth(creds, "s3", "us-east-1").add_auth(req)
        resp = requests.get(url, headers=dict(req.headers), verify=ca_bundle)
        assert resp.status_code == 200
        data = resp.json()
        assert data["tls_enabled"] is True

    def test_https_list_buckets(self, s3_client):
        """ListBuckets works over HTTPS."""
        resp = s3_client.list_buckets()
        assert "Buckets" in resp

    def test_https_put_get_object(self, s3_client):
        """PutObject + GetObject roundtrip over HTTPS."""
        bucket = f"tls-test-{uuid.uuid4().hex[:8]}"
        key = "hello.txt"
        body = b"Hello over TLS!"

        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key=key, Body=body)
            resp = s3_client.get_object(Bucket=bucket, Key=key)
            assert resp["Body"].read() == body
            s3_client.delete_object(Bucket=bucket, Key=key)
        finally:
            s3_client.delete_bucket(Bucket=bucket)

    def test_https_multipart_upload(self, s3_client):
        """Multipart upload works over HTTPS."""
        bucket = f"tls-mp-{uuid.uuid4().hex[:8]}"
        key = "multipart.bin"

        s3_client.create_bucket(Bucket=bucket)
        try:
            mpu = s3_client.create_multipart_upload(Bucket=bucket, Key=key)
            upload_id = mpu["UploadId"]

            # 5 MB minimum part size
            part_data = b"x" * (5 * 1024 * 1024)
            part1 = s3_client.upload_part(
                Bucket=bucket, Key=key, UploadId=upload_id,
                PartNumber=1, Body=part_data,
            )

            s3_client.complete_multipart_upload(
                Bucket=bucket, Key=key, UploadId=upload_id,
                MultipartUpload={"Parts": [
                    {"PartNumber": 1, "ETag": part1["ETag"]},
                ]},
            )

            resp = s3_client.head_object(Bucket=bucket, Key=key)
            assert resp["ContentLength"] == len(part_data)

            s3_client.delete_object(Bucket=bucket, Key=key)
        finally:
            s3_client.delete_bucket(Bucket=bucket)


class TestMinioClientHTTPS:
    """MinIO client over HTTPS."""

    def test_minio_client_https(self, minio_client):
        """MinIO client can list buckets over HTTPS."""
        buckets = list(minio_client.list_buckets())
        assert isinstance(buckets, list)


class TestTLSVerification:
    """TLS certificate verification tests."""

    def test_wrong_ca_rejected(self, endpoint_url):
        """Connection with wrong/no CA bundle fails."""
        with pytest.raises((requests.exceptions.SSLError, urllib3.exceptions.SSLError)):
            requests.get(
                f"{endpoint_url}/admin/health",
                verify=True,  # uses system CA store, which won't have our self-signed CA
                timeout=5,
            )
