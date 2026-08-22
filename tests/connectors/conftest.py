"""Shared fixtures for Arca connector integration tests."""

import os
import uuid

import boto3
import pytest


@pytest.fixture
def endpoint_url():
    """S3 endpoint URL for the Arca server."""
    return os.environ.get("ARCA_ENDPOINT", "http://arca:9000")


@pytest.fixture
def s3_client(endpoint_url):
    """boto3 S3 client configured to talk to Arca with valid credentials."""
    return boto3.client(
        "s3",
        endpoint_url=endpoint_url,
        aws_access_key_id=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"
        ),
        aws_secret_access_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
        ),
        region_name="us-east-1",
    )


@pytest.fixture
def unique_bucket(s3_client):
    """Create a unique test bucket and clean up after the test."""
    name = f"conn-test-{uuid.uuid4().hex[:8]}"
    s3_client.create_bucket(Bucket=name)
    yield name
    # Cleanup: remove all objects and delete bucket
    try:
        objs = s3_client.list_objects_v2(Bucket=name).get("Contents", [])
        for obj in objs:
            s3_client.delete_object(Bucket=name, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=name)
    except Exception:
        pass


def sigv4_request(method, url, data=None, headers=None, timeout=30):
    """Build a SigV4-signed HTTP request for admin API calls.

    Default timeout is 30s because some connector test endpoints (notably
    Kafka) wait for broker-side connection timeouts which can be 10-15s on
    unreachable hosts.
    """
    import hashlib
    import requests as req_lib
    import botocore.auth
    import botocore.credentials
    from botocore.awsrequest import AWSRequest

    access_key = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
    secret_key = os.environ.get(
        "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
    )
    credentials = botocore.credentials.Credentials(access_key, secret_key)

    body = data or b""
    if isinstance(body, str):
        body = body.encode()
    content_sha = hashlib.sha256(body).hexdigest()

    req_headers = {"x-amz-content-sha256": content_sha}
    if headers:
        req_headers.update(headers)

    aws_req = AWSRequest(method=method, url=url, data=body, headers=req_headers)
    botocore.auth.SigV4Auth(credentials, "s3", "us-east-1").add_auth(aws_req)

    fn = getattr(req_lib, method.lower())
    kwargs = {"headers": dict(aws_req.headers), "timeout": timeout}
    if data is not None:
        kwargs["data"] = body
    return fn(url, **kwargs)
