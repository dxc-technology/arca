"""Integration tests for request body integrity (TD-034).

PutObject and UploadPart verify the body against what the request declares,
as AWS S3 does:

- ``Content-MD5``: mismatch -> 400 BadDigest, malformed -> 400 InvalidDigest;
- ``x-amz-checksum-<algo>`` (header, or aws-chunked trailer): mismatch ->
  400 BadDigest, malformed -> 400 InvalidRequest;
- hex ``x-amz-content-sha256``: mismatch -> 400 XAmzContentSHA256Mismatch.

A refused body is never stored. boto3 never sends a wrong digest by itself,
so the negative cases are raw SigV4-signed requests; the positive cases also
use boto3's default upload path (its automatic CRC32, sent as a header over
HTTP and as an aws-chunked trailer over HTTPS).
"""

import base64
import hashlib
import io
import os
import re
import zlib
from urllib.parse import quote

import pytest
import requests
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials
from botocore.exceptions import ClientError


BUCKET = "test-integrity-bucket"
DATA = b"integrity check payload " * 64
OTHER = b"a different payload!!!! " * 64


# -- Digest helpers (CRC32C and CRC64NVME need awscrt in botocore) --

def _crc_reflected(data, poly, init, xorout):
    crc = init
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (poly if crc & 1 else 0)
    return crc ^ xorout


def crc32c(data):
    return _crc_reflected(data, 0x82F63B78, 0xFFFFFFFF, 0xFFFFFFFF)


def crc64nvme(data):
    return _crc_reflected(
        data, 0x9A6C9329AC4BC9B5, 0xFFFFFFFFFFFFFFFF, 0xFFFFFFFFFFFFFFFF,
    )


def b64(raw):
    return base64.b64encode(raw).decode()


CHECKSUMS = {
    "CRC32": lambda d: b64(zlib.crc32(d).to_bytes(4, "big")),
    "CRC32C": lambda d: b64(crc32c(d).to_bytes(4, "big")),
    "CRC64NVME": lambda d: b64(crc64nvme(d).to_bytes(8, "big")),
    "SHA1": lambda d: b64(hashlib.sha1(d).digest()),
    "SHA256": lambda d: b64(hashlib.sha256(d).digest()),
}


def md5_b64(data):
    return b64(hashlib.md5(data).digest())


def test_reference_digests():
    """The helpers reproduce the published check values."""
    assert crc32c(b"123456789") == 0xE3069283
    assert crc64nvme(b"123456789") == 0xAE8B14860A799888
    # Ceph s3-tests vector: 1024 bytes of 'A'.
    assert CHECKSUMS["CRC64NVME"](b"A" * 1024) == "Qeh8oXvGiSo="


# -- Raw signed requests --

def _signed(method, endpoint, path, body=b"", headers=None, query=""):
    """Send a SigV4-signed request with exactly the given headers and body.

    ``x-amz-content-sha256`` defaults to UNSIGNED-PAYLOAD; whatever value is
    set is what gets signed, so a wrong digest still carries a valid
    signature.
    """
    creds = Credentials(
        access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
        ),
    )
    url = f"{endpoint}{quote(path)}" + (f"?{query}" if query else "")
    hdrs = {"X-Amz-Content-SHA256": "UNSIGNED-PAYLOAD"}
    hdrs.update(headers or {})
    req = AWSRequest(method=method, url=url, data=body, headers=hdrs)
    SigV4Auth(creds, "s3", "us-east-1").add_auth(req)
    verify = os.environ.get("AWS_CA_BUNDLE", True) if url.startswith("https") else True
    return requests.request(
        method, url, headers=dict(req.headers), data=body, timeout=30, verify=verify,
    )


def _code(resp):
    m = re.search(r"<Code>([^<]+)</Code>", resp.text)
    return m.group(1) if m else None


def _message(resp):
    m = re.search(r"<Message>([^<]*)</Message>", resp.text)
    return m.group(1) if m else None


def _aws_chunked(data, trailer_name, trailer_value, chunk=4096):
    """Encode `data` as an unsigned aws-chunked body with one trailer."""
    out = b""
    for i in range(0, len(data), chunk):
        piece = data[i:i + chunk]
        out += f"{len(piece):x}\r\n".encode() + piece + b"\r\n"
    out += b"0\r\n" + f"{trailer_name}:{trailer_value}\r\n".encode() + b"\r\n"
    return out


def _trailer_headers(data, trailer_name):
    return {
        "X-Amz-Content-SHA256": "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
        "Content-Encoding": "aws-chunked",
        "X-Amz-Decoded-Content-Length": str(len(data)),
        "X-Amz-Trailer": trailer_name,
    }


def _assert_absent(s3_client, key):
    with pytest.raises(ClientError) as exc:
        s3_client.head_object(Bucket=BUCKET, Key=key)
    assert exc.value.response["Error"]["Code"] in ("404", "NoSuchKey")


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass
    yield
    try:
        for up in s3_client.list_multipart_uploads(Bucket=BUCKET).get("Uploads", []):
            s3_client.abort_multipart_upload(
                Bucket=BUCKET, Key=up["Key"], UploadId=up["UploadId"],
            )
        for obj in s3_client.list_objects_v2(Bucket=BUCKET).get("Contents", []):
            s3_client.delete_object(Bucket=BUCKET, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass


# -- Content-MD5 --

class TestContentMd5:
    def test_correct_md5_is_accepted(self, s3_client):
        s3_client.put_object(
            Bucket=BUCKET, Key="md5-ok", Body=DATA, ContentMD5=md5_b64(DATA),
        )
        body = s3_client.get_object(Bucket=BUCKET, Key="md5-ok")["Body"].read()
        assert body == DATA

    def test_wrong_md5_is_bad_digest_and_not_stored(self, s3_client, endpoint_url):
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/md5-bad", DATA,
                    {"Content-MD5": md5_b64(OTHER)})
        assert r.status_code == 400, r.text
        assert _code(r) == "BadDigest"
        _assert_absent(s3_client, "md5-bad")

    def test_wrong_md5_does_not_replace_existing_object(self, s3_client, endpoint_url):
        s3_client.put_object(Bucket=BUCKET, Key="md5-keep", Body=DATA)
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/md5-keep", OTHER,
                    {"Content-MD5": md5_b64(DATA)})
        assert r.status_code == 400 and _code(r) == "BadDigest"
        body = s3_client.get_object(Bucket=BUCKET, Key="md5-keep")["Body"].read()
        assert body == DATA

    @pytest.mark.parametrize("value", ["not-base64!", "AAAA", b64(b"x" * 17)])
    def test_malformed_md5_is_invalid_digest(self, s3_client, endpoint_url, value):
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/md5-malformed", DATA,
                    {"Content-MD5": value})
        assert r.status_code == 400, r.text
        assert _code(r) == "InvalidDigest"
        _assert_absent(s3_client, "md5-malformed")


# -- x-amz-checksum-* headers --

class TestChecksumHeaders:
    @pytest.mark.parametrize("algo", list(CHECKSUMS))
    def test_correct_checksum_is_stored_and_returned(self, s3_client, endpoint_url, algo):
        key = f"cksum-ok-{algo.lower()}"
        value = CHECKSUMS[algo](DATA)
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/{key}", DATA,
                    {f"x-amz-checksum-{algo.lower()}": value})
        assert r.status_code == 200, r.text
        assert r.headers.get(f"x-amz-checksum-{algo.lower()}") == value

        head = s3_client.head_object(Bucket=BUCKET, Key=key, ChecksumMode="ENABLED")
        assert head[f"Checksum{algo}"] == value
        assert f"Checksum{algo}" not in s3_client.head_object(Bucket=BUCKET, Key=key)

    @pytest.mark.parametrize("algo", list(CHECKSUMS))
    def test_wrong_checksum_is_bad_digest(self, s3_client, endpoint_url, algo):
        key = f"cksum-bad-{algo.lower()}"
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/{key}", DATA,
                    {f"x-amz-checksum-{algo.lower()}": CHECKSUMS[algo](OTHER)})
        assert r.status_code == 400, r.text
        assert _code(r) == "BadDigest"
        assert _message(r) == f"The {algo} you specified did not match the calculated checksum."
        _assert_absent(s3_client, key)

    @pytest.mark.parametrize("algo", list(CHECKSUMS))
    def test_malformed_checksum_is_invalid_request(self, s3_client, endpoint_url, algo):
        key = f"cksum-malformed-{algo.lower()}"
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/{key}", DATA,
                    {f"x-amz-checksum-{algo.lower()}": "bad"})
        assert r.status_code == 400, r.text
        assert _code(r) == "InvalidRequest"
        _assert_absent(s3_client, key)

    def test_two_checksums_are_rejected(self, s3_client, endpoint_url):
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/cksum-two", DATA, {
            "x-amz-checksum-crc32": CHECKSUMS["CRC32"](DATA),
            "x-amz-checksum-sha256": CHECKSUMS["SHA256"](DATA),
        })
        assert r.status_code == 400 and _code(r) == "InvalidRequest"
        _assert_absent(s3_client, "cksum-two")

    def test_boto3_explicit_sha256_checksum(self, s3_client):
        resp = s3_client.put_object(
            Bucket=BUCKET, Key="boto-sha256", Body=DATA, ChecksumAlgorithm="SHA256",
        )
        assert resp["ChecksumSHA256"] == CHECKSUMS["SHA256"](DATA)
        got = s3_client.get_object(Bucket=BUCKET, Key="boto-sha256", ChecksumMode="ENABLED")
        assert got["Body"].read() == DATA
        assert got["ChecksumSHA256"] == CHECKSUMS["SHA256"](DATA)

    def test_get_object_attributes_reports_checksum(self, s3_client, endpoint_url):
        value = CHECKSUMS["SHA1"](DATA)
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/attrs", DATA,
                    {"x-amz-checksum-sha1": value})
        assert r.status_code == 200, r.text
        attrs = s3_client.get_object_attributes(
            Bucket=BUCKET, Key="attrs", ObjectAttributes=["Checksum"],
        )
        assert attrs["Checksum"]["ChecksumSHA1"] == value


# -- hex x-amz-content-sha256 --

class TestContentSha256:
    def test_correct_hex_sha256_is_accepted(self, s3_client, endpoint_url):
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/sha-ok", DATA,
                    {"X-Amz-Content-SHA256": hashlib.sha256(DATA).hexdigest()})
        assert r.status_code == 200, r.text
        assert s3_client.get_object(Bucket=BUCKET, Key="sha-ok")["Body"].read() == DATA

    def test_wrong_hex_sha256_is_mismatch(self, s3_client, endpoint_url):
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/sha-bad", DATA,
                    {"X-Amz-Content-SHA256": hashlib.sha256(OTHER).hexdigest()})
        assert r.status_code == 400, r.text
        assert _code(r) == "XAmzContentSHA256Mismatch"
        _assert_absent(s3_client, "sha-bad")


# -- aws-chunked trailing checksums --

class TestTrailerChecksum:
    @pytest.mark.parametrize("algo", ["CRC32", "CRC32C", "SHA256"])
    def test_correct_trailer_is_stored(self, s3_client, endpoint_url, algo):
        key = f"trailer-ok-{algo.lower()}"
        name = f"x-amz-checksum-{algo.lower()}"
        value = CHECKSUMS[algo](DATA)
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/{key}",
                    _aws_chunked(DATA, name, value), _trailer_headers(DATA, name))
        assert r.status_code == 200, r.text
        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == DATA
        head = s3_client.head_object(Bucket=BUCKET, Key=key, ChecksumMode="ENABLED")
        assert head[f"Checksum{algo}"] == value

    def test_wrong_trailer_is_bad_digest(self, s3_client, endpoint_url):
        name = "x-amz-checksum-crc32"
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/trailer-bad",
                    _aws_chunked(DATA, name, CHECKSUMS["CRC32"](OTHER)),
                    _trailer_headers(DATA, name))
        assert r.status_code == 400, r.text
        assert _code(r) == "BadDigest"
        _assert_absent(s3_client, "trailer-bad")

    def test_missing_trailer_is_invalid_request(self, s3_client, endpoint_url):
        # Declares a CRC32 trailer, then ends the body without it.
        body = f"{len(DATA):x}\r\n".encode() + DATA + b"\r\n0\r\n\r\n"
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/trailer-missing", body,
                    _trailer_headers(DATA, "x-amz-checksum-crc32"))
        assert r.status_code == 400, r.text
        assert _code(r) == "InvalidRequest"
        _assert_absent(s3_client, "trailer-missing")


# -- boto3 defaults (automatic CRC32, header over HTTP, trailer over HTTPS) --

class TestDefaultClients:
    def test_default_put_records_crc32(self, s3_client):
        s3_client.put_object(Bucket=BUCKET, Key="default", Body=DATA)
        head = s3_client.head_object(Bucket=BUCKET, Key="default", ChecksumMode="ENABLED")
        assert head["ChecksumCRC32"] == CHECKSUMS["CRC32"](DATA)
        # botocore validates the returned CRC32 against the body it reads.
        got = s3_client.get_object(Bucket=BUCKET, Key="default", ChecksumMode="ENABLED")
        assert got["Body"].read() == DATA
        assert got["ChecksumCRC32"] == CHECKSUMS["CRC32"](DATA)

    def test_default_multipart_upload_fileobj(self, s3_client):
        from boto3.s3.transfer import TransferConfig
        data = os.urandom(6 * 1024 * 1024)
        s3_client.upload_fileobj(
            io.BytesIO(data), BUCKET, "default-mp",
            Config=TransferConfig(multipart_threshold=5 * 1024 * 1024,
                                  multipart_chunksize=5 * 1024 * 1024),
        )
        assert s3_client.get_object(Bucket=BUCKET, Key="default-mp")["Body"].read() == data

    def test_minio_put_object(self, minio_client):
        # minio-py sends Content-MD5 over HTTPS and a hex sha256 over HTTP.
        minio_client.put_object(BUCKET, "minio", io.BytesIO(DATA), len(DATA))
        resp = minio_client.get_object(BUCKET, "minio")
        try:
            assert resp.read() == DATA
        finally:
            resp.close()
            resp.release_conn()


# -- Multipart parts --

class TestUploadPart:
    def test_part_checksum_is_verified_and_listed(self, s3_client, endpoint_url):
        key = "mp-cksum"
        up = s3_client.create_multipart_upload(
            Bucket=BUCKET, Key=key, ChecksumAlgorithm="CRC32",
        )
        assert up["ChecksumAlgorithm"] == "CRC32"
        uid = up["UploadId"]

        resp = s3_client.upload_part(
            Bucket=BUCKET, Key=key, UploadId=uid, PartNumber=1, Body=DATA,
            ChecksumAlgorithm="CRC32",
        )
        assert resp["ChecksumCRC32"] == CHECKSUMS["CRC32"](DATA)
        parts = s3_client.list_parts(Bucket=BUCKET, Key=key, UploadId=uid)["Parts"]
        assert parts[0]["ChecksumCRC32"] == CHECKSUMS["CRC32"](DATA)

        s3_client.complete_multipart_upload(
            Bucket=BUCKET, Key=key, UploadId=uid,
            MultipartUpload={"Parts": [{
                "PartNumber": 1, "ETag": resp["ETag"],
                "ChecksumCRC32": resp["ChecksumCRC32"],
            }]},
        )
        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == DATA

    @pytest.mark.parametrize("header", ["content-md5", "x-amz-checksum-crc32"])
    def test_wrong_part_digest_is_bad_digest(self, s3_client, endpoint_url, header):
        key = "mp-bad"
        uid = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)["UploadId"]
        value = md5_b64(OTHER) if header == "content-md5" else CHECKSUMS["CRC32"](OTHER)
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/{key}", DATA, {header: value},
                    query=f"partNumber=1&uploadId={uid}")
        assert r.status_code == 400, r.text
        assert _code(r) == "BadDigest"
        parts = s3_client.list_parts(Bucket=BUCKET, Key=key, UploadId=uid)
        assert parts.get("Parts", []) == []

    def test_correct_part_md5_is_accepted(self, s3_client, endpoint_url):
        key = "mp-md5-ok"
        uid = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)["UploadId"]
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/{key}", DATA,
                    {"Content-MD5": md5_b64(DATA)},
                    query=f"partNumber=1&uploadId={uid}")
        assert r.status_code == 200, r.text
        assert r.headers["ETag"].strip('"') == hashlib.md5(DATA).hexdigest()


# -- Other write paths --

class TestOtherPaths:
    def test_sse_c_wrong_md5_is_bad_digest(self, s3_client, endpoint_url):
        key = os.urandom(32)
        sse = {
            "x-amz-server-side-encryption-customer-algorithm": "AES256",
            "x-amz-server-side-encryption-customer-key": b64(key),
            "x-amz-server-side-encryption-customer-key-md5": md5_b64(key),
        }
        r = _signed("PUT", endpoint_url, f"/{BUCKET}/ssec-bad", DATA,
                    {**sse, "Content-MD5": md5_b64(OTHER)})
        assert r.status_code == 400, r.text
        assert _code(r) == "BadDigest"
        _assert_absent(s3_client, "ssec-bad")

        r = _signed("PUT", endpoint_url, f"/{BUCKET}/ssec-ok", DATA,
                    {**sse, "Content-MD5": md5_b64(DATA)})
        assert r.status_code == 200, r.text

    def test_compressed_bucket_verifies_plaintext(self, s3_client, endpoint_url):
        xml = (b'<?xml version="1.0" encoding="UTF-8"?>'
               b"<CompressionConfiguration><Algorithm>zstd</Algorithm>"
               b"</CompressionConfiguration>")
        r = _signed("PUT", endpoint_url, f"/{BUCKET}", xml,
                    {"Content-Type": "application/xml"}, query="compression")
        assert r.status_code == 200, r.text

        r = _signed("PUT", endpoint_url, f"/{BUCKET}/comp-bad", DATA,
                    {"Content-MD5": md5_b64(OTHER), "Content-Type": "text/plain"})
        assert r.status_code == 400 and _code(r) == "BadDigest", r.text
        _assert_absent(s3_client, "comp-bad")

        s3_client.put_object(
            Bucket=BUCKET, Key="comp-ok", Body=DATA, ContentMD5=md5_b64(DATA),
            ContentType="text/plain",
        )
        assert s3_client.get_object(Bucket=BUCKET, Key="comp-ok")["Body"].read() == DATA
