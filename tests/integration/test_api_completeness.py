"""Integration tests for Phase 22 — S3 API Completeness.

Tests ListParts, GetObjectAttributes, checksum headers, and storage class.
"""

import hashlib
import base64
import pytest
import uuid
from botocore.exceptions import ClientError


# -- Helpers --

def make_bucket(s3_client, prefix="api"):
    name = f"{prefix}-{uuid.uuid4().hex[:8]}"
    s3_client.create_bucket(Bucket=name)
    return name


def cleanup_bucket(s3_client, bucket):
    try:
        resp = s3_client.list_objects_v2(Bucket=bucket)
        for obj in resp.get("Contents", []):
            s3_client.delete_object(Bucket=bucket, Key=obj["Key"])
    except Exception:
        pass
    try:
        # Abort any in-progress uploads
        uploads = s3_client.list_multipart_uploads(Bucket=bucket)
        for u in uploads.get("Uploads", []):
            s3_client.abort_multipart_upload(
                Bucket=bucket, Key=u["Key"], UploadId=u["UploadId"]
            )
    except Exception:
        pass
    try:
        s3_client.delete_bucket(Bucket=bucket)
    except Exception:
        pass


# -- ListParts --

class TestListParts:
    def test_list_parts_basic(self, s3_client):
        """ListParts returns uploaded parts with correct metadata."""
        bucket = make_bucket(s3_client)
        try:
            upload = s3_client.create_multipart_upload(Bucket=bucket, Key="multi.bin")
            uid = upload["UploadId"]

            # Upload 3 parts (min 5MB each except last)
            part_data = b"x" * (5 * 1024 * 1024)
            etags = []
            for i in range(1, 4):
                data = part_data if i < 3 else b"final"
                resp = s3_client.upload_part(
                    Bucket=bucket, Key="multi.bin", UploadId=uid,
                    PartNumber=i, Body=data
                )
                etags.append(resp["ETag"])

            # ListParts
            parts_resp = s3_client.list_parts(
                Bucket=bucket, Key="multi.bin", UploadId=uid
            )
            parts = parts_resp["Parts"]
            assert len(parts) == 3
            assert parts[0]["PartNumber"] == 1
            assert parts[1]["PartNumber"] == 2
            assert parts[2]["PartNumber"] == 3
            assert parts[0]["Size"] == 5 * 1024 * 1024
            assert parts[2]["Size"] == 5  # "final"

            s3_client.abort_multipart_upload(
                Bucket=bucket, Key="multi.bin", UploadId=uid
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_list_parts_pagination(self, s3_client):
        """ListParts pagination with MaxParts."""
        bucket = make_bucket(s3_client)
        try:
            upload = s3_client.create_multipart_upload(Bucket=bucket, Key="paged.bin")
            uid = upload["UploadId"]

            # Upload 3 parts
            for i in range(1, 4):
                s3_client.upload_part(
                    Bucket=bucket, Key="paged.bin", UploadId=uid,
                    PartNumber=i, Body=b"data" * 1024
                )

            # List with MaxParts=1
            resp = s3_client.list_parts(
                Bucket=bucket, Key="paged.bin", UploadId=uid, MaxParts=1
            )
            assert len(resp["Parts"]) == 1
            assert resp["IsTruncated"] is True

            s3_client.abort_multipart_upload(
                Bucket=bucket, Key="paged.bin", UploadId=uid
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_list_parts_nonexistent_upload(self, s3_client):
        """ListParts on nonexistent upload returns NoSuchUpload."""
        bucket = make_bucket(s3_client)
        try:
            with pytest.raises(ClientError) as exc_info:
                s3_client.list_parts(
                    Bucket=bucket, Key="nope.bin",
                    UploadId="00000000-0000-0000-0000-000000000000"
                )
            assert exc_info.value.response["Error"]["Code"] == "NoSuchUpload"
        finally:
            cleanup_bucket(s3_client, bucket)


# -- Checksum --

class TestChecksum:
    def test_checksum_sha256_roundtrip(self, s3_client):
        """PutObject with x-amz-checksum-sha256, verify on HEAD."""
        bucket = make_bucket(s3_client)
        try:
            data = b"hello checksum"
            sha = base64.b64encode(hashlib.sha256(data).digest()).decode()

            s3_client.put_object(
                Bucket=bucket, Key="cksum.txt", Body=data,
                ChecksumAlgorithm="SHA256",
                ChecksumSHA256=sha,
            )

            # S3 returns checksums only when asked: ChecksumMode=ENABLED
            head = s3_client.head_object(Bucket=bucket, Key="cksum.txt", ChecksumMode="ENABLED")
            assert head.get("ChecksumSHA256") == sha
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_checksum_crc32_roundtrip(self, s3_client):
        """PutObject with x-amz-checksum-crc32, verify on HEAD."""
        import struct, binascii
        bucket = make_bucket(s3_client)
        try:
            data = b"crc32 test data"
            crc = binascii.crc32(data) & 0xFFFFFFFF
            crc_b64 = base64.b64encode(struct.pack(">I", crc)).decode()

            s3_client.put_object(
                Bucket=bucket, Key="crc.txt", Body=data,
                ChecksumAlgorithm="CRC32",
                ChecksumCRC32=crc_b64,
            )

            head = s3_client.head_object(Bucket=bucket, Key="crc.txt", ChecksumMode="ENABLED")
            assert head.get("ChecksumCRC32") == crc_b64
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_checksum_on_get(self, s3_client):
        """Checksum header is also returned on GetObject."""
        bucket = make_bucket(s3_client)
        try:
            data = b"get checksum"
            sha = base64.b64encode(hashlib.sha256(data).digest()).decode()

            s3_client.put_object(
                Bucket=bucket, Key="getck.txt", Body=data,
                ChecksumAlgorithm="SHA256",
                ChecksumSHA256=sha,
            )

            resp = s3_client.get_object(Bucket=bucket, Key="getck.txt", ChecksumMode="ENABLED")
            assert resp.get("ChecksumSHA256") == sha
        finally:
            cleanup_bucket(s3_client, bucket)


# -- Storage Class --

class TestStorageClass:
    def test_storage_class_default(self, s3_client):
        """Default storage class is STANDARD."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object(Bucket=bucket, Key="default.txt", Body=b"data")
            resp = s3_client.list_objects_v2(Bucket=bucket)
            obj = resp["Contents"][0]
            assert obj["StorageClass"] == "STANDARD"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_storage_class_custom(self, s3_client):
        """Custom storage class is stored and returned."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object(
                Bucket=bucket, Key="cold.txt", Body=b"data",
                StorageClass="GLACIER",
            )
            resp = s3_client.list_objects_v2(Bucket=bucket)
            obj = resp["Contents"][0]
            assert obj["StorageClass"] == "GLACIER"

            # Also check HEAD response
            head = s3_client.head_object(Bucket=bucket, Key="cold.txt")
            assert head.get("StorageClass") == "GLACIER"
        finally:
            cleanup_bucket(s3_client, bucket)


# -- GetObjectAttributes --

class TestGetObjectAttributes:
    def test_get_attributes_etag_and_size(self, s3_client):
        """GetObjectAttributes returns ETag and ObjectSize."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object(Bucket=bucket, Key="attrs.txt", Body=b"hello")
            resp = s3_client.get_object_attributes(
                Bucket=bucket, Key="attrs.txt",
                ObjectAttributes=["ETag", "ObjectSize"],
            )
            assert "ETag" in resp
            assert resp["ObjectSize"] == 5
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_get_attributes_storage_class(self, s3_client):
        """GetObjectAttributes returns StorageClass."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object(
                Bucket=bucket, Key="sc.txt", Body=b"data",
                StorageClass="REDUCED_REDUNDANCY",
            )
            resp = s3_client.get_object_attributes(
                Bucket=bucket, Key="sc.txt",
                ObjectAttributes=["StorageClass"],
            )
            assert resp.get("StorageClass") == "REDUCED_REDUNDANCY"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_get_attributes_nonexistent(self, s3_client):
        """GetObjectAttributes on nonexistent key returns NoSuchKey."""
        bucket = make_bucket(s3_client)
        try:
            with pytest.raises(ClientError) as exc_info:
                s3_client.get_object_attributes(
                    Bucket=bucket, Key="nope.txt",
                    ObjectAttributes=["ETag"],
                )
            assert exc_info.value.response["Error"]["Code"] == "NoSuchKey"
        finally:
            cleanup_bucket(s3_client, bucket)
