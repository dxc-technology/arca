"""Integration tests for Arca Phase 27 — Transparent At-Rest Compression.

Compression is a console-only feature: it's always available at runtime but
only activates when a bucket has a `?compression` configuration set.

These tests are executed via `bin/test compression`.
"""

import hashlib
import os

import boto3
import pytest
import requests
from botocore.exceptions import ClientError


BUCKET = "test-compression-bucket"
BUCKET_PER_ALGO = "test-compression-algo-bucket"


def _signed_s3(method, url, data=b""):
    """Sign an S3 request with SigV4 and send it via `requests`."""
    from botocore.auth import S3SigV4Auth
    from botocore.awsrequest import AWSRequest
    from botocore.credentials import Credentials
    creds = Credentials(
        access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
        ),
    )
    headers = {}
    if method == "PUT":
        headers["Content-Type"] = "application/xml"
    req = AWSRequest(method=method, url=url, data=data, headers=headers)
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(req)
    return requests.request(
        method, url, headers=dict(req.headers), data=data, timeout=10
    )


def _enable_compression(endpoint, bucket, algorithm="auto"):
    """Turn on compression for a bucket via the Arca extension subresource."""
    xml = (
        f"""<?xml version="1.0" encoding="UTF-8"?>
<CompressionConfiguration>
  <Algorithm>{algorithm}</Algorithm>
</CompressionConfiguration>""".encode()
    )
    r = _signed_s3("PUT", f"{endpoint}/{bucket}?compression", xml)
    assert r.status_code == 200, f"enable compression failed: {r.text}"


@pytest.fixture(autouse=True)
def setup_bucket(s3_client, endpoint_url):
    """Ensure test buckets exist with compression enabled, clean up after."""
    for b in [BUCKET, BUCKET_PER_ALGO]:
        try:
            s3_client.create_bucket(Bucket=b)
        except ClientError:
            pass
    # BUCKET always has auto-compression on; the per-algo bucket is empty so
    # individual tests can install their own config.
    _enable_compression(endpoint_url, BUCKET, "auto")
    yield
    for b in [BUCKET, BUCKET_PER_ALGO]:
        try:
            response = s3_client.list_objects_v2(Bucket=b)
            for obj in response.get("Contents", []):
                s3_client.delete_object(Bucket=b, Key=obj["Key"])
            s3_client.delete_bucket(Bucket=b)
        except ClientError:
            pass


class TestCompressedPutGet:
    """Basic compressed object put/get roundtrip tests."""

    def test_put_get_compressible_json(self, s3_client):
        """Highly compressible JSON round-trips byte-for-byte."""
        data = (b'{"greeting":"hello world"}\n' * 2000)
        s3_client.put_object(
            Bucket=BUCKET, Key="blob.json", Body=data,
            ContentType="application/json",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="blob.json")
        assert resp["Body"].read() == data

    def test_etag_matches_plaintext_md5(self, s3_client):
        """ETag must stay the plaintext MD5, not the compressed MD5."""
        data = b"A" * 8192  # very compressible
        expected_md5 = hashlib.md5(data).hexdigest()
        s3_client.put_object(
            Bucket=BUCKET, Key="etag.txt", Body=data,
            ContentType="text/plain",
        )
        resp = s3_client.head_object(Bucket=BUCKET, Key="etag.txt")
        assert resp["ETag"].strip('"') == expected_md5
        assert resp["ContentLength"] == len(data)

    def test_small_object_passthrough(self, s3_client):
        """Objects below min_size should not be compressed, but still round-trip."""
        data = b"tiny"  # under min_size (1024)
        s3_client.put_object(
            Bucket=BUCKET, Key="small.txt", Body=data,
            ContentType="text/plain",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="small.txt")
        assert resp["Body"].read() == data

    def test_already_compressed_mime_skipped(self, s3_client):
        """Objects declaring image/* should bypass compression and still round-trip."""
        data = os.urandom(50_000)
        s3_client.put_object(
            Bucket=BUCKET, Key="fake.png", Body=data,
            ContentType="image/png",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="fake.png")
        assert resp["Body"].read() == data

    def test_large_compressible_roundtrip(self, s3_client):
        """200 KB of repetitive text — spans multiple frames."""
        data = (b"the quick brown fox jumps over the lazy dog. " * 5000)
        s3_client.put_object(
            Bucket=BUCKET, Key="lorem.txt", Body=data,
            ContentType="text/plain",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="lorem.txt")
        assert resp["Body"].read() == data


class TestCompressedRangeRead:
    """Byte range reads on compressed objects."""

    def test_range_read_inside_first_chunk(self, s3_client):
        data = b"abcdefghijklmnopqrstuvwxyz" * 100  # 2600 bytes
        s3_client.put_object(
            Bucket=BUCKET, Key="range-simple", Body=data,
            ContentType="text/plain",
        )
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="range-simple", Range="bytes=10-19",
        )
        assert resp["Body"].read() == data[10:20]

    def test_range_cross_frame_boundary(self, s3_client):
        # Larger than one 64 KiB frame.
        data = (b"word " * 20_000)  # 100_000 bytes
        s3_client.put_object(
            Bucket=BUCKET, Key="range-cross", Body=data,
            ContentType="text/plain",
        )
        start, end = 65_000, 66_500
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="range-cross",
            Range=f"bytes={start}-{end}",
        )
        assert resp["Body"].read() == data[start:end + 1]

    def test_range_open_ended_and_suffix(self, s3_client):
        """`bytes=N-` and `bytes=-N` reach the last (partial) frame."""
        data = (b"the quick brown fox jumps over the lazy dog. " * 5000)
        s3_client.put_object(
            Bucket=BUCKET, Key="range-tail", Body=data,
            ContentType="text/plain",
        )
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="range-tail", Range="bytes=150000-",
        )
        assert resp["Body"].read() == data[150_000:]
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="range-tail", Range="bytes=-100",
        )
        assert resp["Body"].read() == data[-100:]


class TestCompressedIncompressibleAndCopies:
    """Bodies that grow when compressed, multipart uploads and copies."""

    def test_incompressible_body_roundtrip(self, s3_client):
        """Random bytes declared as text are still framed and compressed,
        so the stored payload is LARGER than the object: full and ranged
        reads must both work."""
        data = os.urandom(200_000)
        s3_client.put_object(
            Bucket=BUCKET, Key="random.txt", Body=data,
            ContentType="text/plain",
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="random.txt")
        assert resp["Body"].read() == data
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="random.txt", Range="bytes=65000-140000",
        )
        assert resp["Body"].read() == data[65_000:140_001]
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="random.txt", Range="bytes=-10",
        )
        assert resp["Body"].read() == data[-10:]

    def test_multipart_upload_in_compressed_bucket(self, s3_client):
        """Parts are stored uncompressed; the assembled object round-trips."""
        part1 = b"multipart part one " * 300_000  # > 5 MiB minimum part size
        part2 = b"tail part"
        mpu = s3_client.create_multipart_upload(
            Bucket=BUCKET, Key="mpu.txt", ContentType="text/plain",
        )
        upload_id = mpu["UploadId"]
        etags = []
        for n, body in enumerate([part1, part2], start=1):
            r = s3_client.upload_part(
                Bucket=BUCKET, Key="mpu.txt", UploadId=upload_id,
                PartNumber=n, Body=body,
            )
            etags.append({"PartNumber": n, "ETag": r["ETag"]})
        s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key="mpu.txt", UploadId=upload_id,
            MultipartUpload={"Parts": etags},
        )
        data = part1 + part2
        resp = s3_client.get_object(Bucket=BUCKET, Key="mpu.txt")
        assert resp["Body"].read() == data
        start = len(part1) - 5
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="mpu.txt", Range=f"bytes={start}-",
        )
        assert resp["Body"].read() == data[start:]

    def test_copy_compressed_object(self, s3_client):
        """CopyObject reads the compressed source and writes a new object."""
        data = (b'{"copy":"me"}\n' * 10_000)
        s3_client.put_object(
            Bucket=BUCKET, Key="copy-src.json", Body=data,
            ContentType="application/json",
        )
        s3_client.copy_object(
            Bucket=BUCKET, Key="copy-dst.json",
            CopySource={"Bucket": BUCKET, "Key": "copy-src.json"},
        )
        resp = s3_client.get_object(Bucket=BUCKET, Key="copy-dst.json")
        assert resp["Body"].read() == data
        resp = s3_client.get_object(
            Bucket=BUCKET, Key="copy-dst.json", Range="bytes=70000-70099",
        )
        assert resp["Body"].read() == data[70_000:70_100]


@pytest.mark.skipif(
    not os.environ.get("ARCA_ENCRYPTION_ENABLED"),
    reason="needs a server with global encryption (bin/test compression, second pass)",
)
class TestCompressedOnEncryptedServer:
    """The suite also runs on a server with SSE-S3 enabled: compression then
    sits above encryption. Check the objects really are encrypted there."""

    def test_compressed_object_is_encrypted(self, s3_client):
        data = (b"compressed and encrypted " * 4000)
        put = s3_client.put_object(
            Bucket=BUCKET, Key="enc.txt", Body=data, ContentType="text/plain",
        )
        assert put.get("ServerSideEncryption") == "AES256"
        resp = s3_client.get_object(Bucket=BUCKET, Key="enc.txt")
        assert resp.get("ServerSideEncryption") == "AES256"
        assert resp["Body"].read() == data


class TestPerBucketCompression:
    """Tests for PUT/GET/DELETE /{bucket}?compression."""

    def _sign_and_put_xml(self, endpoint, bucket, xml):
        return _signed_s3("PUT", f"{endpoint}/{bucket}?compression", xml)

    def _sign_and_delete(self, endpoint, bucket):
        return _signed_s3("DELETE", f"{endpoint}/{bucket}?compression", b"")

    def _sign_and_get(self, endpoint, bucket):
        return _signed_s3("GET", f"{endpoint}/{bucket}?compression", b"")

    def test_put_get_delete_roundtrip(self, s3_client, endpoint_url):
        xml = b"""<?xml version="1.0" encoding="UTF-8"?>
<CompressionConfiguration>
  <Algorithm>zstd</Algorithm>
  <Level>5</Level>
</CompressionConfiguration>"""
        r = self._sign_and_put_xml(endpoint_url, BUCKET_PER_ALGO, xml)
        assert r.status_code == 200, r.text

        r = self._sign_and_get(endpoint_url, BUCKET_PER_ALGO)
        assert r.status_code == 200
        assert b"<Algorithm>zstd</Algorithm>" in r.content

        r = self._sign_and_delete(endpoint_url, BUCKET_PER_ALGO)
        assert r.status_code == 204

        r = self._sign_and_get(endpoint_url, BUCKET_PER_ALGO)
        # After delete, GET should return a 400-series error (no config).
        assert r.status_code >= 400

    def test_per_algorithm_roundtrip_all_codecs(self, s3_client, endpoint_url):
        """Force each algorithm via per-bucket config and verify roundtrip."""
        for algo in ["zstd", "lz4", "snappy", "gzip", "brotli", "xz"]:
            xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<CompressionConfiguration>
  <Algorithm>{algo}</Algorithm>
</CompressionConfiguration>""".encode()
            r = self._sign_and_put_xml(endpoint_url, BUCKET_PER_ALGO, xml)
            assert r.status_code == 200, f"{algo} put: {r.text}"
            data = (f"payload for {algo} " * 2000).encode()
            s3_client.put_object(
                Bucket=BUCKET_PER_ALGO, Key=f"obj-{algo}.txt", Body=data,
                ContentType="text/plain",
            )
            resp = s3_client.get_object(Bucket=BUCKET_PER_ALGO, Key=f"obj-{algo}.txt")
            assert resp["Body"].read() == data

    def test_rejects_unknown_algorithm(self, s3_client, endpoint_url):
        xml = b"""<?xml version="1.0" encoding="UTF-8"?>
<CompressionConfiguration>
  <Algorithm>nope</Algorithm>
</CompressionConfiguration>"""
        r = self._sign_and_put_xml(endpoint_url, BUCKET_PER_ALGO, xml)
        assert r.status_code >= 400
