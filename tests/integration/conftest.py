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
    """boto3 S3 client configured to talk to Arca."""
    return boto3.client(
        "s3",
        endpoint_url=endpoint_url,
        aws_access_key_id="test",
        aws_secret_access_key="test",
        region_name="us-east-1",
    )
