"""Integration tests for Phase 19 — Object Tagging.

Tests bucket tagging (Put/Get/Delete), object tagging (Put/Get/Delete),
inline tags on PutObject via x-amz-tagging header, CopyObject tag
directives, versioned object tags, cascade deletes, and edge cases.
"""

import pytest
from botocore.exceptions import ClientError, ParamValidationError


# -- Helpers --

def put_tags(s3_client, bucket, key=None, tags=None):
    """Helper to set tags on a bucket or object."""
    tag_set = [{"Key": k, "Value": v} for k, v in (tags or {}).items()]
    if key:
        s3_client.put_object_tagging(
            Bucket=bucket, Key=key, Tagging={"TagSet": tag_set}
        )
    else:
        s3_client.put_bucket_tagging(
            Bucket=bucket, Tagging={"TagSet": tag_set}
        )


def get_tags_dict(s3_client, bucket, key=None):
    """Helper to get tags as a dict."""
    if key:
        resp = s3_client.get_object_tagging(Bucket=bucket, Key=key)
    else:
        resp = s3_client.get_bucket_tagging(Bucket=bucket)
    return {t["Key"]: t["Value"] for t in resp["TagSet"]}


# -- Bucket Tagging --

class TestBucketTagging:
    def test_put_and_get_bucket_tags(self, s3_client):
        """PutBucketTagging followed by GetBucketTagging should return the tags."""
        s3_client.create_bucket(Bucket="tag-bucket-1")
        try:
            put_tags(s3_client, "tag-bucket-1", tags={"env": "prod", "team": "platform"})
            tags = get_tags_dict(s3_client, "tag-bucket-1")
            assert tags == {"env": "prod", "team": "platform"}
        finally:
            s3_client.delete_bucket_tagging(Bucket="tag-bucket-1")
            s3_client.delete_bucket(Bucket="tag-bucket-1")

    def test_get_bucket_tags_nonexistent(self, s3_client):
        """GetBucketTagging on a bucket with no tags should return NoSuchTagSet."""
        s3_client.create_bucket(Bucket="tag-bucket-2")
        try:
            with pytest.raises(ClientError) as exc_info:
                s3_client.get_bucket_tagging(Bucket="tag-bucket-2")
            assert exc_info.value.response["Error"]["Code"] == "NoSuchTagSet"
        finally:
            s3_client.delete_bucket(Bucket="tag-bucket-2")

    def test_delete_bucket_tags(self, s3_client):
        """DeleteBucketTagging should remove all tags."""
        s3_client.create_bucket(Bucket="tag-bucket-3")
        try:
            put_tags(s3_client, "tag-bucket-3", tags={"env": "dev"})
            s3_client.delete_bucket_tagging(Bucket="tag-bucket-3")
            with pytest.raises(ClientError) as exc_info:
                s3_client.get_bucket_tagging(Bucket="tag-bucket-3")
            assert exc_info.value.response["Error"]["Code"] == "NoSuchTagSet"
        finally:
            s3_client.delete_bucket(Bucket="tag-bucket-3")

    def test_replace_bucket_tags(self, s3_client):
        """PutBucketTagging should replace all existing tags."""
        s3_client.create_bucket(Bucket="tag-bucket-4")
        try:
            put_tags(s3_client, "tag-bucket-4", tags={"env": "dev", "team": "a"})
            put_tags(s3_client, "tag-bucket-4", tags={"status": "active"})
            tags = get_tags_dict(s3_client, "tag-bucket-4")
            assert tags == {"status": "active"}
            assert "env" not in tags
        finally:
            s3_client.delete_bucket_tagging(Bucket="tag-bucket-4")
            s3_client.delete_bucket(Bucket="tag-bucket-4")

    def test_bucket_tags_nonexistent_bucket(self, s3_client):
        """Tagging operations on a nonexistent bucket should return NoSuchBucket."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.get_bucket_tagging(Bucket="no-such-tag-bucket")
        assert exc_info.value.response["Error"]["Code"] == "NoSuchBucket"

    def test_bucket_tags_cascade_delete(self, s3_client):
        """Deleting a bucket should clean up its tags."""
        s3_client.create_bucket(Bucket="tag-bucket-5")
        put_tags(s3_client, "tag-bucket-5", tags={"env": "temp"})
        s3_client.delete_bucket(Bucket="tag-bucket-5")
        # Bucket is gone, so tagging should fail
        with pytest.raises(ClientError) as exc_info:
            s3_client.get_bucket_tagging(Bucket="tag-bucket-5")
        assert exc_info.value.response["Error"]["Code"] == "NoSuchBucket"

    def test_delete_bucket_tags_idempotent(self, s3_client):
        """DeleteBucketTagging on a bucket with no tags should return 204 (success)."""
        s3_client.create_bucket(Bucket="tag-bucket-6")
        try:
            # Should not raise
            s3_client.delete_bucket_tagging(Bucket="tag-bucket-6")
        finally:
            s3_client.delete_bucket(Bucket="tag-bucket-6")


# -- Object Tagging --

class TestObjectTagging:
    def test_put_and_get_object_tags(self, s3_client):
        """PutObjectTagging followed by GetObjectTagging should return the tags."""
        s3_client.create_bucket(Bucket="tag-obj-1")
        try:
            s3_client.put_object(Bucket="tag-obj-1", Key="file.txt", Body=b"hello")
            put_tags(s3_client, "tag-obj-1", key="file.txt", tags={"env": "prod", "team": "core"})
            tags = get_tags_dict(s3_client, "tag-obj-1", key="file.txt")
            assert tags == {"env": "prod", "team": "core"}
        finally:
            s3_client.delete_object(Bucket="tag-obj-1", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-obj-1")

    def test_get_object_tags_empty(self, s3_client):
        """GetObjectTagging on an object with no tags should return an empty TagSet."""
        s3_client.create_bucket(Bucket="tag-obj-2")
        try:
            s3_client.put_object(Bucket="tag-obj-2", Key="file.txt", Body=b"hello")
            resp = s3_client.get_object_tagging(Bucket="tag-obj-2", Key="file.txt")
            assert resp["TagSet"] == []
        finally:
            s3_client.delete_object(Bucket="tag-obj-2", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-obj-2")

    def test_delete_object_tags(self, s3_client):
        """DeleteObjectTagging should remove all tags."""
        s3_client.create_bucket(Bucket="tag-obj-3")
        try:
            s3_client.put_object(Bucket="tag-obj-3", Key="file.txt", Body=b"hello")
            put_tags(s3_client, "tag-obj-3", key="file.txt", tags={"env": "dev"})
            s3_client.delete_object_tagging(Bucket="tag-obj-3", Key="file.txt")
            resp = s3_client.get_object_tagging(Bucket="tag-obj-3", Key="file.txt")
            assert resp["TagSet"] == []
        finally:
            s3_client.delete_object(Bucket="tag-obj-3", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-obj-3")

    def test_replace_object_tags(self, s3_client):
        """PutObjectTagging should replace all existing tags."""
        s3_client.create_bucket(Bucket="tag-obj-4")
        try:
            s3_client.put_object(Bucket="tag-obj-4", Key="file.txt", Body=b"hello")
            put_tags(s3_client, "tag-obj-4", key="file.txt", tags={"env": "dev", "team": "a"})
            put_tags(s3_client, "tag-obj-4", key="file.txt", tags={"status": "archived"})
            tags = get_tags_dict(s3_client, "tag-obj-4", key="file.txt")
            assert tags == {"status": "archived"}
            assert "env" not in tags
        finally:
            s3_client.delete_object(Bucket="tag-obj-4", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-obj-4")

    def test_object_tags_nonexistent_key(self, s3_client):
        """GetObjectTagging on a nonexistent key should return NoSuchKey."""
        s3_client.create_bucket(Bucket="tag-obj-5")
        try:
            with pytest.raises(ClientError) as exc_info:
                s3_client.get_object_tagging(Bucket="tag-obj-5", Key="nope.txt")
            assert exc_info.value.response["Error"]["Code"] == "NoSuchKey"
        finally:
            s3_client.delete_bucket(Bucket="tag-obj-5")

    def test_object_tags_cascade_on_delete(self, s3_client):
        """Deleting an object should clean up its tags."""
        s3_client.create_bucket(Bucket="tag-obj-6")
        try:
            s3_client.put_object(Bucket="tag-obj-6", Key="file.txt", Body=b"hello")
            put_tags(s3_client, "tag-obj-6", key="file.txt", tags={"env": "temp"})
            s3_client.delete_object(Bucket="tag-obj-6", Key="file.txt")
            # Object is gone, tags should be too
            with pytest.raises(ClientError) as exc_info:
                s3_client.get_object_tagging(Bucket="tag-obj-6", Key="file.txt")
            assert exc_info.value.response["Error"]["Code"] == "NoSuchKey"
        finally:
            s3_client.delete_bucket(Bucket="tag-obj-6")

    def test_object_tags_cascade_on_overwrite(self, s3_client):
        """Overwriting an object should clear its old tags."""
        s3_client.create_bucket(Bucket="tag-obj-7")
        try:
            s3_client.put_object(Bucket="tag-obj-7", Key="file.txt", Body=b"v1")
            put_tags(s3_client, "tag-obj-7", key="file.txt", tags={"version": "1"})
            # Overwrite the object (no inline tags)
            s3_client.put_object(Bucket="tag-obj-7", Key="file.txt", Body=b"v2")
            resp = s3_client.get_object_tagging(Bucket="tag-obj-7", Key="file.txt")
            assert resp["TagSet"] == []
        finally:
            s3_client.delete_object(Bucket="tag-obj-7", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-obj-7")


# -- Inline Tags on PutObject --

class TestInlineTags:
    def test_put_object_with_tagging_header(self, s3_client):
        """PutObject with x-amz-tagging header should store inline tags."""
        s3_client.create_bucket(Bucket="tag-inline-1")
        try:
            s3_client.put_object(
                Bucket="tag-inline-1", Key="file.txt", Body=b"hello",
                Tagging="env=prod&team=platform"
            )
            tags = get_tags_dict(s3_client, "tag-inline-1", key="file.txt")
            assert tags == {"env": "prod", "team": "platform"}
        finally:
            s3_client.delete_object(Bucket="tag-inline-1", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-inline-1")

    def test_put_object_without_tagging_header(self, s3_client):
        """PutObject without x-amz-tagging should have no tags."""
        s3_client.create_bucket(Bucket="tag-inline-2")
        try:
            s3_client.put_object(Bucket="tag-inline-2", Key="file.txt", Body=b"hello")
            resp = s3_client.get_object_tagging(Bucket="tag-inline-2", Key="file.txt")
            assert resp["TagSet"] == []
        finally:
            s3_client.delete_object(Bucket="tag-inline-2", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-inline-2")


# -- CopyObject Tagging Directive --

class TestCopyObjectTagging:
    def test_copy_default_copies_tags(self, s3_client):
        """CopyObject with default (COPY) tagging directive should copy source tags."""
        s3_client.create_bucket(Bucket="tag-copy-1")
        try:
            s3_client.put_object(Bucket="tag-copy-1", Key="src.txt", Body=b"hello")
            put_tags(s3_client, "tag-copy-1", key="src.txt", tags={"env": "prod"})
            s3_client.copy_object(
                Bucket="tag-copy-1", Key="dst.txt",
                CopySource={"Bucket": "tag-copy-1", "Key": "src.txt"},
            )
            tags = get_tags_dict(s3_client, "tag-copy-1", key="dst.txt")
            assert tags == {"env": "prod"}
        finally:
            s3_client.delete_object(Bucket="tag-copy-1", Key="src.txt")
            s3_client.delete_object(Bucket="tag-copy-1", Key="dst.txt")
            s3_client.delete_bucket(Bucket="tag-copy-1")

    def test_copy_replace_tags(self, s3_client):
        """CopyObject with REPLACE tagging directive should use the x-amz-tagging header."""
        s3_client.create_bucket(Bucket="tag-copy-2")
        try:
            s3_client.put_object(Bucket="tag-copy-2", Key="src.txt", Body=b"hello")
            put_tags(s3_client, "tag-copy-2", key="src.txt", tags={"env": "prod"})
            s3_client.copy_object(
                Bucket="tag-copy-2", Key="dst.txt",
                CopySource={"Bucket": "tag-copy-2", "Key": "src.txt"},
                TaggingDirective="REPLACE",
                Tagging="status=archived&team=ops",
            )
            tags = get_tags_dict(s3_client, "tag-copy-2", key="dst.txt")
            assert tags == {"status": "archived", "team": "ops"}
            assert "env" not in tags
        finally:
            s3_client.delete_object(Bucket="tag-copy-2", Key="src.txt")
            s3_client.delete_object(Bucket="tag-copy-2", Key="dst.txt")
            s3_client.delete_bucket(Bucket="tag-copy-2")


# -- Edge Cases --

class TestTaggingEdgeCases:
    def test_max_10_tags(self, s3_client):
        """S3 allows up to 10 tags per object."""
        s3_client.create_bucket(Bucket="tag-edge-1")
        try:
            s3_client.put_object(Bucket="tag-edge-1", Key="file.txt", Body=b"hello")
            tags = {f"key{i}": f"val{i}" for i in range(10)}
            put_tags(s3_client, "tag-edge-1", key="file.txt", tags=tags)
            got = get_tags_dict(s3_client, "tag-edge-1", key="file.txt")
            assert len(got) == 10
        finally:
            s3_client.delete_object(Bucket="tag-edge-1", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-edge-1")

    def test_exceed_10_tags(self, s3_client):
        """More than 10 tags should be rejected."""
        s3_client.create_bucket(Bucket="tag-edge-2")
        try:
            s3_client.put_object(Bucket="tag-edge-2", Key="file.txt", Body=b"hello")
            tags = [{"Key": f"key{i}", "Value": f"val{i}"} for i in range(11)]
            with pytest.raises(ClientError):
                s3_client.put_object_tagging(
                    Bucket="tag-edge-2", Key="file.txt",
                    Tagging={"TagSet": tags}
                )
        finally:
            s3_client.delete_object(Bucket="tag-edge-2", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-edge-2")

    def test_empty_tag_key_rejected(self, s3_client):
        """A tag with an empty key should be rejected (client-side by boto3)."""
        s3_client.create_bucket(Bucket="tag-edge-3")
        try:
            s3_client.put_object(Bucket="tag-edge-3", Key="file.txt", Body=b"hello")
            with pytest.raises(ParamValidationError):
                s3_client.put_object_tagging(
                    Bucket="tag-edge-3", Key="file.txt",
                    Tagging={"TagSet": [{"Key": "", "Value": "x"}]}
                )
        finally:
            s3_client.delete_object(Bucket="tag-edge-3", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-edge-3")

    def test_tag_value_empty_allowed(self, s3_client):
        """A tag with an empty value should be allowed."""
        s3_client.create_bucket(Bucket="tag-edge-4")
        try:
            s3_client.put_object(Bucket="tag-edge-4", Key="file.txt", Body=b"hello")
            put_tags(s3_client, "tag-edge-4", key="file.txt", tags={"env": ""})
            tags = get_tags_dict(s3_client, "tag-edge-4", key="file.txt")
            assert tags == {"env": ""}
        finally:
            s3_client.delete_object(Bucket="tag-edge-4", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-edge-4")

    def test_special_characters_in_tags(self, s3_client):
        """Tags with special characters (spaces, unicode) should work."""
        s3_client.create_bucket(Bucket="tag-edge-5")
        try:
            s3_client.put_object(Bucket="tag-edge-5", Key="file.txt", Body=b"hello")
            put_tags(s3_client, "tag-edge-5", key="file.txt",
                     tags={"Project Name": "Arca Storage", "Cost Center": "CC-1234"})
            tags = get_tags_dict(s3_client, "tag-edge-5", key="file.txt")
            assert tags == {"Project Name": "Arca Storage", "Cost Center": "CC-1234"}
        finally:
            s3_client.delete_object(Bucket="tag-edge-5", Key="file.txt")
            s3_client.delete_bucket(Bucket="tag-edge-5")
