"""Integration tests for Phase 3 — S3 compatibility fixes.

Tests user metadata (x-amz-meta-*), system metadata headers,
CopyObject metadata-directive, fetch-owner in ListObjectsV2,
and ListMultipartUploads.
"""

import io

import pytest
from botocore.exceptions import ClientError


BUCKET = "test-phase3-bucket"


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass
    yield
    # Clean up multipart uploads first.
    try:
        uploads = s3_client.list_multipart_uploads(Bucket=BUCKET)
        for upload in uploads.get("Uploads", []):
            s3_client.abort_multipart_upload(
                Bucket=BUCKET, Key=upload["Key"], UploadId=upload["UploadId"]
            )
    except ClientError:
        pass
    # Clean up objects and bucket.
    try:
        response = s3_client.list_objects_v2(Bucket=BUCKET)
        for obj in response.get("Contents", []):
            s3_client.delete_object(Bucket=BUCKET, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass


class TestUserMetadata:
    """Tests for x-amz-meta-* custom headers on PutObject/GetObject/HeadObject."""

    def test_put_get_user_metadata(self, s3_client):
        """User metadata set on PutObject should be returned on GetObject."""
        metadata = {"color": "blue", "priority": "high"}
        s3_client.put_object(
            Bucket=BUCKET, Key="meta.txt", Body=b"data", Metadata=metadata
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="meta.txt")
        assert resp["Metadata"]["color"] == "blue"
        assert resp["Metadata"]["priority"] == "high"

    def test_head_returns_user_metadata(self, s3_client):
        """HeadObject should return user metadata."""
        metadata = {"author": "pietro"}
        s3_client.put_object(
            Bucket=BUCKET, Key="meta2.txt", Body=b"data", Metadata=metadata
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="meta2.txt")
        assert resp["Metadata"]["author"] == "pietro"

    def test_overwrite_replaces_metadata(self, s3_client):
        """Overwriting an object should replace its metadata."""
        s3_client.put_object(
            Bucket=BUCKET, Key="ow.txt", Body=b"v1", Metadata={"version": "1"}
        )
        s3_client.put_object(
            Bucket=BUCKET, Key="ow.txt", Body=b"v2", Metadata={"version": "2"}
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="ow.txt")
        assert resp["Metadata"]["version"] == "2"

    def test_no_metadata_returns_empty(self, s3_client):
        """Object without metadata should return empty Metadata dict."""
        s3_client.put_object(Bucket=BUCKET, Key="plain.txt", Body=b"data")
        resp = s3_client.head_object(Bucket=BUCKET, Key="plain.txt")
        assert resp["Metadata"] == {}


class TestSystemMetadata:
    """Tests for system metadata headers (Cache-Control, Content-Disposition, etc.)."""

    def test_cache_control_roundtrip(self, s3_client):
        """Cache-Control header should be stored and returned."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="cached.txt",
            Body=b"data",
            CacheControl="max-age=3600",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="cached.txt")
        assert resp["CacheControl"] == "max-age=3600"

    def test_content_disposition_roundtrip(self, s3_client):
        """Content-Disposition header should be stored and returned."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="download.txt",
            Body=b"data",
            ContentDisposition='attachment; filename="file.txt"',
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="download.txt")
        assert resp["ContentDisposition"] == 'attachment; filename="file.txt"'

    def test_content_encoding_roundtrip(self, s3_client):
        """Content-Encoding header should be stored and returned."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="encoded.txt",
            Body=b"data",
            ContentEncoding="gzip",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="encoded.txt")
        assert resp["ContentEncoding"] == "gzip"

    def test_content_language_roundtrip(self, s3_client):
        """Content-Language header should be stored and returned."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="lang.txt",
            Body=b"data",
            ContentLanguage="en-US",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="lang.txt")
        assert resp["ContentLanguage"] == "en-US"


class TestCopyObjectMetadataDirective:
    """Tests for x-amz-metadata-directive on CopyObject."""

    def test_copy_preserves_metadata_by_default(self, s3_client):
        """COPY (default) should preserve source metadata."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="src.txt",
            Body=b"data",
            Metadata={"origin": "source"},
        )
        s3_client.copy_object(
            Bucket=BUCKET,
            Key="dst.txt",
            CopySource=f"{BUCKET}/src.txt",
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="dst.txt")
        assert resp["Metadata"]["origin"] == "source"

    def test_copy_with_replace_uses_new_metadata(self, s3_client):
        """REPLACE directive should use the request's metadata, not source."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="src.txt",
            Body=b"data",
            Metadata={"origin": "source"},
        )
        s3_client.copy_object(
            Bucket=BUCKET,
            Key="dst.txt",
            CopySource=f"{BUCKET}/src.txt",
            MetadataDirective="REPLACE",
            Metadata={"origin": "replaced"},
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="dst.txt")
        assert resp["Metadata"]["origin"] == "replaced"

    def test_copy_replace_clears_metadata(self, s3_client):
        """REPLACE with empty metadata should clear source metadata."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="src.txt",
            Body=b"data",
            Metadata={"keep": "no"},
        )
        s3_client.copy_object(
            Bucket=BUCKET,
            Key="dst.txt",
            CopySource=f"{BUCKET}/src.txt",
            MetadataDirective="REPLACE",
            Metadata={},
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="dst.txt")
        assert resp["Metadata"] == {}

    def test_copy_preserves_content_type(self, s3_client):
        """COPY directive should preserve source content-type."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="src.txt",
            Body=b"data",
            ContentType="application/json",
        )
        s3_client.copy_object(
            Bucket=BUCKET,
            Key="dst.txt",
            CopySource=f"{BUCKET}/src.txt",
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="dst.txt")
        assert resp["ContentType"] == "application/json"

    def test_copy_replace_changes_content_type(self, s3_client):
        """REPLACE directive should use request's content-type."""
        s3_client.put_object(
            Bucket=BUCKET,
            Key="src.txt",
            Body=b"data",
            ContentType="application/json",
        )
        s3_client.copy_object(
            Bucket=BUCKET,
            Key="dst.txt",
            CopySource=f"{BUCKET}/src.txt",
            MetadataDirective="REPLACE",
            ContentType="text/plain",
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="dst.txt")
        assert resp["ContentType"] == "text/plain"


class TestFetchOwner:
    """Tests for fetch-owner parameter in ListObjectsV2."""

    def test_list_v2_without_fetch_owner(self, s3_client):
        """ListObjectsV2 without FetchOwner should not include Owner."""
        s3_client.put_object(Bucket=BUCKET, Key="obj.txt", Body=b"data")
        resp = s3_client.list_objects_v2(Bucket=BUCKET)
        # boto3 omits Owner key entirely when not present in XML.
        for item in resp["Contents"]:
            assert "Owner" not in item

    def test_list_v2_with_fetch_owner(self, s3_client):
        """ListObjectsV2 with FetchOwner=True should include Owner."""
        s3_client.put_object(Bucket=BUCKET, Key="obj.txt", Body=b"data")
        resp = s3_client.list_objects_v2(Bucket=BUCKET, FetchOwner=True)
        for item in resp["Contents"]:
            assert "Owner" in item
            assert "ID" in item["Owner"]
            assert "DisplayName" in item["Owner"]


class TestListMultipartUploads:
    """Tests for ListMultipartUploads (GET /{bucket}?uploads)."""

    def test_list_no_uploads(self, s3_client):
        """Empty bucket should return no uploads."""
        resp = s3_client.list_multipart_uploads(Bucket=BUCKET)
        assert resp.get("Uploads", []) == []

    def test_list_active_upload(self, s3_client):
        """Active multipart upload should appear in listing."""
        create = s3_client.create_multipart_upload(
            Bucket=BUCKET, Key="big.bin"
        )
        upload_id = create["UploadId"]

        resp = s3_client.list_multipart_uploads(Bucket=BUCKET)
        uploads = resp.get("Uploads", [])
        assert len(uploads) == 1
        assert uploads[0]["Key"] == "big.bin"
        assert uploads[0]["UploadId"] == upload_id

        # Cleanup.
        s3_client.abort_multipart_upload(
            Bucket=BUCKET, Key="big.bin", UploadId=upload_id
        )

    def test_list_multiple_uploads(self, s3_client):
        """Multiple active uploads should all appear."""
        ids = []
        for key in ["a.bin", "b.bin", "c.bin"]:
            create = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
            ids.append(create["UploadId"])

        resp = s3_client.list_multipart_uploads(Bucket=BUCKET)
        uploads = resp.get("Uploads", [])
        assert len(uploads) == 3
        listed_keys = {u["Key"] for u in uploads}
        assert listed_keys == {"a.bin", "b.bin", "c.bin"}

        # Cleanup.
        for key, uid in zip(["a.bin", "b.bin", "c.bin"], ids):
            s3_client.abort_multipart_upload(
                Bucket=BUCKET, Key=key, UploadId=uid
            )

    def test_completed_upload_not_listed(self, s3_client):
        """Completed multipart upload should not appear in listing."""
        create = s3_client.create_multipart_upload(
            Bucket=BUCKET, Key="done.bin"
        )
        upload_id = create["UploadId"]

        # Upload a single part and complete.
        part = s3_client.upload_part(
            Bucket=BUCKET,
            Key="done.bin",
            UploadId=upload_id,
            PartNumber=1,
            Body=b"x" * (5 * 1024 * 1024),
        )
        s3_client.complete_multipart_upload(
            Bucket=BUCKET,
            Key="done.bin",
            UploadId=upload_id,
            MultipartUpload={
                "Parts": [{"PartNumber": 1, "ETag": part["ETag"]}]
            },
        )

        resp = s3_client.list_multipart_uploads(Bucket=BUCKET)
        assert resp.get("Uploads", []) == []

    def test_aborted_upload_not_listed(self, s3_client):
        """Aborted multipart upload should not appear in listing."""
        create = s3_client.create_multipart_upload(
            Bucket=BUCKET, Key="aborted.bin"
        )
        upload_id = create["UploadId"]
        s3_client.abort_multipart_upload(
            Bucket=BUCKET, Key="aborted.bin", UploadId=upload_id
        )

        resp = s3_client.list_multipart_uploads(Bucket=BUCKET)
        assert resp.get("Uploads", []) == []

    def test_list_uploads_with_prefix(self, s3_client):
        """Prefix filter should only return matching uploads."""
        ids = []
        for key in ["logs/a.bin", "logs/b.bin", "data/c.bin"]:
            create = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
            ids.append((key, create["UploadId"]))

        resp = s3_client.list_multipart_uploads(Bucket=BUCKET, Prefix="logs/")
        uploads = resp.get("Uploads", [])
        assert len(uploads) == 2
        listed_keys = {u["Key"] for u in uploads}
        assert listed_keys == {"logs/a.bin", "logs/b.bin"}

        # Cleanup.
        for key, uid in ids:
            s3_client.abort_multipart_upload(
                Bucket=BUCKET, Key=key, UploadId=uid
            )


class TestMultipartMetadata:
    """Tests for metadata on multipart uploads."""

    def test_multipart_preserves_metadata(self, s3_client):
        """Metadata set on CreateMultipartUpload should be on the final object."""
        metadata = {"project": "arca", "env": "test"}
        create = s3_client.create_multipart_upload(
            Bucket=BUCKET, Key="multi-meta.bin", Metadata=metadata
        )
        upload_id = create["UploadId"]

        part = s3_client.upload_part(
            Bucket=BUCKET,
            Key="multi-meta.bin",
            UploadId=upload_id,
            PartNumber=1,
            Body=b"x" * (5 * 1024 * 1024),
        )
        s3_client.complete_multipart_upload(
            Bucket=BUCKET,
            Key="multi-meta.bin",
            UploadId=upload_id,
            MultipartUpload={
                "Parts": [{"PartNumber": 1, "ETag": part["ETag"]}]
            },
        )

        resp = s3_client.head_object(Bucket=BUCKET, Key="multi-meta.bin")
        assert resp["Metadata"]["project"] == "arca"
        assert resp["Metadata"]["env"] == "test"
