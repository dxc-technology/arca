"""Shared fixtures for Arca integration tests."""

import os

import boto3
import pytest


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
