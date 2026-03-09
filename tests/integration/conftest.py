"""Shared fixtures for Arca integration tests."""

import os
from urllib.parse import urlparse

import boto3
import pytest
from minio import Minio


@pytest.fixture
def endpoint_url():
    """S3 endpoint URL for the Arca server."""
    return os.environ.get("ARCA_ENDPOINT", "http://localhost:9000")


@pytest.fixture
def s3_client(endpoint_url):
    """boto3 S3 client configured to talk to Arca with valid credentials."""
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
def minio_client(endpoint_url):
    """MinIO Python client configured to talk to Arca with valid credentials."""
    parsed = urlparse(endpoint_url)
    return Minio(
        parsed.netloc,
        access_key=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"
        ),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        ),
        secure=parsed.scheme == "https",
    )
