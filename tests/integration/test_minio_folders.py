"""Integration tests for folder (directory marker) operations using the MinIO Python client.

Mirrors test_folders.py but uses the minio_client fixture instead of boto3.
Bucket names use a "minio-" prefix to avoid collisions with boto3 tests.
"""

import io

import pytest
from minio.deleteobjects import DeleteObject
from minio.error import S3Error


BUCKET = "minio-test-folders-bucket"


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
    """Helper to put multiple empty objects."""
    for key in keys:
        minio_client.put_object(BUCKET, key, io.BytesIO(b""), 0)


class TestDirectoryMarkerCreation:
    def test_create_directory_marker(self, minio_client):
        """PUT a zero-byte object with trailing slash creates a directory marker."""
        minio_client.put_object(BUCKET, "myfolder/", io.BytesIO(b""), 0)

        stat = minio_client.stat_object(BUCKET, "myfolder/")
        assert stat.size == 0

    def test_directory_marker_key_preserved(self, minio_client):
        """The trailing slash in the key must be preserved exactly."""
        minio_client.put_object(BUCKET, "docs/", io.BytesIO(b""), 0)

        # List without delimiter (recursive) to see all raw keys
        objects = list(minio_client.list_objects(BUCKET, recursive=True))
        keys = [obj.object_name for obj in objects]
        assert "docs/" in keys

    def test_nested_directory_markers(self, minio_client):
        """Multiple levels of directory markers can coexist."""
        minio_client.put_object(BUCKET, "a/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "a/b/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "a/b/c/", io.BytesIO(b""), 0)

        objects = list(minio_client.list_objects(BUCKET, recursive=True))
        keys = [obj.object_name for obj in objects]
        assert "a/" in keys
        assert "a/b/" in keys
        assert "a/b/c/" in keys

    def test_directory_marker_with_content_type(self, minio_client):
        """Directory markers can have a content type (e.g. application/x-directory)."""
        minio_client.put_object(
            BUCKET, "uploads/", io.BytesIO(b""), 0,
            content_type="application/x-directory",
        )

        stat = minio_client.stat_object(BUCKET, "uploads/")
        assert stat.content_type == "application/x-directory"


class TestDirectoryMarkerListing:
    def test_marker_as_common_prefix(self, minio_client):
        """Directory marker should appear as is_dir entry when listing non-recursive."""
        minio_client.put_object(BUCKET, "photos/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "photos/a.jpg", io.BytesIO(b"img"), 3)

        objects = list(minio_client.list_objects(BUCKET, recursive=False))

        dirs = [obj for obj in objects if obj.is_dir]
        files = [obj for obj in objects if not obj.is_dir]

        dir_names = [obj.object_name for obj in dirs]
        file_keys = [obj.object_name for obj in files]

        assert "photos/" in dir_names
        # The marker itself should not appear as a regular file
        assert "photos/" not in file_keys

    def test_marker_visible_without_delimiter(self, minio_client):
        """Without a delimiter (recursive=True), directory markers appear as normal objects."""
        minio_client.put_object(BUCKET, "folder/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "folder/file.txt", io.BytesIO(b"x"), 1)

        objects = list(minio_client.list_objects(BUCKET, prefix="folder/", recursive=True))
        keys = [obj.object_name for obj in objects]
        # Without delimiter, the marker is just another object
        assert "folder/" in keys
        assert "folder/file.txt" in keys

    def test_empty_folder_shows_as_prefix(self, minio_client):
        """An empty folder (only marker, no files) still shows as is_dir entry."""
        minio_client.put_object(BUCKET, "empty/", io.BytesIO(b""), 0)

        objects = list(minio_client.list_objects(BUCKET, recursive=False))

        dirs = [obj for obj in objects if obj.is_dir]
        files = [obj for obj in objects if not obj.is_dir]

        dir_names = [obj.object_name for obj in dirs]
        file_keys = [obj.object_name for obj in files]

        assert "empty/" in dir_names
        assert "empty/" not in file_keys


class TestDirectoryMarkerDeletion:
    def test_delete_empty_directory_marker(self, minio_client):
        """Deleting a directory marker removes it."""
        minio_client.put_object(BUCKET, "temp/", io.BytesIO(b""), 0)
        minio_client.remove_object(BUCKET, "temp/")

        with pytest.raises(S3Error) as exc_info:
            minio_client.stat_object(BUCKET, "temp/")
        assert exc_info.value.code == "NoSuchKey"

    def test_delete_marker_leaves_children(self, minio_client):
        """Deleting a directory marker does not delete its children."""
        minio_client.put_object(BUCKET, "parent/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "parent/child.txt", io.BytesIO(b"x"), 1)

        minio_client.remove_object(BUCKET, "parent/")

        # Child should still exist
        stat = minio_client.stat_object(BUCKET, "parent/child.txt")
        assert stat.size == 1


class TestDeleteObjects:
    def test_delete_multiple_objects(self, minio_client):
        """remove_objects should delete multiple keys in one request."""
        keys = ["a.txt", "b.txt", "c.txt"]
        for key in keys:
            minio_client.put_object(BUCKET, key, io.BytesIO(b"x"), 1)

        errors = list(minio_client.remove_objects(
            BUCKET, [DeleteObject(k) for k in keys],
        ))
        assert errors == []

        # Verify all deleted
        objects = list(minio_client.list_objects(BUCKET, recursive=True))
        assert objects == []

    def test_delete_objects_with_trailing_slash(self, minio_client):
        """remove_objects should handle keys with trailing slashes."""
        minio_client.put_object(BUCKET, "dir/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "dir/file.txt", io.BytesIO(b"x"), 1)

        errors = list(minio_client.remove_objects(
            BUCKET, [DeleteObject("dir/"), DeleteObject("dir/file.txt")],
        ))
        assert errors == []

        objects = list(minio_client.list_objects(BUCKET, recursive=True))
        assert objects == []

    def test_recursive_folder_deletion(self, minio_client):
        """Simulate recursive folder deletion: list prefix recursive + remove_objects."""
        # Create a folder tree
        minio_client.put_object(BUCKET, "project/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "project/src/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "project/src/main.py", io.BytesIO(b"code"), 4)
        minio_client.put_object(BUCKET, "project/src/utils.py", io.BytesIO(b"util"), 4)
        minio_client.put_object(BUCKET, "project/docs/", io.BytesIO(b""), 0)
        minio_client.put_object(BUCKET, "project/docs/readme.md", io.BytesIO(b"docs"), 4)
        minio_client.put_object(BUCKET, "project/config.toml", io.BytesIO(b"cfg"), 3)
        minio_client.put_object(BUCKET, "other.txt", io.BytesIO(b"keep"), 4)

        # List all objects under project/ (recursive for flat listing)
        project_objects = list(minio_client.list_objects(BUCKET, prefix="project/", recursive=True))
        keys = [obj.object_name for obj in project_objects]

        # Delete them all via remove_objects
        errors = list(minio_client.remove_objects(
            BUCKET, [DeleteObject(k) for k in keys],
        ))
        assert errors == []

        # Verify project/ tree is gone but other.txt remains
        remaining = list(minio_client.list_objects(BUCKET, recursive=True))
        remaining_keys = [obj.object_name for obj in remaining]
        assert remaining_keys == ["other.txt"]
        assert not any(k.startswith("project/") for k in remaining_keys)


class TestSpecialChars:
    def test_object_with_special_chars_in_key(self, minio_client):
        """Objects with spaces in keys should work correctly."""
        special_keys = [
            "file with spaces.txt",
            "path/with spaces/file.txt",
        ]
        for key in special_keys:
            data = b"data"
            minio_client.put_object(BUCKET, key, io.BytesIO(data), len(data))

            resp = minio_client.get_object(BUCKET, key)
            try:
                assert resp.read() == data
            finally:
                resp.close()
                resp.release_conn()

            minio_client.remove_object(BUCKET, key)
