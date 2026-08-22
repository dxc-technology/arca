"""Integration tests for configuration export/import."""

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


def unique(prefix="test"):
    return f"{prefix}-{uuid.uuid4().hex[:8]}"


# -- Export --


class TestExport:
    def test_export_all_sections(self, endpoint, creds):
        """Export all sections returns expected structure."""
        resp = signed_request("GET", f"{endpoint}/admin/export", creds)
        assert resp.status_code == 200
        body = resp.json()
        assert "arca_export" in body
        assert "version" in body["arca_export"]
        assert "exported_at" in body["arca_export"]
        sections = body["arca_export"]["sections"]
        assert "settings" in sections
        assert "auth" in sections
        assert "credentials" in sections
        assert "buckets" in sections
        assert "bucket_configs" in sections

    def test_export_single_section(self, endpoint, creds):
        """Export only settings section."""
        resp = signed_request(
            "GET", f"{endpoint}/admin/export?sections=settings", creds
        )
        assert resp.status_code == 200
        body = resp.json()
        assert "settings" in body
        assert "users" not in body
        assert "credentials" not in body
        assert "buckets" not in body

    def test_export_multiple_sections(self, endpoint, creds):
        """Export settings and buckets only."""
        resp = signed_request(
            "GET", f"{endpoint}/admin/export?sections=settings%2Cbuckets", creds
        )
        assert resp.status_code == 200
        body = resp.json()
        assert "settings" in body
        assert "buckets" in body
        assert "users" not in body
        assert "credentials" not in body

    def test_export_invalid_section(self, endpoint, creds):
        """Unknown section name returns 400."""
        resp = signed_request(
            "GET", f"{endpoint}/admin/export?sections=nonsense", creds
        )
        assert resp.status_code == 400

    def test_export_secrets_masked_by_default(self, endpoint, creds):
        """Credentials have masked secrets by default."""
        resp = signed_request(
            "GET", f"{endpoint}/admin/export?sections=credentials", creds
        )
        assert resp.status_code == 200
        body = resp.json()
        assert "credentials" in body
        for c in body["credentials"]:
            assert c["secret_access_key"] == "****"

    def test_export_secrets_included(self, endpoint, creds):
        """include_secrets=true shows real secret keys."""
        resp = signed_request(
            "GET",
            f"{endpoint}/admin/export?sections=credentials&include_secrets=true",
            creds,
        )
        assert resp.status_code == 200
        body = resp.json()
        assert "credentials" in body
        assert len(body["credentials"]) > 0
        for c in body["credentials"]:
            assert c["secret_access_key"] != "****"
            assert len(c["secret_access_key"]) > 4

    def test_export_auth_includes_users_teams_grants(self, endpoint, creds):
        """Auth section includes users, teams, and grants arrays."""
        resp = signed_request(
            "GET", f"{endpoint}/admin/export?sections=auth", creds
        )
        assert resp.status_code == 200
        body = resp.json()
        assert "users" in body
        assert "teams" in body
        assert "grants" in body
        assert isinstance(body["users"], list)
        assert isinstance(body["teams"], list)
        assert isinstance(body["grants"], list)

    def test_export_bucket_configs(self, endpoint, creds, s3_client):
        """Bucket configs export includes per-bucket settings."""
        bucket = unique("exp")
        s3_client.create_bucket(Bucket=bucket)
        try:
            resp = signed_request(
                "GET",
                f"{endpoint}/admin/export?sections=bucket_configs",
                creds,
            )
            assert resp.status_code == 200
            body = resp.json()
            assert "bucket_configs" in body
            assert isinstance(body["bucket_configs"], dict)
        finally:
            s3_client.delete_bucket(Bucket=bucket)

    def test_export_requires_auth(self, endpoint):
        """Export requires authentication."""
        resp = requests.get(f"{endpoint}/admin/export", timeout=10)
        assert resp.status_code == 403


# -- Import --


class TestImport:
    def test_import_dry_run(self, endpoint, creds):
        """Dry run validates but does not apply changes."""
        payload = {
            "arca_export": {"version": "test", "sections": ["settings"]},
            "settings": {"region": "test-region-dry"},
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=dry_run",
            creds,
            data=payload,
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["mode"] == "dry_run"
        assert "settings" in body["results"]

    def test_import_settings_skip(self, endpoint, creds):
        """Import settings in skip mode does not overwrite existing values."""
        # First, set a known value
        signed_request(
            "PUT",
            f"{endpoint}/admin/settings/region",
            creds,
            data={"value": "us-east-1"},
        )

        payload = {
            "settings": {"region": "imported-region"},
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=skip",
            creds,
            data=payload,
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["results"]["settings"]["skipped"] >= 1

    def test_import_settings_overwrite(self, endpoint, creds):
        """Import settings in overwrite mode replaces existing values."""
        payload = {
            "settings": {"preview_max_size_mb": "42"},
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=overwrite",
            creds,
            data=payload,
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["results"]["settings"]["applied"] >= 1

        # Verify the value was applied
        settings_resp = signed_request(
            "GET", f"{endpoint}/admin/settings", creds
        )
        settings = settings_resp.json()
        assert settings["preview_max_size_mb"]["value"] == "42"

        # Clean up: reset to default
        signed_request(
            "DELETE", f"{endpoint}/admin/settings/preview_max_size_mb", creds
        )

    def test_import_skips_node_local_settings(self, endpoint, creds):
        """node_id is node identity: import must refuse it even in overwrite
        mode, and a subsequent export must not contain it (D12.2)."""
        payload = {
            "settings": {"node_id": "imported-evil-node-id"},
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=overwrite",
            creds,
            data=payload,
        )
        assert resp.status_code == 200
        body = resp.json()
        # Zero-valued counters are omitted from the JSON.
        assert body["results"]["settings"]["skipped"] >= 1
        assert body["results"]["settings"].get("applied", 0) == 0

        export = signed_request(
            "GET", f"{endpoint}/admin/export?sections=settings", creds
        )
        assert export.status_code == 200
        assert "node_id" not in export.json()["settings"]

    def test_import_invalid_mode(self, endpoint, creds):
        """Invalid mode returns 400."""
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=invalid",
            creds,
            data={"settings": {}},
        )
        assert resp.status_code == 400

    def test_import_masked_credentials_skipped(self, endpoint, creds):
        """Credentials with masked secrets are skipped with a note."""
        payload = {
            "credentials": [
                {
                    "access_key_id": "AKIATEST12345678",
                    "secret_access_key": "****",
                    "description": "masked cred",
                    "user_id": "u-root",
                    "active": True,
                }
            ]
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=skip",
            creds,
            data=payload,
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["results"]["credentials"]["skipped"] >= 1
        # Should have a note about masked secret
        masked_errors = [e for e in body["errors"] if "masked" in e.lower()]
        assert len(masked_errors) >= 1

    def test_import_creates_bucket(self, endpoint, creds, s3_client):
        """Import can create a new bucket."""
        bucket = unique("imp")
        payload = {
            "buckets": [{"name": bucket, "created_at": "2026-01-01T00:00:00Z", "owner": "admin"}]
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=skip",
            creds,
            data=payload,
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["results"]["buckets"]["created"] == 1

        # Verify bucket exists
        head = s3_client.head_bucket(Bucket=bucket)
        assert head["ResponseMetadata"]["HTTPStatusCode"] == 200

        # Clean up
        s3_client.delete_bucket(Bucket=bucket)

    def test_import_skips_existing_bucket(self, endpoint, creds, s3_client):
        """Import in skip mode does not error on existing buckets."""
        bucket = unique("imp-skip")
        s3_client.create_bucket(Bucket=bucket)
        try:
            payload = {
                "buckets": [{"name": bucket, "created_at": "2026-01-01T00:00:00Z", "owner": "admin"}]
            }
            resp = signed_request(
                "POST",
                f"{endpoint}/admin/import?mode=skip",
                creds,
                data=payload,
            )
            assert resp.status_code == 200
            body = resp.json()
            assert body["results"]["buckets"]["skipped"] == 1
        finally:
            s3_client.delete_bucket(Bucket=bucket)

    def test_import_requires_auth(self, endpoint):
        """Import requires authentication."""
        resp = requests.post(
            f"{endpoint}/admin/import",
            json={"settings": {}},
            timeout=10,
        )
        assert resp.status_code == 403


# -- Round-trip --


class TestRoundTrip:
    def test_export_import_round_trip(self, endpoint, creds, s3_client):
        """Export then import on same instance (dry_run) validates the format."""
        # Export everything with secrets
        export_resp = signed_request(
            "GET",
            f"{endpoint}/admin/export?include_secrets=true",
            creds,
        )
        assert export_resp.status_code == 200
        exported = export_resp.json()

        # Import as dry_run
        import_resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=dry_run",
            creds,
            data=exported,
        )
        assert import_resp.status_code == 200
        body = import_resp.json()
        assert body["mode"] == "dry_run"
        # No errors in a clean round-trip
        real_errors = [e for e in body["errors"] if "masked" not in e.lower()]
        assert len(real_errors) == 0, f"Unexpected errors: {real_errors}"

    def test_export_without_secrets_import_dry_run(self, endpoint, creds):
        """Export without secrets, import dry_run: credentials are all skipped."""
        export_resp = signed_request(
            "GET",
            f"{endpoint}/admin/export",
            creds,
        )
        assert export_resp.status_code == 200
        exported = export_resp.json()

        import_resp = signed_request(
            "POST",
            f"{endpoint}/admin/import?mode=dry_run",
            creds,
            data=exported,
        )
        assert import_resp.status_code == 200
        body = import_resp.json()
        # All credentials should be skipped (masked)
        if "credentials" in body["results"]:
            assert body["results"]["credentials"].get("created", 0) == 0
