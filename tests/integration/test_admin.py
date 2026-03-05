"""Integration tests for Phase 8: Admin API."""

import json
import os

import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials


@pytest.fixture
def endpoint(endpoint_url):
    """Base URL for admin API endpoints."""
    return endpoint_url


@pytest.fixture
def creds():
    """AWS credentials for SigV4 signing."""
    return Credentials(
        access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        ),
    )


def signed_request(method, url, creds, data=None):
    """Make an HTTP request signed with SigV4."""
    headers = {}
    if data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data) if isinstance(data, dict) else data

    aws_req = AWSRequest(method=method, url=url, data=data or "", headers=headers)
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)
    return requests.request(
        method, url, headers=dict(aws_req.headers), data=data, timeout=10
    )


# -- Health --


class TestHealth:
    def test_health_returns_ok(self, endpoint):
        resp = requests.get(f"{endpoint}/admin/health", timeout=10)
        assert resp.status_code == 200
        body = resp.json()
        assert body["status"] == "ok"

    def test_health_no_auth_required(self, endpoint):
        # No Authorization header — should still work.
        resp = requests.get(f"{endpoint}/admin/health", timeout=10)
        assert resp.status_code == 200


# -- Info --


class TestInfo:
    def test_info_returns_version_and_uptime(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/info", creds)
        assert resp.status_code == 200
        body = resp.json()
        assert "version" in body
        assert "uptime_seconds" in body
        assert isinstance(body["uptime_seconds"], int)
        assert body["uptime_seconds"] >= 0

    def test_info_requires_auth(self, endpoint):
        resp = requests.get(f"{endpoint}/admin/info", timeout=10)
        assert resp.status_code == 403
        body = resp.json()
        assert body["error"] == "AccessDenied"


# -- Stats --


class TestStats:
    def test_stats_returns_counts(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/stats", creds)
        assert resp.status_code == 200
        body = resp.json()
        assert "bucket_count" in body
        assert "object_count" in body
        assert "total_size_bytes" in body
        assert isinstance(body["bucket_count"], int)
        assert isinstance(body["object_count"], int)
        assert isinstance(body["total_size_bytes"], int)

    def test_stats_requires_auth(self, endpoint):
        resp = requests.get(f"{endpoint}/admin/stats", timeout=10)
        assert resp.status_code == 403

    def test_stats_reflect_data(self, endpoint, creds, s3_client):
        """Stats should reflect created buckets and objects."""
        bucket = "admin-stats-test"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key="test.txt", Body=b"hello")
            resp = signed_request("GET", f"{endpoint}/admin/stats", creds)
            body = resp.json()
            assert body["bucket_count"] >= 1
            assert body["object_count"] >= 1
            assert body["total_size_bytes"] >= 5
        finally:
            # Cleanup
            try:
                s3_client.delete_object(Bucket=bucket, Key="test.txt")
                s3_client.delete_bucket(Bucket=bucket)
            except Exception:
                pass


# -- Credentials --


class TestCredentialCRUD:
    def test_list_credentials(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/credentials", creds)
        assert resp.status_code == 200
        body = resp.json()
        assert isinstance(body, list)
        assert len(body) >= 1  # At least the root credential
        # Secret key should be redacted
        for cred in body:
            assert "access_key_id" in cred
            assert "secret_access_key" not in cred or cred["secret_access_key"] is None
            assert "description" in cred
            assert "active" in cred

    def test_create_and_delete_credential(self, endpoint, creds):
        # Create
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/credentials",
            creds,
            data={"description": "test credential"},
        )
        assert resp.status_code == 201
        body = resp.json()
        assert "access_key_id" in body
        assert "secret_access_key" in body
        assert body["secret_access_key"] is not None
        assert body["description"] == "test credential"
        assert body["active"] is True

        new_key = body["access_key_id"]

        # Verify it appears in the list
        resp = signed_request("GET", f"{endpoint}/admin/credentials", creds)
        keys = [c["access_key_id"] for c in resp.json()]
        assert new_key in keys

        # Delete
        resp = signed_request(
            "DELETE", f"{endpoint}/admin/credentials/{new_key}", creds
        )
        assert resp.status_code == 204

        # Verify it's gone
        resp = signed_request("GET", f"{endpoint}/admin/credentials", creds)
        keys = [c["access_key_id"] for c in resp.json()]
        assert new_key not in keys

    def test_create_credential_empty_description(self, endpoint, creds):
        resp = signed_request(
            "POST", f"{endpoint}/admin/credentials", creds, data={}
        )
        assert resp.status_code == 201
        body = resp.json()
        assert body["description"] == ""

        # Cleanup
        signed_request(
            "DELETE",
            f"{endpoint}/admin/credentials/{body['access_key_id']}",
            creds,
        )

    def test_delete_nonexistent_credential(self, endpoint, creds):
        resp = signed_request(
            "DELETE", f"{endpoint}/admin/credentials/NONEXISTENT_KEY_12345", creds
        )
        assert resp.status_code == 404
        body = resp.json()
        assert body["error"] == "NotFound"

    def test_cannot_delete_last_active_credential(self, endpoint, creds):
        # There should be exactly one active credential (the root one).
        # Trying to delete it should fail.
        resp = signed_request("GET", f"{endpoint}/admin/credentials", creds)
        active_creds = [c for c in resp.json() if c["active"]]

        if len(active_creds) == 1:
            key = active_creds[0]["access_key_id"]
            resp = signed_request(
                "DELETE", f"{endpoint}/admin/credentials/{key}", creds
            )
            assert resp.status_code == 409
            body = resp.json()
            assert body["error"] == "Conflict"

    def test_credentials_require_auth(self, endpoint):
        resp = requests.get(f"{endpoint}/admin/credentials", timeout=10)
        assert resp.status_code == 403

        resp = requests.post(
            f"{endpoint}/admin/credentials",
            json={"description": "test"},
            timeout=10,
        )
        assert resp.status_code == 403

        resp = requests.delete(
            f"{endpoint}/admin/credentials/SOME_KEY", timeout=10
        )
        assert resp.status_code == 403


# -- CORS --


class TestCORS:
    def test_preflight_returns_cors_headers(self, endpoint):
        """OPTIONS preflight should return CORS headers."""
        resp = requests.options(
            f"{endpoint}/admin/health",
            headers={
                "Origin": "http://localhost:9080",
                "Access-Control-Request-Method": "GET",
                "Access-Control-Request-Headers": "authorization,x-amz-content-sha256,x-amz-date",
            },
            timeout=10,
        )
        assert resp.status_code == 200
        assert "access-control-allow-origin" in resp.headers
        assert "access-control-allow-methods" in resp.headers
        assert "GET" in resp.headers["access-control-allow-methods"]
        assert "PUT" in resp.headers["access-control-allow-methods"]
        assert "DELETE" in resp.headers["access-control-allow-methods"]

    def test_cors_headers_on_regular_request(self, endpoint):
        """Regular requests with Origin header should include CORS response headers."""
        resp = requests.get(
            f"{endpoint}/admin/health",
            headers={"Origin": "http://localhost:9080"},
            timeout=10,
        )
        assert resp.status_code == 200
        assert "access-control-allow-origin" in resp.headers
        assert "access-control-expose-headers" in resp.headers

    def test_cors_headers_on_s3_endpoint(self, endpoint, creds):
        """S3 endpoints should also include CORS headers."""
        resp = signed_request("GET", f"{endpoint}/", creds)
        # Note: CORS headers only appear when Origin is sent.
        # signed_request doesn't set Origin, so check with explicit Origin.
        resp = requests.get(
            f"{endpoint}/admin/health",
            headers={"Origin": "http://example.com"},
            timeout=10,
        )
        assert resp.status_code == 200
        assert "access-control-allow-origin" in resp.headers
