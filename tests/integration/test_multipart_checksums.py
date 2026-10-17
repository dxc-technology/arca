"""Integration tests for the checksum and the part layout of multipart objects (TD-046).

An upload created with `x-amz-checksum-algorithm` gives its object a checksum
when it is completed: COMPOSITE (the algorithm over the concatenated part
checksums, `-N` appended) or FULL_OBJECT (the CRC of the whole object, for the
CRC algorithms). The object also keeps the layout of its parts, which
`?partNumber=N` and GetObjectAttributes (`ObjectParts`) report.

The expected values are computed here in plain Python (`checksums.py`),
independently of the server: FULL_OBJECT is the CRC of the whole body, not of
the part CRCs combined the way the server does it.
"""

import os
import re
import uuid
from urllib.parse import quote
from xml.sax.saxutils import escape

import boto3
import pytest
import requests
from botocore.auth import SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.config import Config
from botocore.credentials import Credentials
from botocore.exceptions import ClientError

from checksums import cached, composite

ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
SECRET_KEY = os.environ.get(
    "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
)
MIB = 1024 * 1024
# Every part but the last is at least 5 MiB; 16 bytes x 327680 = exactly 5 MiB.
P1 = b"0123456789abcdef" * 327680
P2 = b"fedcba9876543210" * 327680
TAIL = b"tail-" * 200  # 1000 bytes
ALL_ALGORITHMS = ["CRC32", "CRC32C", "CRC64NVME", "SHA1", "SHA256"]
COMBINATIONS = [
    ("SHA256", "COMPOSITE"),
    ("SHA1", "COMPOSITE"),
    ("CRC32", "COMPOSITE"),
    ("CRC32C", "COMPOSITE"),
    ("CRC32", "FULL_OBJECT"),
    ("CRC32C", "FULL_OBJECT"),
    ("CRC64NVME", "FULL_OBJECT"),
]


# -- Clients and raw requests --


def _client_kwargs(endpoint_url):
    kwargs = dict(
        endpoint_url=endpoint_url,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        region_name="us-east-1",
        # No automatic checksums: botocore cannot compute CRC32C / CRC64NVME
        # without awscrt, and every checksum here is sent explicitly.
        config=Config(
            request_checksum_calculation="when_required",
            response_checksum_validation="when_required",
        ),
    )
    if endpoint_url.startswith("https://") and os.environ.get("AWS_CA_BUNDLE"):
        kwargs["verify"] = os.environ["AWS_CA_BUNDLE"]
    return kwargs


@pytest.fixture
def s3(endpoint_url):
    return boto3.client("s3", **_client_kwargs(endpoint_url))


@pytest.fixture
def bucket(s3):
    name = f"mpck-{uuid.uuid4().hex[:12]}"
    s3.create_bucket(Bucket=name)
    yield name
    try:
        for up in s3.list_multipart_uploads(Bucket=name).get("Uploads", []):
            s3.abort_multipart_upload(Bucket=name, Key=up["Key"], UploadId=up["UploadId"])
        for obj in s3.list_objects_v2(Bucket=name).get("Contents", []):
            s3.delete_object(Bucket=name, Key=obj["Key"])
        s3.delete_bucket(Bucket=name)
    except ClientError:
        pass


def _raw(method, endpoint_url, path, query="", body=b"", headers=None):
    """A SigV4-signed request with exactly the given headers."""
    url = f"{endpoint_url}{quote(path)}" + (f"?{query}" if query else "")
    hdrs = {"X-Amz-Content-SHA256": "UNSIGNED-PAYLOAD"}
    hdrs.update(headers or {})
    request = AWSRequest(method=method, url=url, data=body, headers=hdrs)
    SigV4Auth(Credentials(ACCESS_KEY, SECRET_KEY), "s3", "us-east-1").add_auth(request)
    verify = os.environ.get("AWS_CA_BUNDLE", True) if url.startswith("https") else True
    return requests.request(
        method, url, headers=dict(request.headers), data=body, timeout=60, verify=verify
    )


def _code(resp):
    m = re.search(r"<Code>([^<]+)</Code>", resp.text)
    return m.group(1) if m else None


def _tag(resp, name):
    m = re.search(rf"<{name}>([^<]*)</{name}>", resp.text)
    return m.group(1) if m else None


def _error(exc_info):
    err = exc_info.value.response
    return err["ResponseMetadata"]["HTTPStatusCode"], err["Error"]["Code"]


# -- Building uploads --


def _create(s3, bucket, key, algorithm=None, kind=None):
    args = {"Bucket": bucket, "Key": key}
    if algorithm:
        args["ChecksumAlgorithm"] = algorithm
    if kind:
        args["ChecksumType"] = kind
    return s3.create_multipart_upload(**args)


def _upload_parts(s3, bucket, key, upload_id, algorithm, bodies):
    """Uploads `bodies` as parts 1..N, each with its checksum sent explicitly."""
    parts = []
    for number, body in enumerate(bodies, start=1):
        args = {
            "Bucket": bucket, "Key": key, "UploadId": upload_id,
            "PartNumber": number, "Body": body,
        }
        if algorithm:
            args[f"Checksum{algorithm}"] = cached(algorithm, body)
        resp = s3.upload_part(**args)
        part = {"ETag": resp["ETag"], "PartNumber": number}
        if algorithm:
            assert resp[f"Checksum{algorithm}"] == cached(algorithm, body)
            part[f"Checksum{algorithm}"] = cached(algorithm, body)
        parts.append(part)
    return parts


def _expected(algorithm, kind, bodies):
    """The checksum S3 gives the object made of `bodies`."""
    if kind == "COMPOSITE":
        return composite(algorithm, [cached(algorithm, b) for b in bodies])
    return cached(algorithm, b"".join(bodies))


def _complete(s3, bucket, key, upload_id, parts, **extra):
    return s3.complete_multipart_upload(
        Bucket=bucket, Key=key, UploadId=upload_id,
        MultipartUpload={"Parts": parts}, **extra,
    )


def _make_object(s3, bucket, key, algorithm, kind, bodies):
    upload_id = _create(s3, bucket, key, algorithm, kind)["UploadId"]
    parts = _upload_parts(s3, bucket, key, upload_id, algorithm, bodies)
    return _complete(s3, bucket, key, upload_id, parts), parts


# -- The checksum of the object --


class TestObjectChecksum:
    @pytest.mark.parametrize("algorithm,kind", COMBINATIONS)
    def test_complete_head_get_and_attributes_agree(self, s3, bucket, algorithm, kind):
        bodies = [P1, TAIL]
        done, _ = _make_object(s3, bucket, "k", algorithm, kind, bodies)
        expected = _expected(algorithm, kind, bodies)

        assert done[f"Checksum{algorithm}"] == expected
        assert done["ChecksumType"] == kind

        head = s3.head_object(Bucket=bucket, Key="k", ChecksumMode="ENABLED")
        assert head[f"Checksum{algorithm}"] == expected
        assert head["ChecksumType"] == kind
        # Without checksum mode the checksum is not sent.
        assert f"Checksum{algorithm}" not in s3.head_object(Bucket=bucket, Key="k")

        got = s3.get_object(Bucket=bucket, Key="k", ChecksumMode="ENABLED")
        assert got[f"Checksum{algorithm}"] == expected
        assert got["ChecksumType"] == kind
        assert got["Body"].read() == P1 + TAIL

        attrs = s3.get_object_attributes(Bucket=bucket, Key="k", ObjectAttributes=["Checksum"])
        assert attrs["Checksum"][f"Checksum{algorithm}"] == expected
        assert attrs["Checksum"]["ChecksumType"] == kind

    @pytest.mark.parametrize("algorithm", ["CRC32", "CRC32C", "SHA1", "SHA256"])
    def test_the_default_type_is_composite(self, s3, bucket, algorithm):
        bodies = [P1, TAIL]
        done, _ = _make_object(s3, bucket, "k", algorithm, None, bodies)
        assert done["ChecksumType"] == "COMPOSITE"
        assert done[f"Checksum{algorithm}"] == _expected(algorithm, "COMPOSITE", bodies)

    def test_the_default_type_of_crc64nvme_is_full_object(self, s3, bucket):
        bodies = [P1, TAIL]
        done, _ = _make_object(s3, bucket, "k", "CRC64NVME", None, bodies)
        assert done["ChecksumType"] == "FULL_OBJECT"
        assert done["ChecksumCRC64NVME"] == _expected("CRC64NVME", "FULL_OBJECT", bodies)

    def test_a_single_part_upload_has_a_checksum_too(self, s3, bucket):
        done, _ = _make_object(s3, bucket, "k", "SHA256", "COMPOSITE", [TAIL])
        assert done["ChecksumSHA256"] == composite("SHA256", [cached("SHA256", TAIL)])
        assert done["ChecksumSHA256"].endswith("-1")

    def test_three_parts(self, s3, bucket):
        bodies = [P1, P2, TAIL]
        done, _ = _make_object(s3, bucket, "k", "SHA256", "COMPOSITE", bodies)
        assert done["ChecksumSHA256"] == _expected("SHA256", "COMPOSITE", bodies)
        assert done["ChecksumSHA256"].endswith("-3")

    def test_re_completing_returns_the_same_checksum(self, s3, bucket):
        bodies = [P1, TAIL]
        upload_id = _create(s3, bucket, "k", "SHA256")["UploadId"]
        parts = _upload_parts(s3, bucket, "k", upload_id, "SHA256", bodies)
        first = _complete(s3, bucket, "k", upload_id, parts)
        again = _complete(s3, bucket, "k", upload_id, parts)
        assert again["ChecksumSHA256"] == first["ChecksumSHA256"]
        assert again["ChecksumType"] == "COMPOSITE"

    def test_an_upload_without_an_algorithm_has_no_checksum(self, s3, bucket):
        done, _ = _make_object(s3, bucket, "k", None, None, [P1, TAIL])
        assert not any(k.startswith("Checksum") for k in done if k != "ResponseMetadata")
        head = s3.head_object(Bucket=bucket, Key="k", ChecksumMode="ENABLED")
        assert not any(k.startswith("Checksum") for k in head)

    def test_an_overwritten_object_gets_the_new_checksum(self, s3, bucket):
        s3.put_object(Bucket=bucket, Key="k", Body=b"old")
        done, _ = _make_object(s3, bucket, "k", "SHA256", "COMPOSITE", [TAIL])
        head = s3.head_object(Bucket=bucket, Key="k", ChecksumMode="ENABLED")
        assert head["ChecksumSHA256"] == done["ChecksumSHA256"]


# -- CreateMultipartUpload --


class TestCreate:
    def test_reports_the_algorithm_and_the_type(self, s3, bucket):
        resp = _create(s3, bucket, "k", "CRC32", "FULL_OBJECT")
        assert resp["ChecksumAlgorithm"] == "CRC32"
        assert resp["ChecksumType"] == "FULL_OBJECT"
        resp = _create(s3, bucket, "k2", "SHA256")
        assert resp["ChecksumType"] == "COMPOSITE"

    @pytest.mark.parametrize(
        "algorithm,kind",
        [("SHA256", "FULL_OBJECT"), ("SHA1", "FULL_OBJECT"), ("CRC64NVME", "COMPOSITE")],
    )
    def test_a_combination_that_does_not_exist_is_refused(self, s3, bucket, algorithm, kind):
        with pytest.raises(ClientError) as exc:
            _create(s3, bucket, "k", algorithm, kind)
        assert _error(exc) == (400, "InvalidRequest")

    def test_a_type_without_an_algorithm_is_refused(self, endpoint_url, bucket):
        resp = _raw("POST", endpoint_url, f"/{bucket}/k", "uploads",
                    headers={"x-amz-checksum-type": "COMPOSITE"})
        assert resp.status_code == 400 and _code(resp) == "InvalidRequest"

    def test_an_unknown_algorithm_is_refused(self, endpoint_url, bucket):
        resp = _raw("POST", endpoint_url, f"/{bucket}/k", "uploads",
                    headers={"x-amz-checksum-algorithm": "MD5"})
        assert resp.status_code == 400 and _code(resp) == "InvalidRequest"

    def test_an_unknown_type_is_refused(self, endpoint_url, bucket):
        resp = _raw("POST", endpoint_url, f"/{bucket}/k", "uploads",
                    headers={"x-amz-checksum-algorithm": "CRC32", "x-amz-checksum-type": "WHOLE"})
        assert resp.status_code == 400 and _code(resp) == "InvalidRequest"


# -- UploadPart --


class TestUploadPart:
    def test_a_part_with_another_algorithm_is_refused(self, s3, bucket):
        upload_id = _create(s3, bucket, "k", "SHA256")["UploadId"]
        with pytest.raises(ClientError) as exc:
            s3.upload_part(
                Bucket=bucket, Key="k", UploadId=upload_id, PartNumber=1, Body=TAIL,
                ChecksumCRC32=cached("CRC32", TAIL),
            )
        assert _error(exc) == (400, "InvalidRequest")
        assert "expected checksum Type: sha256" in exc.value.response["Error"]["Message"]
        # Nothing was stored for the part.
        assert s3.list_parts(Bucket=bucket, Key="k", UploadId=upload_id).get("Parts", []) == []

    def test_a_wrong_part_checksum_is_still_refused(self, s3, bucket):
        upload_id = _create(s3, bucket, "k", "SHA256")["UploadId"]
        with pytest.raises(ClientError) as exc:
            s3.upload_part(
                Bucket=bucket, Key="k", UploadId=upload_id, PartNumber=1, Body=TAIL,
                ChecksumSHA256=cached("SHA256", b"other"),
            )
        assert _error(exc) == (400, "BadDigest")

    def test_a_part_without_a_checksum_gets_one_computed(self, s3, endpoint_url, bucket):
        """Clients that never send part checksums keep working."""
        upload_id = _create(s3, bucket, "k", "SHA256")["UploadId"]
        resp = _raw("PUT", endpoint_url, f"/{bucket}/k", f"partNumber=1&uploadId={upload_id}", TAIL)
        assert resp.status_code == 200, resp.text
        assert resp.headers["x-amz-checksum-sha256"] == cached("SHA256", TAIL)
        listed = s3.list_parts(Bucket=bucket, Key="k", UploadId=upload_id)["Parts"]
        assert listed[0]["ChecksumSHA256"] == cached("SHA256", TAIL)
        # ...and the object checksum can be built from it.
        parts = [{"ETag": resp.headers["ETag"], "PartNumber": 1}]
        done = _complete(s3, bucket, "k", upload_id, parts)
        assert done["ChecksumSHA256"] == composite("SHA256", [cached("SHA256", TAIL)])

    def test_an_upload_without_an_algorithm_ignores_part_checksums(self, s3, bucket):
        upload_id = _create(s3, bucket, "k")["UploadId"]
        resp = s3.upload_part(
            Bucket=bucket, Key="k", UploadId=upload_id, PartNumber=1, Body=TAIL,
            ChecksumCRC32=cached("CRC32", TAIL),
        )
        done = _complete(s3, bucket, "k", upload_id, [{"ETag": resp["ETag"], "PartNumber": 1}])
        assert not any(k.startswith("Checksum") for k in done if k != "ResponseMetadata")


# -- CompleteMultipartUpload validation --


class TestCompleteValidation:
    def _ready(self, s3, bucket, algorithm="SHA256", kind=None, bodies=(P1, TAIL)):
        upload_id = _create(s3, bucket, "k", algorithm, kind)["UploadId"]
        parts = _upload_parts(s3, bucket, "k", upload_id, algorithm, list(bodies))
        return upload_id, parts

    def test_a_wrong_object_checksum_is_bad_digest_and_the_upload_survives(self, s3, bucket):
        upload_id, parts = self._ready(s3, bucket)
        with pytest.raises(ClientError) as exc:
            _complete(s3, bucket, "k", upload_id, parts, ChecksumSHA256="bad")
        assert _error(exc) == (400, "BadDigest")
        # Nothing was assembled and the upload can still be completed.
        with pytest.raises(ClientError):
            s3.head_object(Bucket=bucket, Key="k")
        expected = _expected("SHA256", "COMPOSITE", [P1, TAIL])
        done = _complete(s3, bucket, "k", upload_id, parts, ChecksumSHA256=expected)
        assert done["ChecksumSHA256"] == expected

    def test_a_wrong_object_checksum_wins_over_a_missing_part_checksum(self, s3, bucket):
        # Pinned by the Ceph s3-tests (test_multipart_checksum_sha256).
        upload_id, parts = self._ready(s3, bucket, bodies=(TAIL,))
        bare = [{"ETag": parts[0]["ETag"], "PartNumber": 1}]
        with pytest.raises(ClientError) as exc:
            _complete(s3, bucket, "k", upload_id, bare, ChecksumSHA256="bad")
        assert _error(exc) == (400, "BadDigest")

    def test_a_wrong_part_checksum_in_the_xml_is_bad_digest(self, s3, bucket):
        upload_id, parts = self._ready(s3, bucket)
        parts[0]["ChecksumSHA256"] = cached("SHA256", b"other")
        with pytest.raises(ClientError) as exc:
            _complete(s3, bucket, "k", upload_id, parts)
        assert _error(exc) == (400, "BadDigest")
        assert "part 1" in exc.value.response["Error"]["Message"]

    def test_part_checksums_may_be_left_out_of_the_xml(self, s3, bucket):
        upload_id, parts = self._ready(s3, bucket)
        bare = [{"ETag": p["ETag"], "PartNumber": p["PartNumber"]} for p in parts]
        done = _complete(s3, bucket, "k", upload_id, bare)
        assert done["ChecksumSHA256"] == _expected("SHA256", "COMPOSITE", [P1, TAIL])

    def test_the_checksum_type_must_match_the_upload(self, s3, endpoint_url, bucket):
        upload_id, parts = self._ready(s3, bucket, "CRC32", "COMPOSITE")
        resp = _raw("POST", endpoint_url, f"/{bucket}/k", f"uploadId={upload_id}",
                    _complete_xml(parts), {"x-amz-checksum-type": "FULL_OBJECT"})
        assert resp.status_code == 400 and _code(resp) == "InvalidRequest"
        ok = _raw("POST", endpoint_url, f"/{bucket}/k", f"uploadId={upload_id}",
                  _complete_xml(parts), {"x-amz-checksum-type": "COMPOSITE"})
        assert ok.status_code == 200, ok.text

    def test_the_object_size_must_match(self, s3, endpoint_url, bucket):
        upload_id, parts = self._ready(s3, bucket)
        size = len(P1) + len(TAIL)
        wrong = _raw("POST", endpoint_url, f"/{bucket}/k", f"uploadId={upload_id}",
                     _complete_xml(parts), {"x-amz-mp-object-size": str(size + 1)})
        assert wrong.status_code == 400 and _code(wrong) == "InvalidRequest"
        ok = _raw("POST", endpoint_url, f"/{bucket}/k", f"uploadId={upload_id}",
                  _complete_xml(parts), {"x-amz-mp-object-size": str(size)})
        assert ok.status_code == 200, ok.text

    def test_the_full_object_checksum_header_is_checked(self, s3, endpoint_url, bucket):
        upload_id, parts = self._ready(s3, bucket, "CRC32", "FULL_OBJECT")
        whole = cached("CRC32", P1 + TAIL)
        wrong = _raw("POST", endpoint_url, f"/{bucket}/k", f"uploadId={upload_id}",
                     _complete_xml(parts),
                     {"x-amz-checksum-crc32": cached("CRC32", b"other"),
                      "x-amz-checksum-type": "FULL_OBJECT"})
        assert wrong.status_code == 400 and _code(wrong) == "BadDigest"
        ok = _raw("POST", endpoint_url, f"/{bucket}/k", f"uploadId={upload_id}",
                  _complete_xml(parts),
                  {"x-amz-checksum-crc32": whole, "x-amz-checksum-type": "FULL_OBJECT"})
        assert ok.status_code == 200, ok.text
        assert _tag(ok, "ChecksumCRC32") == whole
        assert _tag(ok, "ChecksumType") == "FULL_OBJECT"


def _complete_xml(parts):
    out = "<CompleteMultipartUpload>"
    for p in parts:
        out += f"<Part><PartNumber>{p['PartNumber']}</PartNumber><ETag>{escape(p['ETag'])}</ETag>"
        for algorithm in ALL_ALGORITHMS:
            if f"Checksum{algorithm}" in p:
                out += f"<Checksum{algorithm}>{p[f'Checksum{algorithm}']}</Checksum{algorithm}>"
        out += "</Part>"
    return (out + "</CompleteMultipartUpload>").encode()


# -- The part layout: ?partNumber and ObjectParts --


class TestPartLayout:
    def _object(self, s3, bucket, algorithm="SHA256"):
        bodies = [P1, P2, TAIL]
        kind = "COMPOSITE" if algorithm else None
        done, parts = _make_object(s3, bucket, "k", algorithm, kind, bodies)
        return done, bodies

    def test_get_part_returns_the_bytes_of_that_part(self, s3, bucket):
        done, bodies = self._object(s3, bucket)
        for number, body in enumerate(bodies, start=1):
            got = s3.get_object(Bucket=bucket, Key="k", PartNumber=number)
            assert got["PartsCount"] == 3
            assert got["ETag"] == done["ETag"]
            assert got["ContentLength"] == len(body)
            assert got["ResponseMetadata"]["HTTPStatusCode"] == 206
            assert got["Body"].read() == body

    def test_head_part_reports_the_size_and_the_count(self, s3, bucket):
        done, bodies = self._object(s3, bucket)
        for number, body in enumerate(bodies, start=1):
            head = s3.head_object(Bucket=bucket, Key="k", PartNumber=number)
            assert head["PartsCount"] == 3
            assert head["ContentLength"] == len(body)
            assert head["ETag"] == done["ETag"]

    @pytest.mark.parametrize("algorithm,kind", COMBINATIONS)
    def test_get_part_carries_the_checksum_of_the_part(self, s3, bucket, algorithm, kind):
        bodies = [P1, TAIL]
        _make_object(s3, bucket, "k", algorithm, kind, bodies)
        for number, body in enumerate(bodies, start=1):
            got = s3.get_object(Bucket=bucket, Key="k", PartNumber=number, ChecksumMode="ENABLED")
            assert got[f"Checksum{algorithm}"] == cached(algorithm, body)
            assert got["ChecksumType"] == kind

    def test_a_part_out_of_range_is_invalid_part(self, s3, bucket):
        self._object(s3, bucket)
        for number in (4, 99):
            with pytest.raises(ClientError) as exc:
                s3.get_object(Bucket=bucket, Key="k", PartNumber=number)
            assert _error(exc) == (400, "InvalidPart")
            with pytest.raises(ClientError) as exc:
                s3.head_object(Bucket=bucket, Key="k", PartNumber=number)
            assert exc.value.response["ResponseMetadata"]["HTTPStatusCode"] == 400

    def test_part_zero_and_garbage_are_invalid_arguments(self, endpoint_url, s3, bucket):
        self._object(s3, bucket)
        for value in ("0", "abc", "-1", "10001"):
            resp = _raw("GET", endpoint_url, f"/{bucket}/k", f"partNumber={value}")
            assert resp.status_code == 400 and _code(resp) == "InvalidArgument", value

    def test_a_part_number_and_a_range_together_are_refused(self, endpoint_url, s3, bucket):
        self._object(s3, bucket)
        resp = _raw("GET", endpoint_url, f"/{bucket}/k", "partNumber=1", headers={"Range": "bytes=0-9"})
        assert resp.status_code == 400 and _code(resp) == "InvalidRequest"

    def test_a_plain_object_is_part_one(self, s3, bucket):
        put = s3.put_object(Bucket=bucket, Key="single", Body=b"body")
        got = s3.get_object(Bucket=bucket, Key="single", PartNumber=1)
        assert got["Body"].read() == b"body"
        assert got["ETag"] == put["ETag"]
        assert "PartsCount" not in got
        with pytest.raises(ClientError) as exc:
            s3.get_object(Bucket=bucket, Key="single", PartNumber=2)
        assert _error(exc) == (400, "InvalidPart")

    def test_the_layout_is_kept_without_a_checksum(self, s3, bucket):
        done, bodies = self._object(s3, bucket, algorithm=None)
        got = s3.get_object(Bucket=bucket, Key="k", PartNumber=2)
        assert got["Body"].read() == P2
        attrs = s3.get_object_attributes(Bucket=bucket, Key="k", ObjectAttributes=["ObjectParts"])
        assert [p["Size"] for p in attrs["ObjectParts"]["Parts"]] == [len(b) for b in bodies]
        assert all("ChecksumSHA256" not in p for p in attrs["ObjectParts"]["Parts"])

    def test_object_parts_lists_every_part_with_its_checksum(self, s3, bucket):
        done, bodies = self._object(s3, bucket)
        attrs = s3.get_object_attributes(
            Bucket=bucket, Key="k",
            ObjectAttributes=["ETag", "Checksum", "ObjectParts", "StorageClass", "ObjectSize"],
        )
        assert attrs["ObjectSize"] == sum(len(b) for b in bodies)
        parts = attrs["ObjectParts"]
        assert parts["TotalPartsCount"] == 3
        assert parts["IsTruncated"] is False
        assert parts["MaxParts"] == 1000
        assert [(p["PartNumber"], p["Size"]) for p in parts["Parts"]] == [
            (i, len(b)) for i, b in enumerate(bodies, start=1)
        ]
        assert [p["ChecksumSHA256"] for p in parts["Parts"]] == [cached("SHA256", b) for b in bodies]

    def test_object_parts_paginates(self, s3, bucket):
        self._object(s3, bucket)
        first = s3.get_object_attributes(
            Bucket=bucket, Key="k", ObjectAttributes=["ObjectParts"], MaxParts=1, PartNumberMarker=1,
        )["ObjectParts"]
        assert first["TotalPartsCount"] == 3
        assert first["MaxParts"] == 1
        assert first["PartNumberMarker"] == 1
        assert first["IsTruncated"] is True
        assert first["NextPartNumberMarker"] == 2
        assert [p["PartNumber"] for p in first["Parts"]] == [2]
        rest = s3.get_object_attributes(
            Bucket=bucket, Key="k", ObjectAttributes=["ObjectParts"], MaxParts=10, PartNumberMarker=2,
        )["ObjectParts"]
        assert rest["IsTruncated"] is False
        assert "NextPartNumberMarker" not in rest
        assert [p["PartNumber"] for p in rest["Parts"]] == [3]

    def test_a_plain_object_has_no_object_parts(self, s3, bucket):
        s3.put_object(Bucket=bucket, Key="single", Body=b"body")
        attrs = s3.get_object_attributes(
            Bucket=bucket, Key="single", ObjectAttributes=["ETag", "ObjectParts"],
        )
        assert "ObjectParts" not in attrs


# -- CopyObject --


class TestCopy:
    def test_a_composite_checksum_is_not_copied(self, s3, bucket):
        _make_object(s3, bucket, "src", "SHA256", "COMPOSITE", [P1, TAIL])
        s3.copy_object(Bucket=bucket, Key="dst", CopySource={"Bucket": bucket, "Key": "src"})
        head = s3.head_object(Bucket=bucket, Key="dst", ChecksumMode="ENABLED")
        assert "ChecksumSHA256" not in head
        assert s3.get_object(Bucket=bucket, Key="dst")["Body"].read() == P1 + TAIL

    def test_a_full_object_checksum_is_copied(self, s3, bucket):
        done, _ = _make_object(s3, bucket, "src", "CRC32", "FULL_OBJECT", [P1, TAIL])
        s3.copy_object(Bucket=bucket, Key="dst", CopySource={"Bucket": bucket, "Key": "src"})
        head = s3.head_object(Bucket=bucket, Key="dst", ChecksumMode="ENABLED")
        assert head["ChecksumCRC32"] == done["ChecksumCRC32"]
        assert head["ChecksumType"] == "FULL_OBJECT"

    def test_a_copy_has_no_part_layout(self, s3, bucket):
        _make_object(s3, bucket, "src", "CRC32", "FULL_OBJECT", [P1, TAIL])
        s3.copy_object(Bucket=bucket, Key="dst", CopySource={"Bucket": bucket, "Key": "src"})
        with pytest.raises(ClientError) as exc:
            s3.get_object(Bucket=bucket, Key="dst", PartNumber=2)
        assert _error(exc) == (400, "InvalidPart")
        attrs = s3.get_object_attributes(Bucket=bucket, Key="dst", ObjectAttributes=["ObjectParts"])
        assert "ObjectParts" not in attrs


# -- Real clients --


class TestClients:
    def test_boto3_upload_file_with_its_default_checksums(self, endpoint_url, bucket, tmp_path):
        """The transfer manager's multipart upload, with the SDK defaults on."""
        client = boto3.client(
            "s3", **{**_client_kwargs(endpoint_url), "config": Config(
                request_checksum_calculation="when_supported",
                response_checksum_validation="when_supported",
            )},
        )
        data = os.urandom(11 * MIB)
        path = tmp_path / "big.bin"
        path.write_bytes(data)
        client.upload_file(str(path), bucket, "big")
        got = client.get_object(Bucket=bucket, Key="big")["Body"].read()
        assert got == data
        attrs = client.get_object_attributes(
            Bucket=bucket, Key="big", ObjectAttributes=["Checksum", "ObjectParts", "ObjectSize"],
        )
        assert attrs["ObjectSize"] == len(data)
        assert attrs["ObjectParts"]["TotalPartsCount"] >= 2
        sizes = [p["Size"] for p in attrs["ObjectParts"]["Parts"]]
        assert sum(sizes) == len(data)
