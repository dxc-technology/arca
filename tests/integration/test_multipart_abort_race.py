"""Integration tests for AbortMultipartUpload racing CompleteMultipartUpload.

Regression coverage for TD-053: CompleteMultipartUpload used to commit the
object row and only afterwards consume the upload, so an abort landing in
between (or just before the commit) deleted the parts of the object just
completed. A completed object whose parts were deleted is unreadable. Now the
row and the consumption of the upload are one transaction: whichever commits
first wins.

The deterministic guarantees are unit-tested on the metadata stores
(`complete_multipart_object_*`). The concurrent test here is a stress test of
the invariant through the real server: it narrows the window but cannot
force it, so a pass proves absence of the failure in N tries, not in general.
"""

import threading
import time
import uuid

import boto3
import pytest
from botocore.config import Config
from botocore.exceptions import ClientError


BUCKET = "test-multipart-abort-race"
BODY = b"arca-td053-" * 1000
RACE_ROUNDS = 60
# The abort is released after a delay that grows each round, sweeping the
# whole life of a Complete: a bare simultaneous start always lets the (much
# cheaper) abort win, which exercises only one side of the race.
RACE_STEP_SECONDS = 0.0002


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass
    yield
    try:
        for obj in s3_client.list_objects_v2(Bucket=BUCKET).get("Contents", []):
            s3_client.delete_object(Bucket=BUCKET, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass


def _client(endpoint_url):
    """A fresh client per racing thread: its own connection, no retries."""
    return boto3.client(
        "s3",
        endpoint_url=endpoint_url,
        aws_access_key_id="AKIA5B6BSHJA8CIHZSVG",
        aws_secret_access_key="hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv",
        region_name="us-east-1",
        config=Config(
            signature_version="s3v4",
            max_pool_connections=10,
            retries={"max_attempts": 0},
        ),
    )


def _code(exc):
    return exc.response["Error"]["Code"]


def _upload(s3_client, key):
    """Starts an upload with its one (so any-size) part; returns what Complete needs."""
    upload_id = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)["UploadId"]
    etag = s3_client.upload_part(
        Bucket=BUCKET, Key=key, UploadId=upload_id, PartNumber=1, Body=BODY,
    )["ETag"]
    return upload_id, {"Parts": [{"PartNumber": 1, "ETag": etag}]}


def _complete(client, key, upload_id, parts):
    return client.complete_multipart_upload(
        Bucket=BUCKET, Key=key, UploadId=upload_id, MultipartUpload=parts,
    )


class TestSequentialAbortAndComplete:
    def test_abort_after_complete_leaves_the_object_readable(self, s3_client):
        key = f"abort-after-complete-{uuid.uuid4().hex[:8]}"
        upload_id, parts = _upload(s3_client, key)
        _complete(s3_client, key, upload_id, parts)

        try:
            s3_client.abort_multipart_upload(Bucket=BUCKET, Key=key, UploadId=upload_id)
        except ClientError as e:
            assert _code(e) == "NoSuchUpload"

        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == BODY

    def test_complete_after_abort_fails_and_writes_nothing(self, s3_client):
        key = f"complete-after-abort-{uuid.uuid4().hex[:8]}"
        upload_id, parts = _upload(s3_client, key)
        s3_client.abort_multipart_upload(Bucket=BUCKET, Key=key, UploadId=upload_id)

        with pytest.raises(ClientError) as exc_info:
            _complete(s3_client, key, upload_id, parts)
        assert _code(exc_info.value) == "NoSuchUpload"

        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(Bucket=BUCKET, Key=key)
        assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 404


class TestAbortRacingComplete:
    def test_a_completed_object_always_keeps_its_data(self, s3_client, endpoint_url):
        """Whoever wins, an object that Complete reported as created is whole.

        Each round releases a complete and, a little later (the delay sweeps
        from 0 up), an abort of the same upload. Either Complete wins (200, the object must read back
        intact, the abort finds nothing to delete) or the abort wins (Complete
        answers NoSuchUpload and no object exists). Never: 200 and a broken
        object.
        """
        completed = aborted = 0
        for round_number in range(RACE_ROUNDS):
            abort_delay = round_number * RACE_STEP_SECONDS
            key = f"race-{uuid.uuid4().hex[:8]}"
            upload_id, parts = _upload(s3_client, key)
            barrier = threading.Barrier(2)
            outcome = {}

            def run(name, call, delay=0.0):
                client = _client(endpoint_url)
                barrier.wait()
                time.sleep(delay)
                try:
                    call(client)
                    outcome[name] = "ok"
                except ClientError as e:
                    outcome[name] = _code(e)

            threads = [
                threading.Thread(
                    target=run,
                    args=("complete", lambda c: _complete(c, key, upload_id, parts)),
                ),
                threading.Thread(
                    target=run,
                    args=(
                        "abort",
                        lambda c: c.abort_multipart_upload(
                            Bucket=BUCKET, Key=key, UploadId=upload_id
                        ),
                        abort_delay,
                    ),
                ),
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            assert outcome["complete"] in ("ok", "NoSuchUpload"), outcome
            assert outcome["abort"] in ("ok", "NoSuchUpload"), outcome
            if outcome["complete"] == "ok":
                completed += 1
                body = s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read()
                assert body == BODY, f"completed object lost its data: {outcome}"
            else:
                aborted += 1
                with pytest.raises(ClientError) as exc_info:
                    s3_client.head_object(Bucket=BUCKET, Key=key)
                assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 404
        print(f"races: {completed} completed, {aborted} aborted")
