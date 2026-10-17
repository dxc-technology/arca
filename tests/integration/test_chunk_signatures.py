"""Integration tests for the signatures of aws-chunked bodies (TD-045).

A ``STREAMING-AWS4-HMAC-SHA256-PAYLOAD`` body (and its ``-TRAILER`` variant)
is authenticated as a chain: the request signature seeds the signature of the
first chunk, every chunk signs its data and the one before it, and a trailer
signs its headers and the last chunk. PutObject and UploadPart verify the
chain while the body streams, and a body that does not verify is never stored
and never replaces what is there.

The signer below is written from the AWS specification with plain ``hmac`` and
``hashlib``, independently of the server's implementation. boto3 only signs
chunks over plain HTTP, so the requests are built by hand.
"""

import base64
import hashlib
import hmac
import os
import re
import uuid
import zlib
from urllib.parse import quote

import boto3
import pytest
import requests
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.config import Config
from botocore.credentials import Credentials
from botocore.exceptions import ClientError


ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
SECRET_KEY = os.environ.get(
    "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
)
REGION = "us-east-1"
PAYLOAD = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"
PAYLOAD_TRAILER = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER"
DATA = b"0123456789abcdef" * 8192  # 128 KiB: two 64 KiB chunks
OTHER = b"a different payload " * 1024
CHUNK = 64 * 1024


# -- An independent aws-chunked signer --


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


def _hmac(key, message):
    return hmac.new(key, message.encode(), hashlib.sha256).digest()


def _signing_key(date):
    key = _hmac(("AWS4" + SECRET_KEY).encode(), date)
    for part in (REGION, "s3", "aws4_request"):
        key = _hmac(key, part)
    return key


def crc32_b64(data):
    return base64.b64encode(zlib.crc32(data).to_bytes(4, "big")).decode()


class ChunkedUpload:
    """A signed aws-chunked request: headers, and a body built chunk by chunk."""

    def __init__(self, endpoint, path, data, query="", trailer=None, chunk=CHUNK):
        self.data = data
        self.chunk = chunk
        self.trailer = trailer  # (header name, value) or None
        url = f"{endpoint}{quote(path)}" + (f"?{query}" if query else "")
        headers = {
            "X-Amz-Content-SHA256": PAYLOAD_TRAILER if trailer else PAYLOAD,
            "Content-Encoding": "aws-chunked",
            "X-Amz-Decoded-Content-Length": str(len(data)),
        }
        if trailer:
            headers["X-Amz-Trailer"] = trailer[0]
        request = AWSRequest(method="PUT", url=url, data=b"", headers=headers)
        SigV4Auth(Credentials(ACCESS_KEY, SECRET_KEY), "s3", REGION).add_auth(request)
        self.url = url
        self.headers = dict(request.headers)
        self.amz_date = self.headers["X-Amz-Date"]
        self.scope = f"{self.amz_date[:8]}/{REGION}/s3/aws4_request"
        self.seed = re.search(r"Signature=([0-9a-f]{64})", self.headers["Authorization"]).group(1)

    def _chunk_signature(self, previous, data):
        sts = "\n".join(
            ["AWS4-HMAC-SHA256-PAYLOAD", self.amz_date, self.scope, previous, _sha256(b""), _sha256(data)]
        )
        return _hmac(_signing_key(self.amz_date[:8]), sts).hex()

    def _trailer_signature(self, previous, canonical):
        sts = "\n".join(
            ["AWS4-HMAC-SHA256-TRAILER", self.amz_date, self.scope, previous, _sha256(canonical)]
        )
        return _hmac(_signing_key(self.amz_date[:8]), sts).hex()

    def body(self):
        """The well-formed signed body; tests tamper with the result."""
        out, previous = b"", self.seed
        pieces = [self.data[i:i + self.chunk] for i in range(0, len(self.data), self.chunk)]
        for piece in pieces + [b""]:
            previous = self._chunk_signature(previous, piece)
            out += f"{len(piece):x};chunk-signature={previous}\r\n".encode()
            if piece:
                out += piece + b"\r\n"
        if self.trailer:
            name, value = self.trailer
            canonical = f"{name.lower()}:{value}\n".encode()
            sig = self._trailer_signature(previous, canonical)
            out += f"{name.lower()}:{value}\r\nx-amz-trailer-signature:{sig}\r\n".encode()
        return out + b"\r\n"

    def send(self, body=None):
        body = self.body() if body is None else body
        return requests.put(
            self.url,
            headers=self.headers,
            data=body,
            timeout=30,
            verify=os.environ.get("AWS_CA_BUNDLE", True) if self.url.startswith("https") else True,
        )


def _code(resp):
    m = re.search(r"<Code>([^<]+)</Code>", resp.text)
    return m.group(1) if m else None


def _flip(body, needle, offset=0):
    """`body` with one byte changed at the first occurrence of `needle`."""
    pos = body.index(needle) + offset
    return body[:pos] + bytes([body[pos] ^ 1]) + body[pos + 1:]


# -- Fixtures --


@pytest.fixture
def bucket(s3_client):
    name = f"chunk-sig-{uuid.uuid4().hex[:12]}"
    s3_client.create_bucket(Bucket=name)
    yield name
    try:
        for up in s3_client.list_multipart_uploads(Bucket=name).get("Uploads", []):
            s3_client.abort_multipart_upload(Bucket=name, Key=up["Key"], UploadId=up["UploadId"])
        for obj in s3_client.list_objects_v2(Bucket=name).get("Contents", []):
            s3_client.delete_object(Bucket=name, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=name)
    except ClientError:
        pass


def _absent(s3_client, bucket, key):
    with pytest.raises(ClientError) as exc:
        s3_client.head_object(Bucket=bucket, Key=key)
    assert exc.value.response["Error"]["Code"] in ("404", "NoSuchKey")


def _assert_refused(resp):
    assert resp.status_code == 403, resp.text
    assert _code(resp) == "SignatureDoesNotMatch"


# -- PutObject --


class TestSignedChunks:
    def test_a_correctly_signed_body_is_stored(self, s3_client, endpoint_url, bucket):
        resp = ChunkedUpload(endpoint_url, f"/{bucket}/ok", DATA).send()
        assert resp.status_code == 200, resp.text
        assert s3_client.get_object(Bucket=bucket, Key="ok")["Body"].read() == DATA

    def test_a_small_and_an_empty_body_are_stored(self, s3_client, endpoint_url, bucket):
        for key, data in (("small", b"hello"), ("empty", b"")):
            resp = ChunkedUpload(endpoint_url, f"/{bucket}/{key}", data).send()
            assert resp.status_code == 200, resp.text
            assert s3_client.get_object(Bucket=bucket, Key=key)["Body"].read() == data

    def test_a_modified_data_byte_is_refused_and_not_stored(self, s3_client, endpoint_url, bucket):
        upload = ChunkedUpload(endpoint_url, f"/{bucket}/tampered", DATA)
        _assert_refused(upload.send(_flip(upload.body(), b"0123456789abcdef", 3)))
        _absent(s3_client, bucket, "tampered")

    def test_a_modified_chunk_signature_is_refused(self, s3_client, endpoint_url, bucket):
        upload = ChunkedUpload(endpoint_url, f"/{bucket}/badsig", DATA)
        _assert_refused(upload.send(_flip(upload.body(), b";chunk-signature=", 17)))
        _absent(s3_client, bucket, "badsig")

    def test_a_tampered_upload_does_not_replace_an_existing_object(self, s3_client, endpoint_url, bucket):
        s3_client.put_object(Bucket=bucket, Key="keep", Body=OTHER)
        upload = ChunkedUpload(endpoint_url, f"/{bucket}/keep", DATA)
        _assert_refused(upload.send(_flip(upload.body(), b"0123456789abcdef")))
        assert s3_client.get_object(Bucket=bucket, Key="keep")["Body"].read() == OTHER

    def test_a_body_with_the_signatures_stripped_is_refused(self, s3_client, endpoint_url, bucket):
        upload = ChunkedUpload(endpoint_url, f"/{bucket}/stripped", b"hello")
        stripped = b"5\r\nhello\r\n0\r\n\r\n"
        _assert_refused(upload.send(stripped))
        _absent(s3_client, bucket, "stripped")

    def test_a_body_cut_before_the_final_chunk_is_refused(self, s3_client, endpoint_url, bucket):
        # Every chunk that is left carries a valid signature; only the missing
        # final chunk gives the truncation away.
        upload = ChunkedUpload(endpoint_url, f"/{bucket}/cut", DATA)
        body = upload.body()
        final = body.rindex(b"\r\n0;chunk-signature=") + 2
        _assert_refused(upload.send(body[:final]))
        _absent(s3_client, bucket, "cut")

    @pytest.mark.parametrize("where", ["inside the final header", "inside a chunk", "inside a chunk header"])
    def test_a_body_cut_anywhere_is_refused(self, s3_client, endpoint_url, bucket, where):
        upload = ChunkedUpload(endpoint_url, f"/{bucket}/cut2", DATA)
        body = upload.body()
        cut = {
            "inside the final header": body.rindex(b"\r\n0;chunk-signature=") + 2 + 20,
            "inside a chunk": 200,
            "inside a chunk header": 20,
        }[where]
        _assert_refused(upload.send(body[:cut]))
        _absent(s3_client, bucket, "cut2")

    def test_chunks_signed_for_another_object_are_refused(self, s3_client, endpoint_url, bucket):
        # A valid chain for one request replayed under another request's seed.
        first = ChunkedUpload(endpoint_url, f"/{bucket}/first", DATA)
        second = ChunkedUpload(endpoint_url, f"/{bucket}/second", DATA)
        _assert_refused(second.send(first.body()))
        _absent(s3_client, bucket, "second")


class TestSignedTrailer:
    def _upload(self, endpoint_url, bucket, key, data=DATA, value=None):
        value = value or crc32_b64(data)
        return ChunkedUpload(
            endpoint_url, f"/{bucket}/{key}", data, trailer=("x-amz-checksum-crc32", value)
        )

    def test_a_correctly_signed_trailer_is_stored(self, s3_client, endpoint_url, bucket):
        resp = self._upload(endpoint_url, bucket, "ok").send()
        assert resp.status_code == 200, resp.text
        got = s3_client.get_object(Bucket=bucket, Key="ok", ChecksumMode="ENABLED")
        assert got["Body"].read() == DATA
        assert got["ChecksumCRC32"] == crc32_b64(DATA)

    def test_a_modified_trailer_value_is_refused(self, s3_client, endpoint_url, bucket):
        upload = self._upload(endpoint_url, bucket, "trailer")
        body = upload.body().replace(crc32_b64(DATA).encode(), crc32_b64(OTHER).encode())
        _assert_refused(upload.send(body))
        _absent(s3_client, bucket, "trailer")

    def test_a_missing_trailer_signature_is_refused(self, s3_client, endpoint_url, bucket):
        upload = self._upload(endpoint_url, bucket, "nosig")
        body = upload.body()
        body = body[: body.index(b"x-amz-trailer-signature")] + b"\r\n"
        _assert_refused(upload.send(body))
        _absent(s3_client, bucket, "nosig")

    def test_a_modified_data_byte_is_refused(self, s3_client, endpoint_url, bucket):
        upload = self._upload(endpoint_url, bucket, "data")
        _assert_refused(upload.send(_flip(upload.body(), b"0123456789abcdef")))
        _absent(s3_client, bucket, "data")

    def test_a_correct_signature_does_not_excuse_a_wrong_checksum(self, s3_client, endpoint_url, bucket):
        # The trailer is signed as sent, but names the checksum of other data:
        # authentic, and still refused by the checksum check (TD-034).
        upload = self._upload(endpoint_url, bucket, "wrongcrc", value=crc32_b64(OTHER))
        resp = upload.send()
        assert resp.status_code == 400, resp.text
        assert _code(resp) == "BadDigest"
        _absent(s3_client, bucket, "wrongcrc")


# -- UploadPart --


class TestSignedParts:
    def _create(self, s3_client, bucket):
        return s3_client.create_multipart_upload(Bucket=bucket, Key="mp")["UploadId"]

    def test_a_signed_part_is_stored_and_completes(self, s3_client, endpoint_url, bucket):
        upload_id = self._create(s3_client, bucket)
        part = ChunkedUpload(
            endpoint_url, f"/{bucket}/mp", DATA, query=f"partNumber=1&uploadId={upload_id}"
        ).send()
        assert part.status_code == 200, part.text
        s3_client.complete_multipart_upload(
            Bucket=bucket,
            Key="mp",
            UploadId=upload_id,
            MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part.headers["ETag"]}]},
        )
        assert s3_client.get_object(Bucket=bucket, Key="mp")["Body"].read() == DATA

    def test_a_modified_part_is_refused_and_not_stored(self, s3_client, endpoint_url, bucket):
        upload_id = self._create(s3_client, bucket)
        upload = ChunkedUpload(
            endpoint_url, f"/{bucket}/mp", DATA, query=f"partNumber=1&uploadId={upload_id}"
        )
        _assert_refused(upload.send(_flip(upload.body(), b"0123456789abcdef", 5)))
        parts = s3_client.list_parts(Bucket=bucket, Key="mp", UploadId=upload_id).get("Parts", [])
        assert parts == []


# -- No seed signature --


def test_signed_chunks_with_a_presigned_url_are_refused(s3_client, endpoint_url, bucket):
    """Query-string auth has no seed signature to chain from."""
    kwargs = dict(
        endpoint_url=endpoint_url,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        region_name=REGION,
        config=Config(signature_version="s3v4"),  # boto3 presigns with SigV2 by default
    )
    if endpoint_url.startswith("https://") and os.environ.get("AWS_CA_BUNDLE"):
        kwargs["verify"] = os.environ["AWS_CA_BUNDLE"]
    url = boto3.client("s3", **kwargs).generate_presigned_url(
        "put_object", Params={"Bucket": bucket, "Key": "presigned"}, ExpiresIn=60
    )
    fake = ChunkedUpload(endpoint_url, f"/{bucket}/presigned", DATA)
    headers = {
        "X-Amz-Content-SHA256": PAYLOAD,
        "Content-Encoding": "aws-chunked",
        "X-Amz-Decoded-Content-Length": str(len(DATA)),
    }
    resp = requests.put(
        url,
        headers=headers,
        data=fake.body(),
        timeout=30,
        verify=os.environ.get("AWS_CA_BUNDLE", True) if url.startswith("https") else True,
    )
    _assert_refused(resp)
    _absent(s3_client, bucket, "presigned")
