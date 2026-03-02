"""Integration tests for Arca Phase 4 — ListObjectsV2.

Tests listing objects with prefix, delimiter, pagination, and continuation tokens.
"""

import pytest
from botocore.exceptions import ClientError


BUCKET = "test-list-bucket"


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass
    yield
    try:
        response = s3_client.list_objects_v2(Bucket=BUCKET)
        for obj in response.get("Contents", []):
            s3_client.delete_object(Bucket=BUCKET, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass


def put_objects(s3_client, keys):
    """Helper to put multiple empty objects."""
    for key in keys:
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"x")


class TestListBasic:
    def test_list_empty_bucket(self, s3_client):
        """Listing an empty bucket should return no contents."""
        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        assert resp["KeyCount"] == 0
        assert resp["IsTruncated"] is False
        assert "Contents" not in resp

    def test_list_basic(self, s3_client):
        """Listing a bucket with objects should return them sorted by key."""
        put_objects(s3_client, ["c.txt", "a.txt", "b.txt"])

        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        keys = [obj["Key"] for obj in resp["Contents"]]
        assert keys == ["a.txt", "b.txt", "c.txt"]
        assert resp["KeyCount"] == 3
        assert resp["IsTruncated"] is False

    def test_list_nonexistent_bucket(self, s3_client):
        """Listing a nonexistent bucket should return NoSuchBucket."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.list_objects_v2(Bucket="nonexistent-list-bucket")
        assert exc_info.value.response["Error"]["Code"] == "NoSuchBucket"

    def test_content_fields(self, s3_client):
        """Each object in Contents should have Key, Size, ETag, LastModified, StorageClass."""
        data = b"hello world"
        s3_client.put_object(Bucket=BUCKET, Key="fields.txt", Body=data)

        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        obj = resp["Contents"][0]
        assert obj["Key"] == "fields.txt"
        assert obj["Size"] == len(data)
        assert "ETag" in obj
        assert "LastModified" in obj
        assert obj["StorageClass"] == "STANDARD"


class TestPrefix:
    def test_prefix_filter(self, s3_client):
        """Prefix filter should only return keys starting with the prefix."""
        put_objects(s3_client, ["photos/a.jpg", "photos/b.jpg", "videos/c.mp4"])

        resp = s3_client.list_objects_v2(Bucket=BUCKET, Prefix="photos/")
        keys = [obj["Key"] for obj in resp["Contents"]]
        assert keys == ["photos/a.jpg", "photos/b.jpg"]

    def test_prefix_no_match(self, s3_client):
        """Prefix with no matching keys should return empty result."""
        put_objects(s3_client, ["a.txt", "b.txt"])

        resp = s3_client.list_objects_v2(Bucket=BUCKET, Prefix="z")
        assert resp["KeyCount"] == 0
        assert "Contents" not in resp


class TestDelimiter:
    def test_delimiter_groups(self, s3_client):
        """Delimiter should group keys into CommonPrefixes."""
        put_objects(s3_client, [
            "photos/2024/a.jpg",
            "photos/2024/b.jpg",
            "photos/2025/c.jpg",
            "videos/d.mp4",
            "readme.txt",
        ])

        resp = s3_client.list_objects_v2(Bucket=BUCKET, Delimiter="/")

        # Direct objects (no delimiter after root)
        content_keys = [obj["Key"] for obj in resp.get("Contents", [])]
        assert content_keys == ["readme.txt"]

        # Common prefixes
        prefixes = [p["Prefix"] for p in resp.get("CommonPrefixes", [])]
        assert "photos/" in prefixes
        assert "videos/" in prefixes

    def test_prefix_and_delimiter(self, s3_client):
        """Prefix + delimiter should group within the prefix."""
        put_objects(s3_client, [
            "photos/2024/a.jpg",
            "photos/2024/b.jpg",
            "photos/2025/c.jpg",
            "photos/top.jpg",
        ])

        resp = s3_client.list_objects_v2(
            Bucket=BUCKET, Prefix="photos/", Delimiter="/",
        )

        # Direct objects under photos/ (no further delimiter)
        content_keys = [obj["Key"] for obj in resp.get("Contents", [])]
        assert content_keys == ["photos/top.jpg"]

        # Sub-prefixes
        prefixes = [p["Prefix"] for p in resp.get("CommonPrefixes", [])]
        assert "photos/2024/" in prefixes
        assert "photos/2025/" in prefixes


class TestPagination:
    def test_max_keys(self, s3_client):
        """MaxKeys should limit the number of returned objects."""
        put_objects(s3_client, ["a", "b", "c", "d", "e"])

        resp = s3_client.list_objects_v2(Bucket=BUCKET, MaxKeys=2)
        keys = [obj["Key"] for obj in resp["Contents"]]
        assert len(keys) == 2
        assert keys == ["a", "b"]
        assert resp["IsTruncated"] is True
        assert "NextContinuationToken" in resp

    def test_continuation_token(self, s3_client):
        """ContinuationToken should resume listing from where we left off."""
        put_objects(s3_client, ["a", "b", "c", "d", "e"])

        resp1 = s3_client.list_objects_v2(Bucket=BUCKET, MaxKeys=2)
        assert resp1["IsTruncated"] is True
        token = resp1["NextContinuationToken"]

        resp2 = s3_client.list_objects_v2(
            Bucket=BUCKET, MaxKeys=2, ContinuationToken=token,
        )
        keys2 = [obj["Key"] for obj in resp2["Contents"]]
        assert keys2 == ["c", "d"]
        assert resp2["IsTruncated"] is True

    def test_paginate_all(self, s3_client):
        """Paginating through all objects should yield all keys."""
        all_keys = [f"key-{i:02d}" for i in range(7)]
        put_objects(s3_client, all_keys)

        collected = []
        token = None
        while True:
            kwargs = {"Bucket": BUCKET, "MaxKeys": 3}
            if token:
                kwargs["ContinuationToken"] = token
            resp = s3_client.list_objects_v2(**kwargs)
            for obj in resp.get("Contents", []):
                collected.append(obj["Key"])
            if not resp["IsTruncated"]:
                break
            token = resp["NextContinuationToken"]

        assert collected == sorted(all_keys)

    def test_start_after(self, s3_client):
        """StartAfter should skip keys up to and including the specified key."""
        put_objects(s3_client, ["a", "b", "c", "d", "e"])

        resp = s3_client.list_objects_v2(Bucket=BUCKET, StartAfter="c")
        keys = [obj["Key"] for obj in resp["Contents"]]
        assert keys == ["d", "e"]
