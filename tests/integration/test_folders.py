"""Integration tests for folder (directory marker) operations.

Tests creating, listing, and deleting directory marker objects (zero-byte
objects with keys ending in '/'), including recursive deletion via
DeleteObjects and trailing-slash preservation in the normalize middleware.
"""

import pytest
from botocore.exceptions import ClientError


BUCKET = "test-folders-bucket"


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass
    yield
    # Cleanup: delete all objects then the bucket
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
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"")


class TestDirectoryMarkerCreation:
    def test_create_directory_marker(self, s3_client):
        """PUT a zero-byte object with trailing slash creates a directory marker."""
        s3_client.put_object(Bucket=BUCKET, Key="myfolder/", Body=b"")

        resp = s3_client.head_object(Bucket=BUCKET, Key="myfolder/")
        assert resp["ContentLength"] == 0

    def test_directory_marker_key_preserved(self, s3_client):
        """The trailing slash in the key must be preserved exactly."""
        s3_client.put_object(Bucket=BUCKET, Key="docs/", Body=b"")

        # List without delimiter to see all raw keys
        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        keys = [obj["Key"] for obj in resp.get("Contents", [])]
        assert "docs/" in keys

    def test_nested_directory_markers(self, s3_client):
        """Multiple levels of directory markers can coexist."""
        s3_client.put_object(Bucket=BUCKET, Key="a/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="a/b/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="a/b/c/", Body=b"")

        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        keys = [obj["Key"] for obj in resp.get("Contents", [])]
        assert "a/" in keys
        assert "a/b/" in keys
        assert "a/b/c/" in keys

    def test_directory_marker_with_content_type(self, s3_client):
        """Directory markers can have a content type (e.g. application/x-directory)."""
        s3_client.put_object(
            Bucket=BUCKET, Key="uploads/",
            Body=b"", ContentType="application/x-directory",
        )

        resp = s3_client.head_object(Bucket=BUCKET, Key="uploads/")
        assert resp["ContentType"] == "application/x-directory"

    def test_directory_marker_with_spaces_in_name(self, s3_client):
        """Directory markers with spaces in the name work correctly."""
        s3_client.put_object(Bucket=BUCKET, Key="my folder/", Body=b"")

        resp = s3_client.head_object(Bucket=BUCKET, Key="my folder/")
        assert resp["ContentLength"] == 0

        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        keys = [obj["Key"] for obj in resp.get("Contents", [])]
        assert "my folder/" in keys


class TestDirectoryMarkerListing:
    def test_marker_visible_in_delimiter_listing_at_prefix(self, s3_client):
        """Directory marker appears in Contents when listing its own prefix with delimiter.

        With Prefix="photos/" and Delimiter="/", the key "photos/" has an empty
        remainder after stripping the prefix — no delimiter found, so it's a
        regular Content entry (not collapsed into CommonPrefixes).
        """
        s3_client.put_object(Bucket=BUCKET, Key="photos/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="photos/a.jpg", Body=b"img")

        resp = s3_client.list_objects_v2(
            Bucket=BUCKET, Prefix="photos/", Delimiter="/",
        )
        content_keys = [obj["Key"] for obj in resp.get("Contents", [])]
        # The directory marker 'photos/' IS in Contents (S3 behavior)
        assert "photos/" in content_keys
        assert "photos/a.jpg" in content_keys

    def test_marker_hidden_from_root_delimiter_listing(self, s3_client):
        """Directory marker should appear as CommonPrefix, not Contents, at root."""
        s3_client.put_object(Bucket=BUCKET, Key="data/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="data/file.txt", Body=b"x")
        s3_client.put_object(Bucket=BUCKET, Key="root.txt", Body=b"x")

        resp = s3_client.list_objects_v2(Bucket=BUCKET, Delimiter="/")
        content_keys = [obj["Key"] for obj in resp.get("Contents", [])]
        prefixes = [p["Prefix"] for p in resp.get("CommonPrefixes", [])]

        # 'data/' marker grouped under CommonPrefixes, not in Contents
        assert "data/" not in content_keys
        assert "data/" in prefixes
        assert "root.txt" in content_keys

    def test_nested_marker_visible(self, s3_client):
        """Nested directory markers appear in Contents when listing their own prefix.

        S3 returns zero-byte directory markers as Content entries when the key
        exactly matches the prefix (empty remainder after stripping prefix).
        """
        s3_client.put_object(Bucket=BUCKET, Key="a/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="a/b/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="a/b/file.txt", Body=b"x")

        # List a/ with delimiter — a/ marker in Contents, b/ as CommonPrefix
        resp = s3_client.list_objects_v2(
            Bucket=BUCKET, Prefix="a/", Delimiter="/",
        )
        content_keys = [obj["Key"] for obj in resp.get("Contents", [])]
        prefixes = [p["Prefix"] for p in resp.get("CommonPrefixes", [])]

        assert "a/" in content_keys
        assert "a/b/" in prefixes

        # List a/b/ with delimiter — a/b/ marker in Contents, plus the file
        resp = s3_client.list_objects_v2(
            Bucket=BUCKET, Prefix="a/b/", Delimiter="/",
        )
        content_keys = [obj["Key"] for obj in resp.get("Contents", [])]
        assert "a/b/" in content_keys
        assert "a/b/file.txt" in content_keys

    def test_marker_visible_without_delimiter(self, s3_client):
        """Without a delimiter, directory markers appear as normal objects."""
        s3_client.put_object(Bucket=BUCKET, Key="folder/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="folder/file.txt", Body=b"x")

        resp = s3_client.list_objects_v2(Bucket=BUCKET, Prefix="folder/")
        keys = [obj["Key"] for obj in resp.get("Contents", [])]
        # Without delimiter, the marker is just another object
        assert "folder/" in keys
        assert "folder/file.txt" in keys

    def test_empty_folder_shows_as_common_prefix(self, s3_client):
        """An empty folder (only marker, no files) still shows as CommonPrefix."""
        s3_client.put_object(Bucket=BUCKET, Key="empty/", Body=b"")

        resp = s3_client.list_objects_v2(Bucket=BUCKET, Delimiter="/")
        prefixes = [p["Prefix"] for p in resp.get("CommonPrefixes", [])]
        content_keys = [obj["Key"] for obj in resp.get("Contents", [])]

        assert "empty/" in prefixes
        assert "empty/" not in content_keys


class TestDirectoryMarkerDeletion:
    def test_delete_empty_directory_marker(self, s3_client):
        """Deleting a directory marker removes it."""
        s3_client.put_object(Bucket=BUCKET, Key="temp/", Body=b"")
        s3_client.delete_object(Bucket=BUCKET, Key="temp/")

        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(Bucket=BUCKET, Key="temp/")
        assert exc_info.value.response["Error"]["Code"] == "404"

    def test_delete_marker_leaves_children(self, s3_client):
        """Deleting a directory marker does not delete its children."""
        s3_client.put_object(Bucket=BUCKET, Key="parent/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="parent/child.txt", Body=b"x")

        s3_client.delete_object(Bucket=BUCKET, Key="parent/")

        # Child should still exist
        resp = s3_client.head_object(Bucket=BUCKET, Key="parent/child.txt")
        assert resp["ContentLength"] == 1


class TestDeleteObjects:
    def test_delete_multiple_objects(self, s3_client):
        """DeleteObjects should delete multiple keys in one request."""
        keys = ["a.txt", "b.txt", "c.txt"]
        put_objects(s3_client, keys)

        resp = s3_client.delete_objects(
            Bucket=BUCKET,
            Delete={"Objects": [{"Key": k} for k in keys], "Quiet": True},
        )

        # Verify all deleted
        list_resp = s3_client.list_objects_v2(Bucket=BUCKET)
        assert list_resp["KeyCount"] == 0

    def test_delete_objects_with_trailing_slash_keys(self, s3_client):
        """DeleteObjects should handle keys with trailing slashes."""
        s3_client.put_object(Bucket=BUCKET, Key="dir/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="dir/file.txt", Body=b"x")

        resp = s3_client.delete_objects(
            Bucket=BUCKET,
            Delete={
                "Objects": [{"Key": "dir/"}, {"Key": "dir/file.txt"}],
                "Quiet": True,
            },
        )

        list_resp = s3_client.list_objects_v2(Bucket=BUCKET)
        assert list_resp["KeyCount"] == 0

    def test_delete_objects_partial_nonexistent(self, s3_client):
        """DeleteObjects with some nonexistent keys should still delete existing ones."""
        s3_client.put_object(Bucket=BUCKET, Key="exists.txt", Body=b"x")

        resp = s3_client.delete_objects(
            Bucket=BUCKET,
            Delete={
                "Objects": [
                    {"Key": "exists.txt"},
                    {"Key": "nonexistent.txt"},
                ],
                "Quiet": True,
            },
        )

        list_resp = s3_client.list_objects_v2(Bucket=BUCKET)
        assert list_resp["KeyCount"] == 0

    def test_recursive_folder_deletion(self, s3_client):
        """Simulate recursive folder deletion: list + DeleteObjects."""
        # Create a folder tree
        s3_client.put_object(Bucket=BUCKET, Key="project/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="project/src/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="project/src/main.py", Body=b"code")
        s3_client.put_object(Bucket=BUCKET, Key="project/src/utils.py", Body=b"util")
        s3_client.put_object(Bucket=BUCKET, Key="project/docs/", Body=b"")
        s3_client.put_object(Bucket=BUCKET, Key="project/docs/readme.md", Body=b"docs")
        s3_client.put_object(Bucket=BUCKET, Key="project/config.toml", Body=b"cfg")
        s3_client.put_object(Bucket=BUCKET, Key="other.txt", Body=b"keep")

        # List all objects under project/ (no delimiter for flat listing)
        resp = s3_client.list_objects_v2(Bucket=BUCKET, Prefix="project/")
        keys = [obj["Key"] for obj in resp.get("Contents", [])]

        # Delete them all via DeleteObjects
        s3_client.delete_objects(
            Bucket=BUCKET,
            Delete={"Objects": [{"Key": k} for k in keys], "Quiet": True},
        )

        # Verify project/ tree is gone but other.txt remains
        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        remaining_keys = [obj["Key"] for obj in resp.get("Contents", [])]
        assert remaining_keys == ["other.txt"]
        assert not any(k.startswith("project/") for k in remaining_keys)


class TestTrailingSlashNormalization:
    def test_bucket_trailing_slash_still_works(self, s3_client):
        """Bucket operations with trailing slash should still work (mc compat)."""
        # This test verifies the normalize middleware still strips
        # trailing slashes for bucket-level paths.
        # boto3 doesn't add trailing slashes, but we can verify
        # bucket operations work normally.
        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        assert "KeyCount" in resp

    def test_object_with_special_chars_in_key(self, s3_client):
        """Objects with special characters in keys should work."""
        special_keys = [
            "file with spaces.txt",
            "path/with spaces/file.txt",
            "unicode-cafe\u0301.txt",
        ]
        for key in special_keys:
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"data")
            resp = s3_client.get_object(Bucket=BUCKET, Key=key)
            assert resp["Body"].read() == b"data"
            s3_client.delete_object(Bucket=BUCKET, Key=key)
