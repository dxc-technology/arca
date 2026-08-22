"""Integration tests for Phase 18: Monitoring, Metrics, Audit, and Settings."""

import json
import os
import time

import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials


@pytest.fixture
def endpoint(endpoint_url):
    """Base URL for API endpoints."""
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


# =============================================================================
# Prometheus Metrics
# =============================================================================


class TestPrometheusMetrics:
    def test_metrics_endpoint_returns_text(self, endpoint):
        """GET /admin/metrics returns Prometheus text format (no auth required)."""
        resp = requests.get(f"{endpoint}/admin/metrics", timeout=10)
        assert resp.status_code == 200
        assert "text/plain" in resp.headers.get("Content-Type", "")

    def test_metrics_contains_counters(self, endpoint, s3_client):
        """Prometheus metrics include request counters after S3 operations."""
        # Generate traffic
        s3_client.list_buckets()
        time.sleep(0.5)

        resp = requests.get(f"{endpoint}/admin/metrics", timeout=10)
        body = resp.text
        assert "arca_requests_total" in body
        assert 'operation="ListBuckets"' in body

    def test_metrics_contains_histograms(self, endpoint):
        """Prometheus metrics include latency histograms."""
        resp = requests.get(f"{endpoint}/admin/metrics", timeout=10)
        body = resp.text
        assert "arca_request_duration_ms" in body
        assert "le=" in body
        assert "+Inf" in body

    def test_metrics_contains_gauges(self, endpoint):
        """Prometheus metrics include gauge metrics."""
        resp = requests.get(f"{endpoint}/admin/metrics", timeout=10)
        body = resp.text
        assert "arca_active_connections" in body
        assert "arca_buckets_total" in body
        assert "arca_objects_total" in body
        assert "arca_storage_bytes_total" in body

    def test_metrics_no_auth_required(self, endpoint):
        """Prometheus metrics endpoint does not require authentication."""
        resp = requests.get(f"{endpoint}/admin/metrics", timeout=10)
        assert resp.status_code == 200


# =============================================================================
# Audit Log
# =============================================================================


class TestAuditLog:
    def test_audit_list_requires_auth(self, endpoint):
        """GET /admin/audit requires SigV4 authentication."""
        resp = requests.get(f"{endpoint}/admin/audit", timeout=10)
        assert resp.status_code in (401, 403)

    def test_audit_list_returns_entries(self, endpoint, creds, s3_client):
        """Audit log contains entries after S3 operations."""
        # Generate traffic
        s3_client.list_buckets()
        time.sleep(1)

        resp = signed_request("GET", f"{endpoint}/admin/audit?limit=5", creds)
        assert resp.status_code == 200
        data = resp.json()
        assert "entries" in data
        assert "total" in data
        assert data["total"] > 0
        assert len(data["entries"]) <= 5

    def test_audit_entry_fields(self, endpoint, creds, s3_client):
        """Audit entries contain expected fields."""
        s3_client.list_buckets()
        time.sleep(1)

        resp = signed_request("GET", f"{endpoint}/admin/audit?limit=1", creds)
        data = resp.json()
        assert len(data["entries"]) > 0
        entry = data["entries"][0]
        assert "timestamp" in entry
        assert "request_id" in entry
        assert "operation" in entry
        assert "http_method" in entry
        assert "http_status" in entry
        assert "duration_ms" in entry

    def test_audit_filter_by_operation(self, endpoint, creds, s3_client):
        """Audit log can be filtered by operation name."""
        s3_client.list_buckets()
        time.sleep(1)

        resp = signed_request(
            "GET", f"{endpoint}/admin/audit?operation=ListBuckets&limit=100", creds
        )
        data = resp.json()
        for entry in data["entries"]:
            assert entry["operation"] == "ListBuckets"

    def test_audit_filter_by_bucket(self, endpoint, creds, s3_client):
        """Audit log can be filtered by bucket name."""
        bucket = "audit-filter-test"
        try:
            s3_client.create_bucket(Bucket=bucket)
            s3_client.put_object(Bucket=bucket, Key="x.txt", Body=b"data")
            time.sleep(1)

            resp = signed_request(
                "GET", f"{endpoint}/admin/audit?bucket={bucket}&limit=100", creds
            )
            data = resp.json()
            assert data["total"] > 0
            for entry in data["entries"]:
                assert entry.get("bucket") == bucket
        finally:
            try:
                s3_client.delete_object(Bucket=bucket, Key="x.txt")
                s3_client.delete_bucket(Bucket=bucket)
            except Exception:
                pass

    def test_audit_pagination(self, endpoint, creds, s3_client):
        """Audit log supports offset/limit pagination."""
        # Generate a few entries
        for _ in range(3):
            s3_client.list_buckets()
        time.sleep(1)

        # Page 1
        resp1 = signed_request(
            "GET", f"{endpoint}/admin/audit?limit=2&offset=0", creds
        )
        data1 = resp1.json()
        assert len(data1["entries"]) == 2

        # Page 2
        resp2 = signed_request(
            "GET", f"{endpoint}/admin/audit?limit=2&offset=2", creds
        )
        data2 = resp2.json()
        assert len(data2["entries"]) > 0
        # Entries should be different (different timestamps/IDs)
        assert data1["entries"][0]["id"] != data2["entries"][0]["id"]

    def test_audit_stats(self, endpoint, creds):
        """GET /admin/audit/stats returns total count."""
        resp = signed_request("GET", f"{endpoint}/admin/audit/stats", creds)
        assert resp.status_code == 200
        data = resp.json()
        assert "total_entries" in data
        assert data["total_entries"] > 0


# =============================================================================
# Metrics History
# =============================================================================


class TestMetricsHistory:
    def test_metrics_history_requires_auth(self, endpoint):
        """GET /admin/metrics/history requires SigV4 authentication."""
        resp = requests.get(f"{endpoint}/admin/metrics/history", timeout=10)
        assert resp.status_code in (401, 403)

    def test_metrics_history_returns_snapshots(self, endpoint, creds):
        """Metrics history returns snapshot data."""
        resp = signed_request(
            "GET", f"{endpoint}/admin/metrics/history?limit=10", creds
        )
        assert resp.status_code == 200
        data = resp.json()
        assert "snapshots" in data
        assert "count" in data


# =============================================================================
# Instance Settings
# =============================================================================


class TestSettings:
    def test_settings_list(self, endpoint, creds):
        """GET /admin/settings returns all settings."""
        resp = signed_request("GET", f"{endpoint}/admin/settings", creds)
        assert resp.status_code == 200
        data = resp.json()
        assert "region" in data
        assert "audit_retention_days" in data
        assert "metrics_retention_days" in data
        assert "preview_max_size_mb" in data
        assert "preview_max_text_mb" in data
        assert "preview_max_video_mb" in data
        # Each setting has value, source, readonly
        for key in ("region", "audit_retention_days", "metrics_retention_days",
                     "preview_max_size_mb", "preview_max_text_mb", "preview_max_video_mb"):
            assert "value" in data[key]
            assert "source" in data[key]
            assert "readonly" in data[key]
        # Preview settings have correct defaults
        assert data["preview_max_size_mb"]["value"] == "10"
        assert data["preview_max_text_mb"]["value"] == "1"
        assert data["preview_max_video_mb"]["value"] == "100"

    def test_settings_update_region(self, endpoint, creds):
        """PUT /admin/settings/region updates the region."""
        try:
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/settings/region",
                creds,
                {"value": "ap-southeast-1"},
            )
            assert resp.status_code == 200
            data = resp.json()
            assert data["value"] == "ap-southeast-1"
            assert data["source"] == "database"

            # Verify it persisted
            resp = signed_request("GET", f"{endpoint}/admin/settings", creds)
            assert resp.json()["region"]["value"] == "ap-southeast-1"
        finally:
            # Reset
            signed_request("DELETE", f"{endpoint}/admin/settings/region", creds)

    def test_settings_delete_resets_to_default(self, endpoint, creds):
        """DELETE /admin/settings/{key} resets to default."""
        # Set, then delete
        signed_request(
            "PUT",
            f"{endpoint}/admin/settings/region",
            creds,
            {"value": "eu-north-1"},
        )
        resp = signed_request(
            "DELETE", f"{endpoint}/admin/settings/region", creds
        )
        assert resp.status_code == 204

        # Verify it's back to default
        resp = signed_request("GET", f"{endpoint}/admin/settings", creds)
        region = resp.json()["region"]
        assert region["source"] == "default"
        assert region["value"] == "us-east-1"

    def test_settings_unknown_key_rejected(self, endpoint, creds):
        """PUT /admin/settings/{unknown} returns 404."""
        resp = signed_request(
            "PUT",
            f"{endpoint}/admin/settings/nonexistent",
            creds,
            {"value": "foo"},
        )
        assert resp.status_code == 404

    def test_settings_invalid_region(self, endpoint, creds):
        """PUT /admin/settings/region rejects invalid format."""
        resp = signed_request(
            "PUT",
            f"{endpoint}/admin/settings/region",
            creds,
            {"value": "INVALID REGION!"},
        )
        assert resp.status_code == 400

    def test_settings_invalid_retention(self, endpoint, creds):
        """PUT /admin/settings/audit_retention_days rejects non-numeric."""
        resp = signed_request(
            "PUT",
            f"{endpoint}/admin/settings/audit_retention_days",
            creds,
            {"value": "abc"},
        )
        assert resp.status_code == 400

    def test_settings_retention_max_limit(self, endpoint, creds):
        """Retention days cannot exceed 3650."""
        resp = signed_request(
            "PUT",
            f"{endpoint}/admin/settings/audit_retention_days",
            creds,
            {"value": "9999"},
        )
        assert resp.status_code == 400

    def test_settings_update_retention(self, endpoint, creds):
        """Can set and reset retention days."""
        try:
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/settings/audit_retention_days",
                creds,
                {"value": "180"},
            )
            assert resp.status_code == 200
            assert resp.json()["value"] == "180"

            resp = signed_request("GET", f"{endpoint}/admin/settings", creds)
            assert resp.json()["audit_retention_days"]["value"] == "180"
            assert resp.json()["audit_retention_days"]["source"] == "database"
        finally:
            signed_request(
                "DELETE",
                f"{endpoint}/admin/settings/audit_retention_days",
                creds,
            )

    def test_settings_update_preview_limits(self, endpoint, creds):
        """Can set, verify, and reset preview size limits."""
        try:
            # Set preview_max_size_mb
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/settings/preview_max_size_mb",
                creds,
                {"value": "50"},
            )
            assert resp.status_code == 200
            assert resp.json()["value"] == "50"

            # Set preview_max_video_mb to 0 (unlimited)
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/settings/preview_max_video_mb",
                creds,
                {"value": "0"},
            )
            assert resp.status_code == 200
            assert resp.json()["value"] == "0"

            # Verify
            resp = signed_request("GET", f"{endpoint}/admin/settings", creds)
            data = resp.json()
            assert data["preview_max_size_mb"]["value"] == "50"
            assert data["preview_max_size_mb"]["source"] == "database"
            assert data["preview_max_video_mb"]["value"] == "0"
            assert data["preview_max_text_mb"]["source"] == "default"
        finally:
            signed_request(
                "DELETE", f"{endpoint}/admin/settings/preview_max_size_mb", creds
            )
            signed_request(
                "DELETE", f"{endpoint}/admin/settings/preview_max_video_mb", creds
            )

    def test_settings_preview_rejects_invalid(self, endpoint, creds):
        """Preview settings reject non-numeric and out-of-range values."""
        resp = signed_request(
            "PUT",
            f"{endpoint}/admin/settings/preview_max_size_mb",
            creds,
            {"value": "abc"},
        )
        assert resp.status_code == 400

        resp = signed_request(
            "PUT",
            f"{endpoint}/admin/settings/preview_max_text_mb",
            creds,
            {"value": "99999"},
        )
        assert resp.status_code == 400

    def test_settings_requires_auth(self, endpoint):
        """GET /admin/settings requires SigV4 authentication."""
        resp = requests.get(f"{endpoint}/admin/settings", timeout=10)
        assert resp.status_code in (401, 403)


# =============================================================================
# Region in S3 responses (TD-004 resolution)
# =============================================================================


class TestRegion:
    def test_head_bucket_returns_default_region(self, endpoint, s3_client, creds):
        """HeadBucket returns us-east-1 by default."""
        bucket = "region-head-test"
        try:
            s3_client.create_bucket(Bucket=bucket)
            resp = s3_client.head_bucket(Bucket=bucket)
            region = resp["ResponseMetadata"]["HTTPHeaders"].get(
                "x-amz-bucket-region"
            )
            assert region == "us-east-1"
        finally:
            try:
                s3_client.delete_bucket(Bucket=bucket)
            except Exception:
                pass

    def test_head_bucket_returns_custom_region(self, endpoint, s3_client, creds):
        """HeadBucket returns region set via admin settings."""
        bucket = "region-custom-test"
        try:
            s3_client.create_bucket(Bucket=bucket)
            signed_request(
                "PUT",
                f"{endpoint}/admin/settings/region",
                creds,
                {"value": "eu-west-1"},
            )
            resp = s3_client.head_bucket(Bucket=bucket)
            region = resp["ResponseMetadata"]["HTTPHeaders"].get(
                "x-amz-bucket-region"
            )
            assert region == "eu-west-1"
        finally:
            signed_request(
                "DELETE", f"{endpoint}/admin/settings/region", creds
            )
            try:
                s3_client.delete_bucket(Bucket=bucket)
            except Exception:
                pass

    def test_get_bucket_location_default(self, endpoint, s3_client, creds):
        """GetBucketLocation returns None (us-east-1) by default."""
        bucket = "region-location-test"
        try:
            s3_client.create_bucket(Bucket=bucket)
            resp = s3_client.get_bucket_location(Bucket=bucket)
            # boto3 returns None for us-east-1 (empty LocationConstraint)
            assert resp["LocationConstraint"] is None
        finally:
            try:
                s3_client.delete_bucket(Bucket=bucket)
            except Exception:
                pass

    def test_get_bucket_location_custom(self, endpoint, s3_client, creds):
        """GetBucketLocation returns region set via admin settings."""
        bucket = "region-loc-custom-test"
        try:
            s3_client.create_bucket(Bucket=bucket)
            signed_request(
                "PUT",
                f"{endpoint}/admin/settings/region",
                creds,
                {"value": "ap-northeast-1"},
            )
            resp = s3_client.get_bucket_location(Bucket=bucket)
            assert resp["LocationConstraint"] == "ap-northeast-1"
        finally:
            signed_request(
                "DELETE", f"{endpoint}/admin/settings/region", creds
            )
            try:
                s3_client.delete_bucket(Bucket=bucket)
            except Exception:
                pass
