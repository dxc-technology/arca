"""Integration tests for virtual-hosted-style addressing (TD-037).

With `[server] domain` set, `Host: {bucket}.{domain}` addresses the bucket and
the path is the key. The rewrite used to run after routing, so
`{bucket}.{domain}/key` was dispatched as bucket=`key`.

Runs only against a server started with a domain, which `bin/test
virtual-host` does and exports as ARCA_VIRTUAL_HOST_DOMAIN. Requests are
signed for the virtual-hosted URL and sent to the endpoint with that Host.
"""

import os
import uuid
from urllib.parse import urlparse

import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

DOMAIN = os.environ.get("ARCA_VIRTUAL_HOST_DOMAIN")

pytestmark = pytest.mark.skipif(
    not DOMAIN, reason="needs a server with [server] domain (bin/test virtual-host)"
)

ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
SECRET_KEY = os.environ.get(
    "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
)


@pytest.fixture
def bucket(s3_client):
    name = f"test-vhost-{uuid.uuid4().hex[:8]}"
    s3_client.create_bucket(Bucket=name)
    yield name
    for obj in s3_client.list_objects_v2(Bucket=name).get("Contents", []):
        s3_client.delete_object(Bucket=name, Key=obj["Key"])
    s3_client.delete_bucket(Bucket=name)


@pytest.fixture
def vhost(endpoint_url):
    """Sends a signed virtual-hosted-style request: vhost(method, bucket, path)."""

    def send(method, bucket, path="/", body=b"", query=""):
        host = f"{bucket}.{DOMAIN}"
        url = f"http://{host}{path}" + (f"?{query}" if query else "")
        signed = AWSRequest(method=method, url=url, data=body)
        S3SigV4Auth(Credentials(ACCESS_KEY, SECRET_KEY), "s3", "us-east-1").add_auth(signed)
        headers = dict(signed.headers.items())
        headers["Host"] = host
        target = urlparse(endpoint_url)
        return requests.request(
            method,
            f"{target.scheme}://{target.netloc}{path}" + (f"?{query}" if query else ""),
            headers=headers,
            data=body,
        )

    return send


class TestVirtualHostedStyle:
    def test_put_stores_the_key_in_the_bucket(self, s3_client, bucket, vhost):
        r = vhost("PUT", bucket, "/hello.txt", b"virtual")
        assert r.status_code == 200, r.text
        got = s3_client.get_object(Bucket=bucket, Key="hello.txt")["Body"].read()
        assert got == b"virtual"

    def test_get_reads_a_key_written_path_style(self, s3_client, bucket, vhost):
        s3_client.put_object(Bucket=bucket, Key="a/b/c.txt", Body=b"nested")
        r = vhost("GET", bucket, "/a/b/c.txt")
        assert r.status_code == 200, r.text
        assert r.content == b"nested"

    def test_head_and_delete(self, s3_client, bucket, vhost):
        s3_client.put_object(Bucket=bucket, Key="k", Body=b"x")
        assert vhost("HEAD", bucket, "/k").status_code == 200
        assert vhost("DELETE", bucket, "/k").status_code == 204
        assert vhost("HEAD", bucket, "/k").status_code == 404

    def test_bucket_root_lists_the_bucket_not_all_buckets(self, s3_client, bucket, vhost):
        s3_client.put_object(Bucket=bucket, Key="listed", Body=b"x")
        r = vhost("GET", bucket, "/", query="list-type=2")
        assert r.status_code == 200, r.text
        assert "<Key>listed</Key>" in r.text
        assert "<ListAllMyBucketsResult" not in r.text

    def test_query_string_survives_the_rewrite(self, s3_client, bucket, vhost):
        for key in ("p/1", "p/2", "q/1"):
            s3_client.put_object(Bucket=bucket, Key=key, Body=b"x")
        r = vhost("GET", bucket, "/", query="list-type=2&prefix=p%2F")
        assert r.status_code == 200, r.text
        assert "<Key>p/1</Key>" in r.text and "<Key>q/1</Key>" not in r.text

    def test_a_missing_bucket_is_no_such_bucket(self, vhost):
        r = vhost("GET", f"no-such-{uuid.uuid4().hex[:8]}", "/k")
        assert r.status_code == 404
        assert "<Code>NoSuchBucket</Code>" in r.text
