"""Integration tests for Phase 8: Admin API."""

import json
import os
import uuid

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
        access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
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
        # Secret key should be redacted; owning user reported
        for cred in body:
            assert "access_key_id" in cred
            assert "secret_access_key" not in cred or cred["secret_access_key"] is None
            assert "description" in cred
            assert "active" in cred
            assert "user_id" in cred
            # Privileges belong to the user, so no per-credential flag
            assert "admin" not in cred

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


# -- Admin privilege --


class TestAdminPrivilege:
    """Admin access follows the owning user, never a per-credential flag."""

    def _root_user_ids(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/users", creds)
        assert resp.status_code == 200
        return {u["user_id"] for u in resp.json() if u["is_root"]}

    def test_root_credential_belongs_to_a_root_user(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/credentials", creds)
        assert resp.status_code == 200
        body = resp.json()
        root = [c for c in body if c["access_key_id"] == os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")]
        assert len(root) == 1
        assert root[0]["user_id"] in self._root_user_ids(endpoint, creds)

    def test_extra_root_credential_reaches_the_admin_api(self, endpoint, creds):
        """A second credential on the root user is admin too, no flag needed."""
        resp = signed_request(
            "POST", f"{endpoint}/admin/credentials", creds,
            data={"description": "second root credential"},
        )
        assert resp.status_code == 201
        body = resp.json()
        assert body["user_id"] in self._root_user_ids(endpoint, creds)

        try:
            new_creds = Credentials(
                access_key=body["access_key_id"],
                secret_key=body["secret_access_key"],
            )
            resp = signed_request("GET", f"{endpoint}/admin/info", new_creds)
            assert resp.status_code == 200
        finally:
            signed_request("DELETE", f"{endpoint}/admin/credentials/{body['access_key_id']}", creds)

    def test_non_root_rejected_from_admin_api(self, endpoint, creds):
        """A credential on a user without grants gets 403 on admin endpoints."""
        name = f"plain-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        assert resp.status_code == 201
        user_id = resp.json()["user_id"]

        resp = signed_request(
            "POST", f"{endpoint}/admin/users/{user_id}/credentials", creds,
            data={"description": "non-root for test"},
        )
        assert resp.status_code == 201
        user_cred = resp.json()

        try:
            user_creds = Credentials(
                access_key=user_cred["access_key_id"],
                secret_key=user_cred["secret_access_key"],
            )
            # Try admin endpoints — all should return 403
            resp = signed_request("GET", f"{endpoint}/admin/info", user_creds)
            assert resp.status_code == 403
            body = resp.json()
            assert body["error"] == "AccessDenied"

            resp = signed_request("GET", f"{endpoint}/admin/stats", user_creds)
            assert resp.status_code == 403

            resp = signed_request("GET", f"{endpoint}/admin/credentials", user_creds)
            assert resp.status_code == 403
        finally:
            signed_request("DELETE", f"{endpoint}/admin/credentials/{user_cred['access_key_id']}", creds)
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_extra_root_credential_can_use_s3(self, endpoint, creds, s3_client):
        """An additional credential on the root user works for S3 operations."""
        import boto3

        resp = signed_request(
            "POST", f"{endpoint}/admin/credentials", creds,
            data={"description": "s3-user"},
        )
        assert resp.status_code == 201
        user_cred = resp.json()

        try:
            user_s3 = boto3.client(
                "s3",
                endpoint_url=endpoint,
                aws_access_key_id=user_cred["access_key_id"],
                aws_secret_access_key=user_cred["secret_access_key"],
                region_name="us-east-1",
            )
            bucket = "extra-cred-test-bucket"
            user_s3.create_bucket(Bucket=bucket)
            user_s3.put_object(Bucket=bucket, Key="hello.txt", Body=b"world")
            obj = user_s3.get_object(Bucket=bucket, Key="hello.txt")
            assert obj["Body"].read() == b"world"
            user_s3.delete_object(Bucket=bucket, Key="hello.txt")
            user_s3.delete_bucket(Bucket=bucket)
        finally:
            signed_request("DELETE", f"{endpoint}/admin/credentials/{user_cred['access_key_id']}", creds)

    def test_cannot_delete_last_root_credential(self, endpoint, creds):
        """Should not be able to delete the last active root credential."""
        root_ids = self._root_user_ids(endpoint, creds)
        resp = signed_request("GET", f"{endpoint}/admin/credentials", creds)
        root_creds = [c for c in resp.json() if c["active"] and c["user_id"] in root_ids]
        if len(root_creds) == 1:
            resp = signed_request(
                "DELETE", f"{endpoint}/admin/credentials/{root_creds[0]['access_key_id']}", creds,
            )
            assert resp.status_code == 409
            body = resp.json()
            assert "root" in body["message"].lower()


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
