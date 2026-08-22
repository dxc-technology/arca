"""Integration tests for authentication using the MinIO Python client.

Mirrors test_auth.py — verifies that valid credentials work, invalid
credentials are rejected, and error codes are correct.
"""

import os
from urllib.parse import urlparse

import pytest
from minio import Minio
from minio.error import S3Error


@pytest.fixture
def endpoint_url():
    return os.environ.get("ARCA_ENDPOINT", "http://localhost:9000")


@pytest.fixture
def valid_minio(endpoint_url):
    """MinIO client with correct credentials."""
    parsed = urlparse(endpoint_url)
    return Minio(
        parsed.netloc,
        access_key=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"
        ),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
        ),
        secure=parsed.scheme == "https",
    )


@pytest.fixture
def bad_key_minio(endpoint_url):
    """MinIO client with a nonexistent access key."""
    parsed = urlparse(endpoint_url)
    return Minio(
        parsed.netloc,
        access_key="AKIAI_NONEXISTENT_KEY",
        secret_key="hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv",
        secure=parsed.scheme == "https",
    )


@pytest.fixture
def bad_secret_minio(endpoint_url):
    """MinIO client with correct access key but wrong secret."""
    parsed = urlparse(endpoint_url)
    return Minio(
        parsed.netloc,
        access_key=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"
        ),
        secret_key="THIS_IS_THE_WRONG_SECRET_KEY_1234567890",
        secure=parsed.scheme == "https",
    )


class TestAuthValidCredentials:
    """Valid credentials should allow all operations."""

    def test_list_buckets(self, valid_minio):
        buckets = valid_minio.list_buckets()
        assert isinstance(buckets, list)

    def test_create_and_delete_bucket(self, valid_minio):
        bucket = "minio-auth-valid"
        valid_minio.make_bucket(bucket)
        assert valid_minio.bucket_exists(bucket)
        valid_minio.remove_bucket(bucket)

    def test_put_get_delete_object(self, valid_minio):
        import io

        bucket = "minio-auth-objects"
        valid_minio.make_bucket(bucket)
        try:
            data = b"hello auth"
            valid_minio.put_object(
                bucket, "hello.txt", io.BytesIO(data), len(data),
            )
            resp = valid_minio.get_object(bucket, "hello.txt")
            try:
                assert resp.read() == data
            finally:
                resp.close()
                resp.release_conn()
            valid_minio.remove_object(bucket, "hello.txt")
        finally:
            valid_minio.remove_bucket(bucket)


class TestAuthBadAccessKey:
    """Nonexistent access key should return InvalidAccessKeyId."""

    def test_list_buckets_rejected(self, bad_key_minio):
        with pytest.raises(S3Error) as exc_info:
            bad_key_minio.list_buckets()
        assert exc_info.value.code == "InvalidAccessKeyId"

    def test_create_bucket_rejected(self, bad_key_minio):
        with pytest.raises(S3Error) as exc_info:
            bad_key_minio.make_bucket("should-fail")
        assert exc_info.value.code == "InvalidAccessKeyId"


class TestAuthBadSecret:
    """Correct access key but wrong secret should return SignatureDoesNotMatch."""

    def test_list_buckets_rejected(self, bad_secret_minio):
        with pytest.raises(S3Error) as exc_info:
            bad_secret_minio.list_buckets()
        assert exc_info.value.code == "SignatureDoesNotMatch"

    def test_put_object_rejected(self, bad_secret_minio, valid_minio):
        import io

        bucket = "minio-auth-badsecret"
        valid_minio.make_bucket(bucket)
        try:
            with pytest.raises(S3Error) as exc_info:
                bad_secret_minio.put_object(
                    bucket, "test.txt", io.BytesIO(b"fail"), 4,
                )
            assert exc_info.value.code == "SignatureDoesNotMatch"
        finally:
            valid_minio.remove_bucket(bucket)
