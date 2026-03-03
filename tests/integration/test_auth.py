"""Integration tests for AWS SigV4 authentication."""

import os

import boto3
import pytest
from botocore.exceptions import ClientError


@pytest.fixture
def endpoint_url():
    return os.environ.get("ARCA_ENDPOINT", "http://localhost:9000")


@pytest.fixture
def valid_client(endpoint_url):
    """Client with correct credentials."""
    return boto3.client(
        "s3",
        endpoint_url=endpoint_url,
        aws_access_key_id=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"
        ),
        aws_secret_access_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        ),
        region_name="us-east-1",
    )


@pytest.fixture
def bad_key_client(endpoint_url):
    """Client with a nonexistent access key."""
    return boto3.client(
        "s3",
        endpoint_url=endpoint_url,
        aws_access_key_id="AKIAI_NONEXISTENT_KEY",
        aws_secret_access_key="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        region_name="us-east-1",
    )


@pytest.fixture
def bad_secret_client(endpoint_url):
    """Client with correct access key but wrong secret."""
    return boto3.client(
        "s3",
        endpoint_url=endpoint_url,
        aws_access_key_id=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"
        ),
        aws_secret_access_key="THIS_IS_THE_WRONG_SECRET_KEY_1234567890",
        region_name="us-east-1",
    )


class TestAuthValidCredentials:
    """Valid credentials should allow all operations."""

    def test_list_buckets(self, valid_client):
        resp = valid_client.list_buckets()
        assert "Buckets" in resp

    def test_create_and_delete_bucket(self, valid_client):
        bucket = "auth-test-valid"
        valid_client.create_bucket(Bucket=bucket)
        valid_client.head_bucket(Bucket=bucket)
        valid_client.delete_bucket(Bucket=bucket)

    def test_put_get_delete_object(self, valid_client):
        bucket = "auth-test-objects"
        valid_client.create_bucket(Bucket=bucket)
        try:
            valid_client.put_object(
                Bucket=bucket, Key="hello.txt", Body=b"hello auth"
            )
            resp = valid_client.get_object(Bucket=bucket, Key="hello.txt")
            assert resp["Body"].read() == b"hello auth"
            valid_client.delete_object(Bucket=bucket, Key="hello.txt")
        finally:
            valid_client.delete_bucket(Bucket=bucket)


class TestAuthBadAccessKey:
    """Nonexistent access key should return InvalidAccessKeyId."""

    def test_list_buckets_rejected(self, bad_key_client):
        with pytest.raises(ClientError) as exc_info:
            bad_key_client.list_buckets()
        assert exc_info.value.response["Error"]["Code"] == "InvalidAccessKeyId"

    def test_create_bucket_rejected(self, bad_key_client):
        with pytest.raises(ClientError) as exc_info:
            bad_key_client.create_bucket(Bucket="should-fail")
        assert exc_info.value.response["Error"]["Code"] == "InvalidAccessKeyId"


class TestAuthBadSecret:
    """Correct access key but wrong secret should return SignatureDoesNotMatch."""

    def test_list_buckets_rejected(self, bad_secret_client):
        with pytest.raises(ClientError) as exc_info:
            bad_secret_client.list_buckets()
        assert (
            exc_info.value.response["Error"]["Code"] == "SignatureDoesNotMatch"
        )

    def test_put_object_rejected(self, bad_secret_client, valid_client):
        bucket = "auth-test-badsecret"
        valid_client.create_bucket(Bucket=bucket)
        try:
            with pytest.raises(ClientError) as exc_info:
                bad_secret_client.put_object(
                    Bucket=bucket, Key="test.txt", Body=b"fail"
                )
            assert (
                exc_info.value.response["Error"]["Code"]
                == "SignatureDoesNotMatch"
            )
        finally:
            valid_client.delete_bucket(Bucket=bucket)


class TestAuthMissingHeader:
    """Requests without Authorization header should return AccessDenied."""

    def test_no_auth_header(self, endpoint_url):
        """Raw HTTP request without any auth headers."""
        import requests

        resp = requests.get(endpoint_url)
        assert resp.status_code == 403
        assert "AccessDenied" in resp.text
