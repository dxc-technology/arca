"""Integration tests for Phase 2 — S3 compatibility fixes via MinIO client.

Mirrors test_phase2.py but uses the minio_client fixture instead of boto3.
Tests range requests (suffix, open-ended, clamped) and copy-to-self.
Conditional headers are skipped because the MinIO Python client does not
expose If-Match / If-None-Match / If-Modified-Since parameters.
"""

import io

import pytest
from minio.commonconfig import CopySource
from minio.error import S3Error


BUCKET = "minio-test-phase2-bucket"


def cleanup_bucket(minio_client, bucket_name):
    """Remove all objects from a bucket and then the bucket itself."""
    try:
        objects = minio_client.list_objects(bucket_name, recursive=True)
        for obj in objects:
            minio_client.remove_object(bucket_name, obj.object_name)
        minio_client.remove_bucket(bucket_name)
    except S3Error:
        pass


@pytest.fixture(autouse=True)
def setup_bucket(minio_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        minio_client.make_bucket(BUCKET)
    except S3Error:
        pass
    yield
    cleanup_bucket(minio_client, BUCKET)


class TestRangeRequests:
    """Tests for Range header variants via MinIO client offset/length."""

    def test_suffix_range(self, minio_client):
        """offset=6, length=4 on '0123456789' should return '6789'."""
        data = b"0123456789"
        minio_client.put_object(
            BUCKET, "range", io.BytesIO(data), len(data),
        )

        resp = minio_client.get_object(BUCKET, "range", offset=6, length=4)
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == b"6789"

    def test_open_ended_range(self, minio_client):
        """offset=7 without length should return from byte 7 to end."""
        data = b"0123456789"
        minio_client.put_object(
            BUCKET, "range", io.BytesIO(data), len(data),
        )

        resp = minio_client.get_object(BUCKET, "range", offset=7)
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == b"789"

    def test_range_past_end_clamped(self, minio_client):
        """Length extending past file size should be clamped to last byte."""
        data = b"0123456789"
        minio_client.put_object(
            BUCKET, "range", io.BytesIO(data), len(data),
        )

        # Request 995 bytes starting at offset 5 — only 5 bytes remain.
        resp = minio_client.get_object(BUCKET, "range", offset=5, length=995)
        try:
            body = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert body == b"56789"


class TestCopyToSelf:
    """Tests for copy-to-self validation."""

    def test_copy_to_self_without_replace_fails(self, minio_client):
        """Copying object to itself without REPLACE directive should fail."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "self-copy", io.BytesIO(data), len(data),
        )

        with pytest.raises(S3Error) as exc_info:
            minio_client.copy_object(
                BUCKET, "self-copy", CopySource(BUCKET, "self-copy"),
            )
        assert exc_info.value.code == "InvalidRequest"

    def test_copy_to_self_with_replace_succeeds(self, minio_client):
        """Copying object to itself with REPLACE directive should succeed."""
        data = b"data"
        minio_client.put_object(
            BUCKET, "self-copy", io.BytesIO(data), len(data),
            content_type="text/plain",
        )

        minio_client.copy_object(
            BUCKET,
            "self-copy",
            CopySource(BUCKET, "self-copy"),
            metadata={"replaced": "yes"},
            metadata_directive="REPLACE",
        )
        # Should succeed (no exception).


class TestConditionalHeaders:
    """Skipped — MinIO Python client does not expose conditional header
    parameters (If-Match, If-None-Match, If-Modified-Since,
    If-Unmodified-Since) on get_object or stat_object. These are
    covered by the boto3 tests in test_phase2.py."""

    @pytest.mark.skip(
        reason="MinIO Python client does not expose conditional header parameters"
    )
    def test_get_if_match_success(self, minio_client):
        pass

    @pytest.mark.skip(
        reason="MinIO Python client does not expose conditional header parameters"
    )
    def test_get_if_match_fails_412(self, minio_client):
        pass

    @pytest.mark.skip(
        reason="MinIO Python client does not expose conditional header parameters"
    )
    def test_get_if_none_match_304(self, minio_client):
        pass
