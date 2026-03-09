"""Integration tests for ListObjectsV2 using the MinIO Python client.

Mirrors test_list.py but uses the minio_client fixture instead of boto3.
Bucket names use a "minio-" prefix to avoid collisions with boto3 tests.
"""

import io

import pytest
from minio.error import S3Error


BUCKET = "minio-test-list-bucket"


@pytest.fixture(autouse=True)
def setup_bucket(minio_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        minio_client.make_bucket(BUCKET)
    except S3Error:
        pass
    yield
    try:
        for obj in minio_client.list_objects(BUCKET, recursive=True):
            minio_client.remove_object(BUCKET, obj.object_name)
        minio_client.remove_bucket(BUCKET)
    except S3Error:
        pass


def put_objects(minio_client, keys):
    """Helper to put multiple single-byte objects."""
    for key in keys:
        minio_client.put_object(BUCKET, key, io.BytesIO(b"x"), 1)


class TestListBasic:
    def test_list_empty_bucket(self, minio_client):
        """Listing an empty bucket should return no objects."""
        objects = list(minio_client.list_objects(BUCKET))
        assert objects == []

    def test_list_basic(self, minio_client):
        """Listing a bucket with objects should return them sorted by key."""
        put_objects(minio_client, ["c.txt", "a.txt", "b.txt"])

        objects = list(minio_client.list_objects(BUCKET, recursive=True))
        keys = [obj.object_name for obj in objects]
        assert keys == ["a.txt", "b.txt", "c.txt"]

    def test_list_nonexistent_bucket(self, minio_client):
        """Listing a nonexistent bucket should raise S3Error with NoSuchBucket."""
        with pytest.raises(S3Error) as exc_info:
            list(minio_client.list_objects("nonexistent-minio-list-bucket"))
        assert exc_info.value.code == "NoSuchBucket"

    def test_content_fields(self, minio_client):
        """Each object should have object_name, size, etag, and last_modified."""
        data = b"hello world"
        minio_client.put_object(BUCKET, "fields.txt", io.BytesIO(data), len(data))

        objects = list(minio_client.list_objects(BUCKET, recursive=True))
        assert len(objects) == 1
        obj = objects[0]
        assert obj.object_name == "fields.txt"
        assert obj.size == len(data)
        assert obj.etag is not None
        assert obj.last_modified is not None


class TestPrefix:
    def test_prefix_filter(self, minio_client):
        """Prefix filter should only return keys starting with the prefix."""
        put_objects(minio_client, ["photos/a.jpg", "photos/b.jpg", "videos/c.mp4"])

        objects = list(minio_client.list_objects(BUCKET, prefix="photos/", recursive=True))
        keys = [obj.object_name for obj in objects]
        assert keys == ["photos/a.jpg", "photos/b.jpg"]

    def test_prefix_no_match(self, minio_client):
        """Prefix with no matching keys should return empty result."""
        put_objects(minio_client, ["a.txt", "b.txt"])

        objects = list(minio_client.list_objects(BUCKET, prefix="z", recursive=True))
        assert objects == []


class TestDelimiter:
    def test_delimiter_groups(self, minio_client):
        """recursive=False should group keys by '/' delimiter."""
        put_objects(minio_client, [
            "photos/2024/a.jpg",
            "photos/2024/b.jpg",
            "photos/2025/c.jpg",
            "videos/d.mp4",
            "readme.txt",
        ])

        objects = list(minio_client.list_objects(BUCKET, recursive=False))

        # Separate directory entries from regular objects
        dirs = [obj for obj in objects if obj.is_dir]
        files = [obj for obj in objects if not obj.is_dir]

        # Direct objects (no delimiter after root)
        file_keys = [obj.object_name for obj in files]
        assert file_keys == ["readme.txt"]

        # Common prefixes as directory entries
        dir_names = [obj.object_name for obj in dirs]
        assert "photos/" in dir_names
        assert "videos/" in dir_names

    def test_prefix_and_delimiter(self, minio_client):
        """Prefix + recursive=False should group within the prefix."""
        put_objects(minio_client, [
            "photos/2024/a.jpg",
            "photos/2024/b.jpg",
            "photos/2025/c.jpg",
            "photos/top.jpg",
        ])

        objects = list(minio_client.list_objects(BUCKET, prefix="photos/", recursive=False))

        # Separate directory entries from regular objects
        dirs = [obj for obj in objects if obj.is_dir]
        files = [obj for obj in objects if not obj.is_dir]

        # Direct objects under photos/ (no further delimiter)
        file_keys = [obj.object_name for obj in files]
        assert file_keys == ["photos/top.jpg"]

        # Sub-prefixes
        dir_names = [obj.object_name for obj in dirs]
        assert "photos/2024/" in dir_names
        assert "photos/2025/" in dir_names


class TestPagination:
    def test_all_objects_returned(self, minio_client):
        """MinIO client handles pagination internally; verify all objects are returned."""
        all_keys = [f"key-{i:02d}" for i in range(7)]
        put_objects(minio_client, all_keys)

        objects = list(minio_client.list_objects(BUCKET, recursive=True))
        keys = [obj.object_name for obj in objects]
        assert keys == sorted(all_keys)
        assert len(keys) == 7
