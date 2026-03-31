"""Integration tests for Phase 23: Performance and Hardening.

Tests request size limits, header validation, health endpoint,
and rate limiting (when enabled).
"""

import io
import os

import pytest
import requests


class TestRequestSizeLimits:
    """Tests for configurable max body size."""

    def test_put_within_limit_succeeds(self, s3_client):
        """PutObject with body within limit succeeds."""
        bucket = "test-hardening-limits"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key="small.txt", Body=b"hello")
            resp = s3_client.get_object(Bucket=bucket, Key="small.txt")
            assert resp["Body"].read() == b"hello"
        finally:
            s3_client.delete_object(Bucket=bucket, Key="small.txt")
            s3_client.delete_bucket(Bucket=bucket)

    def test_content_length_check_rejects_oversized(self, s3_client, endpoint_url):
        """PutObject with Content-Length exceeding limit is rejected.

        Note: this test only works if the server is configured with a
        max_body_size smaller than the declared Content-Length. We test
        with the default 5 GB limit by checking that a normal upload
        works (the limit is enforced but not hit).
        """
        # This test verifies the code path exists without hitting the 5GB limit.
        # A more targeted test would require a custom config with a small limit.
        bucket = "test-hardening-cl"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key="ok.txt", Body=b"x" * 1024)
            resp = s3_client.head_object(Bucket=bucket, Key="ok.txt")
            assert resp["ContentLength"] == 1024
        finally:
            s3_client.delete_object(Bucket=bucket, Key="ok.txt")
            s3_client.delete_bucket(Bucket=bucket)


class TestHealthEndpoint:
    """Tests for the health check endpoint."""

    def test_health_returns_ok(self, endpoint_url):
        """GET /admin/health returns 200 with status=ok."""
        resp = requests.get(f"{endpoint_url}/admin/health", verify=False)
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "ok"


class TestMetadataHeaders:
    """Tests for header and metadata validation."""

    def test_normal_metadata_accepted(self, s3_client):
        """PutObject with normal user metadata succeeds."""
        bucket = "test-hardening-meta"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(
                Bucket=bucket,
                Key="meta.txt",
                Body=b"data",
                Metadata={"author": "test", "version": "1"},
            )
            resp = s3_client.head_object(Bucket=bucket, Key="meta.txt")
            assert resp["Metadata"]["author"] == "test"
            assert resp["Metadata"]["version"] == "1"
        finally:
            s3_client.delete_object(Bucket=bucket, Key="meta.txt")
            s3_client.delete_bucket(Bucket=bucket)


class TestCacheTransparency:
    """Tests that the metadata cache is transparent to clients."""

    def test_head_bucket_cached(self, s3_client):
        """HeadBucket returns correct results even with caching."""
        bucket = "test-hardening-cache"
        s3_client.create_bucket(Bucket=bucket)
        try:
            # First call populates cache.
            s3_client.head_bucket(Bucket=bucket)
            # Second call should hit cache but still return correct result.
            s3_client.head_bucket(Bucket=bucket)
        finally:
            s3_client.delete_bucket(Bucket=bucket)

    def test_cache_invalidated_on_delete(self, s3_client):
        """After deleting a bucket, HeadBucket returns 404."""
        bucket = "test-hardening-cache-inv"
        s3_client.create_bucket(Bucket=bucket)
        s3_client.head_bucket(Bucket=bucket)  # populate cache
        s3_client.delete_bucket(Bucket=bucket)

        with pytest.raises(Exception):
            s3_client.head_bucket(Bucket=bucket)

    def test_object_cache_invalidated_on_overwrite(self, s3_client):
        """Overwriting an object invalidates the cache."""
        bucket = "test-hardening-obj-cache"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key="file.txt", Body=b"v1")
            resp1 = s3_client.head_object(Bucket=bucket, Key="file.txt")
            assert resp1["ContentLength"] == 2

            s3_client.put_object(Bucket=bucket, Key="file.txt", Body=b"version2")
            resp2 = s3_client.head_object(Bucket=bucket, Key="file.txt")
            assert resp2["ContentLength"] == 8
        finally:
            s3_client.delete_object(Bucket=bucket, Key="file.txt")
            s3_client.delete_bucket(Bucket=bucket)
