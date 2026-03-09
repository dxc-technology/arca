"""Integration tests for MinIO Python client — streaming and file-based APIs.

These tests exercise MinIO-specific APIs that don't have boto3 equivalents:
streaming uploads/downloads, fput_object/fget_object, and the high-level
put_object with generator/file-like data sources.
"""

import hashlib
import io
import os
import tempfile

import pytest
from minio.error import S3Error


BUCKET = "minio-test-streaming-bucket"


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


class TestStreamingUpload:
    """Tests for streaming upload via put_object with file-like objects."""

    def test_upload_from_bytes_io(self, minio_client):
        """Upload from an in-memory BytesIO stream."""
        data = b"streaming upload test data"
        minio_client.put_object(
            BUCKET, "stream.txt", io.BytesIO(data), len(data),
        )
        resp = minio_client.get_object(BUCKET, "stream.txt")
        try:
            assert resp.read() == data
        finally:
            resp.close()
            resp.release_conn()

    def test_upload_large_stream(self, minio_client):
        """Upload a large stream (>5MB) that triggers multipart internally."""
        size = 6 * 1024 * 1024  # 6 MB
        data = os.urandom(size)
        minio_client.put_object(
            BUCKET, "large-stream.bin", io.BytesIO(data), size,
        )
        resp = minio_client.get_object(BUCKET, "large-stream.bin")
        try:
            downloaded = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert len(downloaded) == size
        assert downloaded == data

    def test_upload_unknown_length(self, minio_client):
        """Upload with length=-1 (unknown size) — MinIO client buffers and uploads."""
        data = b"unknown length upload " * 100
        minio_client.put_object(
            BUCKET, "unknown-len.txt", io.BytesIO(data), -1,
            part_size=5 * 1024 * 1024,
        )
        resp = minio_client.get_object(BUCKET, "unknown-len.txt")
        try:
            assert resp.read() == data
        finally:
            resp.close()
            resp.release_conn()

    def test_upload_empty_object(self, minio_client):
        """Upload a zero-byte object."""
        minio_client.put_object(
            BUCKET, "empty.txt", io.BytesIO(b""), 0,
        )
        stat = minio_client.stat_object(BUCKET, "empty.txt")
        assert stat.size == 0

    def test_upload_with_content_type(self, minio_client):
        """Content-type should be preserved on streaming upload."""
        data = b'{"key": "value"}'
        minio_client.put_object(
            BUCKET, "data.json", io.BytesIO(data), len(data),
            content_type="application/json",
        )
        stat = minio_client.stat_object(BUCKET, "data.json")
        assert stat.content_type == "application/json"

    def test_upload_with_metadata(self, minio_client):
        """User metadata should be preserved on streaming upload."""
        data = b"metadata test"
        minio_client.put_object(
            BUCKET, "meta.txt", io.BytesIO(data), len(data),
            metadata={"project": "arca", "env": "test"},
        )
        stat = minio_client.stat_object(BUCKET, "meta.txt")
        # MinIO client returns metadata keys lowercased with x-amz-meta- prefix stripped.
        assert stat.metadata["x-amz-meta-project"] == "arca"
        assert stat.metadata["x-amz-meta-env"] == "test"


class TestStreamingDownload:
    """Tests for streaming download via get_object."""

    def test_download_full_object(self, minio_client):
        """Download a complete object as a stream."""
        data = b"download test data " * 1000
        minio_client.put_object(
            BUCKET, "download.txt", io.BytesIO(data), len(data),
        )
        resp = minio_client.get_object(BUCKET, "download.txt")
        try:
            downloaded = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert downloaded == data

    def test_download_range(self, minio_client):
        """Download a byte range using offset and length."""
        data = b"0123456789"
        minio_client.put_object(
            BUCKET, "range.txt", io.BytesIO(data), len(data),
        )
        resp = minio_client.get_object(BUCKET, "range.txt", offset=3, length=4)
        try:
            downloaded = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert downloaded == b"3456"

    def test_download_offset_to_end(self, minio_client):
        """Download from offset to end of object."""
        data = b"0123456789"
        minio_client.put_object(
            BUCKET, "range.txt", io.BytesIO(data), len(data),
        )
        resp = minio_client.get_object(BUCKET, "range.txt", offset=7)
        try:
            downloaded = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert downloaded == b"789"

    def test_download_in_chunks(self, minio_client):
        """Download a large object and read it in chunks."""
        size = 1024 * 100  # 100 KB
        data = os.urandom(size)
        minio_client.put_object(
            BUCKET, "chunked.bin", io.BytesIO(data), size,
        )
        resp = minio_client.get_object(BUCKET, "chunked.bin")
        try:
            chunks = []
            while True:
                chunk = resp.read(8192)
                if not chunk:
                    break
                chunks.append(chunk)
        finally:
            resp.close()
            resp.release_conn()
        downloaded = b"".join(chunks)
        assert downloaded == data

    def test_download_nonexistent_object(self, minio_client):
        """Downloading a nonexistent object should raise S3Error."""
        with pytest.raises(S3Error) as exc_info:
            minio_client.get_object(BUCKET, "no-such-key")
        assert exc_info.value.code == "NoSuchKey"


class TestFileBasedOperations:
    """Tests for fput_object and fget_object (file-based upload/download)."""

    def test_fput_and_fget_roundtrip(self, minio_client):
        """Upload a file from disk and download it back."""
        data = os.urandom(4096)
        md5_original = hashlib.md5(data).hexdigest()

        with tempfile.NamedTemporaryFile(delete=False, suffix=".bin") as f:
            f.write(data)
            src_path = f.name

        try:
            minio_client.fput_object(BUCKET, "file-upload.bin", src_path)

            with tempfile.NamedTemporaryFile(delete=False, suffix=".bin") as f:
                dst_path = f.name

            minio_client.fget_object(BUCKET, "file-upload.bin", dst_path)

            with open(dst_path, "rb") as f:
                downloaded = f.read()

            assert hashlib.md5(downloaded).hexdigest() == md5_original
        finally:
            os.unlink(src_path)
            os.unlink(dst_path)

    def test_fput_with_content_type(self, minio_client):
        """fput_object should preserve content-type."""
        with tempfile.NamedTemporaryFile(
            delete=False, suffix=".html", mode="w"
        ) as f:
            f.write("<html><body>hello</body></html>")
            src_path = f.name

        try:
            minio_client.fput_object(
                BUCKET, "page.html", src_path,
                content_type="text/html",
            )
            stat = minio_client.stat_object(BUCKET, "page.html")
            assert stat.content_type == "text/html"
        finally:
            os.unlink(src_path)

    def test_fput_with_metadata(self, minio_client):
        """fput_object should preserve user metadata."""
        with tempfile.NamedTemporaryFile(delete=False, suffix=".txt") as f:
            f.write(b"metadata file test")
            src_path = f.name

        try:
            minio_client.fput_object(
                BUCKET, "meta-file.txt", src_path,
                metadata={"author": "pietro"},
            )
            stat = minio_client.stat_object(BUCKET, "meta-file.txt")
            assert stat.metadata["x-amz-meta-author"] == "pietro"
        finally:
            os.unlink(src_path)

    def test_fput_large_file_triggers_multipart(self, minio_client):
        """Large file upload via fput_object should trigger multipart."""
        size = 11 * 1024 * 1024  # 11 MB
        data = os.urandom(size)

        with tempfile.NamedTemporaryFile(delete=False, suffix=".bin") as f:
            f.write(data)
            src_path = f.name

        try:
            minio_client.fput_object(BUCKET, "large-file.bin", src_path)

            stat = minio_client.stat_object(BUCKET, "large-file.bin")
            assert stat.size == size
            # Multipart ETag contains "-"
            assert "-" in stat.etag

            with tempfile.NamedTemporaryFile(delete=False, suffix=".bin") as f:
                dst_path = f.name

            minio_client.fget_object(BUCKET, "large-file.bin", dst_path)

            with open(dst_path, "rb") as f:
                downloaded = f.read()

            assert len(downloaded) == size
            assert downloaded == data
        finally:
            os.unlink(src_path)
            os.unlink(dst_path)


class TestDataIntegrity:
    """Tests verifying data integrity across upload/download cycles."""

    def test_binary_data_preserved(self, minio_client):
        """All byte values (0x00-0xFF) should survive round-trip."""
        data = bytes(range(256)) * 100
        minio_client.put_object(
            BUCKET, "binary.bin", io.BytesIO(data), len(data),
        )
        resp = minio_client.get_object(BUCKET, "binary.bin")
        try:
            downloaded = resp.read()
        finally:
            resp.close()
            resp.release_conn()
        assert downloaded == data

    def test_etag_matches_md5(self, minio_client):
        """For single-part uploads, ETag should match MD5 of content."""
        data = b"etag verification test"
        expected_md5 = hashlib.md5(data).hexdigest()

        minio_client.put_object(
            BUCKET, "etag-check.txt", io.BytesIO(data), len(data),
        )
        stat = minio_client.stat_object(BUCKET, "etag-check.txt")
        assert stat.etag == expected_md5

    def test_concurrent_uploads_to_different_keys(self, minio_client):
        """Multiple objects uploaded sequentially should all be retrievable."""
        objects = {}
        for i in range(10):
            data = f"object-{i}-data".encode()
            key = f"concurrent/obj-{i}.txt"
            minio_client.put_object(
                BUCKET, key, io.BytesIO(data), len(data),
            )
            objects[key] = data

        for key, expected_data in objects.items():
            resp = minio_client.get_object(BUCKET, key)
            try:
                assert resp.read() == expected_data
            finally:
                resp.close()
                resp.release_conn()
