"""Integration tests for the copy-source conditional headers.

`x-amz-copy-source-if-match`, `-if-none-match`, `-if-modified-since` and
`-if-unmodified-since` are evaluated against the source object by both
CopyObject and UploadPartCopy (TD-035: UploadPartCopy ignored them). A failed
condition is a 412 and nothing is copied.
"""

import datetime
import uuid

import pytest
from botocore.exceptions import ClientError

PAST = datetime.datetime(2000, 1, 1, tzinfo=datetime.timezone.utc)
FUTURE = datetime.datetime(2100, 1, 1, tzinfo=datetime.timezone.utc)
BODY = b"copy-source-conditions" * 100


@pytest.fixture
def source(s3_client):
    """A bucket with one source object; yields (bucket, key, etag)."""
    bucket = f"test-copy-cond-{uuid.uuid4().hex[:8]}"
    s3_client.create_bucket(Bucket=bucket)
    etag = s3_client.put_object(Bucket=bucket, Key="src", Body=BODY)["ETag"]
    yield bucket, "src", etag
    for obj in s3_client.list_objects_v2(Bucket=bucket).get("Contents", []):
        s3_client.delete_object(Bucket=bucket, Key=obj["Key"])
    for up in s3_client.list_multipart_uploads(Bucket=bucket).get("Uploads", []):
        s3_client.abort_multipart_upload(
            Bucket=bucket, Key=up["Key"], UploadId=up["UploadId"]
        )
    s3_client.delete_bucket(Bucket=bucket)


def _copy_object(s3_client, bucket, key, **conditions):
    s3_client.copy_object(
        Bucket=bucket,
        Key="dst",
        CopySource={"Bucket": bucket, "Key": key},
        **conditions,
    )


def _upload_part_copy(s3_client, bucket, key, **conditions):
    upload_id = s3_client.create_multipart_upload(Bucket=bucket, Key="mp")["UploadId"]
    try:
        s3_client.upload_part_copy(
            Bucket=bucket,
            Key="mp",
            UploadId=upload_id,
            PartNumber=1,
            CopySource={"Bucket": bucket, "Key": key},
            **conditions,
        )
        parts = s3_client.list_parts(Bucket=bucket, Key="mp", UploadId=upload_id)
        return len(parts.get("Parts", []))
    finally:
        s3_client.abort_multipart_upload(Bucket=bucket, Key="mp", UploadId=upload_id)


def _parts_after_refusal(s3_client, bucket, key, **conditions):
    """Runs an UploadPartCopy that must be refused; returns how many parts the
    upload holds afterwards."""
    upload_id = s3_client.create_multipart_upload(Bucket=bucket, Key="mp")["UploadId"]
    try:
        with pytest.raises(ClientError) as exc:
            s3_client.upload_part_copy(
                Bucket=bucket,
                Key="mp",
                UploadId=upload_id,
                PartNumber=1,
                CopySource={"Bucket": bucket, "Key": key},
                **conditions,
            )
        assert exc.value.response["Error"]["Code"] == "PreconditionFailed"
        parts = s3_client.list_parts(Bucket=bucket, Key="mp", UploadId=upload_id)
        return len(parts.get("Parts", []))
    finally:
        s3_client.abort_multipart_upload(Bucket=bucket, Key="mp", UploadId=upload_id)


# (header kwarg, value that passes, value that fails), by the source's ETag.
def _cases(etag):
    return [
        ("CopySourceIfMatch", etag, '"deadbeef"'),
        ("CopySourceIfNoneMatch", '"deadbeef"', etag),
        ("CopySourceIfModifiedSince", PAST, FUTURE),
        ("CopySourceIfUnmodifiedSince", FUTURE, PAST),
    ]


CASE_NAMES = [
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
]


@pytest.mark.parametrize("index", range(4), ids=CASE_NAMES)
class TestCopyObjectConditions:
    def test_passing_condition_copies(self, s3_client, source, index):
        bucket, key, etag = source
        name, passes, _ = _cases(etag)[index]
        _copy_object(s3_client, bucket, key, **{name: passes})
        assert s3_client.get_object(Bucket=bucket, Key="dst")["Body"].read() == BODY

    def test_failing_condition_is_412_and_copies_nothing(self, s3_client, source, index):
        bucket, key, etag = source
        name, _, fails = _cases(etag)[index]
        with pytest.raises(ClientError) as exc:
            _copy_object(s3_client, bucket, key, **{name: fails})
        assert exc.value.response["Error"]["Code"] == "PreconditionFailed"
        with pytest.raises(ClientError) as missing:
            s3_client.head_object(Bucket=bucket, Key="dst")
        assert missing.value.response["Error"]["Code"] in ("404", "NoSuchKey")


@pytest.mark.parametrize("index", range(4), ids=CASE_NAMES)
class TestUploadPartCopyConditions:
    def test_passing_condition_copies_the_part(self, s3_client, source, index):
        bucket, key, etag = source
        name, passes, _ = _cases(etag)[index]
        assert _upload_part_copy(s3_client, bucket, key, **{name: passes}) == 1

    def test_failing_condition_is_412_and_stores_no_part(self, s3_client, source, index):
        bucket, key, etag = source
        name, _, fails = _cases(etag)[index]
        assert _parts_after_refusal(s3_client, bucket, key, **{name: fails}) == 0
