"""Integration tests for bucket operations using the MinIO Python client.

Mirrors test_buckets.py but uses the minio_client fixture instead of boto3.
Bucket names use a "minio-" prefix to avoid collisions with boto3 tests.
"""

import pytest
from minio.error import S3Error


class TestCreateBucket:
    def test_create_bucket(self, minio_client):
        """make_bucket should create a bucket successfully."""
        minio_client.make_bucket("minio-test-create")

        # Verify it exists
        assert minio_client.bucket_exists("minio-test-create")

        # Cleanup
        minio_client.remove_bucket("minio-test-create")

    def test_create_duplicate_bucket(self, minio_client):
        """Creating the same bucket twice should succeed (idempotent)."""
        minio_client.make_bucket("minio-test-dupe")

        # Re-creating a bucket you own should not raise.
        minio_client.make_bucket("minio-test-dupe")

        # Cleanup
        minio_client.remove_bucket("minio-test-dupe")

    def test_create_invalid_name(self, minio_client):
        """Creating a bucket with an invalid name should raise an error.

        MinIO client validates bucket names locally and raises ValueError
        before sending the request, unlike boto3 which sends to the server.
        """
        with pytest.raises((S3Error, ValueError)):
            minio_client.make_bucket("AB")


class TestHeadBucket:
    def test_head_existing(self, minio_client):
        """bucket_exists should return True for an existing bucket."""
        minio_client.make_bucket("minio-test-head")

        assert minio_client.bucket_exists("minio-test-head") is True

        # Cleanup
        minio_client.remove_bucket("minio-test-head")

    def test_head_nonexistent(self, minio_client):
        """bucket_exists should return False for a nonexistent bucket."""
        assert minio_client.bucket_exists("minio-no-such-bucket") is False


class TestListBuckets:
    def test_list_empty(self, minio_client):
        """list_buckets should return a list (check structure)."""
        buckets = minio_client.list_buckets()
        # list_buckets returns a list of Bucket objects; verify it is a list
        assert isinstance(buckets, list)
        # Each bucket should have name and creation_date attributes
        for bucket in buckets:
            assert hasattr(bucket, "name")
            assert hasattr(bucket, "creation_date")

    def test_list_after_create(self, minio_client):
        """list_buckets should include a newly created bucket."""
        minio_client.make_bucket("minio-test-list-bucket")

        buckets = minio_client.list_buckets()
        names = [b.name for b in buckets]
        assert "minio-test-list-bucket" in names

        # Cleanup
        minio_client.remove_bucket("minio-test-list-bucket")

    @pytest.mark.skip(reason="MinIO client does not expose owner info from list_buckets")
    def test_list_has_owner_info(self, minio_client):
        """Skipped: MinIO Python client does not expose Owner in list_buckets."""
        pass


class TestDeleteBucket:
    def test_delete_existing(self, minio_client):
        """remove_bucket should delete an existing bucket."""
        minio_client.make_bucket("minio-test-delete")
        minio_client.remove_bucket("minio-test-delete")

        # Verify it's gone
        assert minio_client.bucket_exists("minio-test-delete") is False

    def test_delete_nonexistent(self, minio_client):
        """remove_bucket on a nonexistent bucket should raise S3Error with NoSuchBucket."""
        with pytest.raises(S3Error) as exc_info:
            minio_client.remove_bucket("minio-no-such-bucket-to-delete")

        assert exc_info.value.code == "NoSuchBucket"
