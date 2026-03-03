"""Integration tests for Arca Phase 2 — Bucket operations.

Tests CreateBucket, HeadBucket, ListBuckets, and DeleteBucket using boto3.
"""

import pytest
from botocore.exceptions import ClientError


class TestCreateBucket:
    def test_create_bucket(self, s3_client):
        """PUT /{bucket} should create a bucket and return 200."""
        s3_client.create_bucket(Bucket="test-create")

        # Verify it exists
        s3_client.head_bucket(Bucket="test-create")

        # Cleanup
        s3_client.delete_bucket(Bucket="test-create")

    def test_create_duplicate_bucket(self, s3_client):
        """Creating the same bucket twice should return BucketAlreadyOwnedByYou."""
        s3_client.create_bucket(Bucket="test-dupe")

        with pytest.raises(ClientError) as exc_info:
            s3_client.create_bucket(Bucket="test-dupe")

        assert exc_info.value.response["Error"]["Code"] == "BucketAlreadyOwnedByYou"

        # Cleanup
        s3_client.delete_bucket(Bucket="test-dupe")

    def test_create_invalid_name(self, s3_client):
        """Creating a bucket with an invalid name should return InvalidBucketName."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.create_bucket(Bucket="AB")

        assert exc_info.value.response["Error"]["Code"] == "InvalidBucketName"


class TestHeadBucket:
    def test_head_existing(self, s3_client):
        """HEAD /{bucket} should return 200 for an existing bucket."""
        s3_client.create_bucket(Bucket="test-head")
        s3_client.head_bucket(Bucket="test-head")

        # Cleanup
        s3_client.delete_bucket(Bucket="test-head")

    def test_head_nonexistent(self, s3_client):
        """HEAD /{bucket} should return 404 for a nonexistent bucket."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.head_bucket(Bucket="no-such-bucket")

        assert exc_info.value.response["Error"]["Code"] == "404"


class TestListBuckets:
    def test_list_empty(self, s3_client):
        """GET / should return an empty bucket list."""
        resp = s3_client.list_buckets()
        # Note: there may be buckets from other tests, so just check structure
        assert "Buckets" in resp
        assert "Owner" in resp

    def test_list_after_create(self, s3_client):
        """GET / should include a newly created bucket."""
        s3_client.create_bucket(Bucket="test-list-bucket")

        resp = s3_client.list_buckets()
        names = [b["Name"] for b in resp["Buckets"]]
        assert "test-list-bucket" in names

        # Cleanup
        s3_client.delete_bucket(Bucket="test-list-bucket")

    def test_list_xml_format(self, s3_client):
        """ListBuckets should return valid S3 XML with Owner and Buckets."""
        # If boto3 parses the response successfully, the XML is well-formed.
        result = s3_client.list_buckets()
        assert "Owner" in result
        assert "Buckets" in result
        assert "ID" in result["Owner"]
        assert "DisplayName" in result["Owner"]


class TestDeleteBucket:
    def test_delete_existing(self, s3_client):
        """DELETE /{bucket} should delete an existing bucket (204)."""
        s3_client.create_bucket(Bucket="test-delete")
        s3_client.delete_bucket(Bucket="test-delete")

        # Verify it's gone
        with pytest.raises(ClientError):
            s3_client.head_bucket(Bucket="test-delete")

    def test_delete_nonexistent(self, s3_client):
        """DELETE /{bucket} on a nonexistent bucket should return NoSuchBucket."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.delete_bucket(Bucket="no-such-bucket-to-delete")

        assert exc_info.value.response["Error"]["Code"] == "NoSuchBucket"
