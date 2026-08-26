"""Integration tests for conditional-write atomicity (compare-and-swap).

Regression coverage for the check-then-commit race fixed by making every
conditional write atomic at commit (early header check as an optimisation
only, authoritative check inside the same transaction as the write). See
`.claude/plans/arca-conditional-write-atomicity.md`.

Sequential tests cover the S3 semantics that had no coverage at all before
this fix. Concurrent tests reproduce the race from the vulnerability report:
before the fix, N racing writers against the same `If-Match` ETag could all
observe a "match" and all commit, instead of exactly one winning.
"""

import threading
import time
import uuid

import boto3
import pytest
from botocore.config import Config
from botocore.exceptions import ClientError


BUCKET = "test-conditional-writes"


@pytest.fixture(autouse=True)
def setup_bucket(s3_client):
    """Ensure the test bucket exists before each test, and clean up after."""
    try:
        s3_client.create_bucket(Bucket=BUCKET)
    except ClientError:
        pass
    yield
    try:
        response = s3_client.list_objects_v2(Bucket=BUCKET)
        for obj in response.get("Contents", []):
            s3_client.delete_object(Bucket=BUCKET, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=BUCKET)
    except ClientError:
        pass


def _client(endpoint_url):
    """A fresh S3 client with retries disabled, for use across worker threads.

    Each racing writer gets its own client (own connection) so the barrier
    release is not serialized behind a shared connection pool.
    """
    return boto3.client(
        "s3",
        endpoint_url=endpoint_url,
        aws_access_key_id="AKIA5B6BSHJA8CIHZSVG",
        aws_secret_access_key="hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv",
        region_name="us-east-1",
        config=Config(
            signature_version="s3v4",
            max_pool_connections=50,
            retries={"max_attempts": 0},
        ),
    )


def _status(exc):
    return exc.response["ResponseMetadata"]["HTTPStatusCode"]


# ── Sequential PutObject ─────────────────────────────────────────────────────


class TestSequentialPutIfMatch:
    def test_matching_etag_succeeds(self, s3_client):
        key = f"seq-if-match-ok-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v1")
        etag = s3_client.head_object(Bucket=BUCKET, Key=key)["ETag"]

        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v2", IfMatch=etag)

        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == b"v2"

    def test_stale_etag_returns_412_and_writes_nothing(self, s3_client):
        key = f"seq-if-match-stale-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v1")
        stale_etag = s3_client.head_object(Bucket=BUCKET, Key=key)["ETag"]
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v2")

        with pytest.raises(ClientError) as exc_info:
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v3", IfMatch=stale_etag)
        assert _status(exc_info.value) == 412

        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == b"v2"

    def test_if_match_on_absent_key_returns_404(self, s3_client):
        key = f"seq-if-match-absent-{uuid.uuid4().hex[:8]}"
        with pytest.raises(ClientError) as exc_info:
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v1", IfMatch='"anything"')
        assert _status(exc_info.value) == 404


class TestSequentialPutIfNoneMatch:
    def test_star_on_absent_key_succeeds(self, s3_client):
        key = f"seq-if-none-match-absent-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"created", IfNoneMatch="*")
        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == b"created"

    def test_star_on_existing_key_returns_412_and_writes_nothing(self, s3_client):
        key = f"seq-if-none-match-existing-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v1")

        with pytest.raises(ClientError) as exc_info:
            s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v2", IfNoneMatch="*")
        assert _status(exc_info.value) == 412

        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == b"v1"


# ── Sequential CompleteMultipartUpload ───────────────────────────────────────


def _single_part_upload(s3_client, key, body):
    """Starts a multipart upload and uploads its (only, so any-size) part."""
    mpu = s3_client.create_multipart_upload(Bucket=BUCKET, Key=key)
    upload_id = mpu["UploadId"]
    part = s3_client.upload_part(
        Bucket=BUCKET, Key=key, UploadId=upload_id, PartNumber=1, Body=body,
    )
    return upload_id, part["ETag"]


class TestSequentialCompleteMultipartUploadConditionals:
    def test_if_match_success(self, s3_client):
        key = f"seq-cmu-if-match-ok-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"base")
        etag = s3_client.head_object(Bucket=BUCKET, Key=key)["ETag"]

        upload_id, part_etag = _single_part_upload(s3_client, key, b"assembled")
        s3_client.complete_multipart_upload(
            Bucket=BUCKET,
            Key=key,
            UploadId=upload_id,
            MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part_etag}]},
            IfMatch=etag,
        )

        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == b"assembled"

    def test_if_none_match_star_returns_412_and_parts_stay_retryable(self, s3_client):
        key = f"seq-cmu-if-none-match-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"already here")

        upload_id, part_etag = _single_part_upload(s3_client, key, b"assembled")
        with pytest.raises(ClientError) as exc_info:
            s3_client.complete_multipart_upload(
                Bucket=BUCKET,
                Key=key,
                UploadId=upload_id,
                MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part_etag}]},
                IfNoneMatch="*",
            )
        assert _status(exc_info.value) == 412

        # The object is untouched...
        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == b"already here"
        # ...and the already-uploaded part must still be resolvable for a retry.
        parts = s3_client.list_parts(Bucket=BUCKET, Key=key, UploadId=upload_id)
        assert len(parts["Parts"]) == 1

        s3_client.abort_multipart_upload(Bucket=BUCKET, Key=key, UploadId=upload_id)


# ── Sequential DeleteObject ───────────────────────────────────────────────────


class TestSequentialDeleteConditionals:
    def test_if_match_success(self, s3_client):
        key = f"seq-del-if-match-ok-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v1")
        etag = s3_client.head_object(Bucket=BUCKET, Key=key)["ETag"]

        s3_client.delete_object(Bucket=BUCKET, Key=key, IfMatch=etag)

        with pytest.raises(ClientError) as exc_info:
            s3_client.head_object(Bucket=BUCKET, Key=key)
        assert _status(exc_info.value) == 404

    def test_stale_if_match_returns_412_and_object_survives(self, s3_client):
        key = f"seq-del-if-match-stale-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v1")
        stale_etag = s3_client.head_object(Bucket=BUCKET, Key=key)["ETag"]
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"v2")

        with pytest.raises(ClientError) as exc_info:
            s3_client.delete_object(Bucket=BUCKET, Key=key, IfMatch=stale_etag)
        assert _status(exc_info.value) == 412

        assert s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read() == b"v2"

    def test_if_match_size_mismatch_returns_412(self, s3_client):
        key = f"seq-del-if-match-size-{uuid.uuid4().hex[:8]}"
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=b"12345")

        with pytest.raises(ClientError) as exc_info:
            s3_client.delete_object(Bucket=BUCKET, Key=key, IfMatchSize=999)
        assert _status(exc_info.value) == 412
        s3_client.head_object(Bucket=BUCKET, Key=key)  # still present


# ── Concurrent PutObject If-Match ────────────────────────────────────────────


class TestConcurrentPutIfMatch:
    """N=8 writers racing the same base ETag: exactly one must win, every run."""

    N = 8
    RUNS = 10
    PAYLOAD_SIZE = 1024 * 1024

    def test_exactly_one_writer_wins_per_run(self, endpoint_url):
        key = f"cas-race-{uuid.uuid4().hex[:8]}"
        seed = _client(endpoint_url)

        for run in range(self.RUNS):
            seed.put_object(Bucket=BUCKET, Key=key, Body=b"seed")
            etag = seed.head_object(Bucket=BUCKET, Key=key)["ETag"]

            bodies = [bytes([65 + i]) * self.PAYLOAD_SIZE for i in range(self.N)]
            codes = [None] * self.N
            gate = threading.Barrier(self.N)

            def writer(i):
                cl = _client(endpoint_url)
                gate.wait()
                try:
                    cl.put_object(Bucket=BUCKET, Key=key, Body=bodies[i], IfMatch=etag)
                    codes[i] = 200
                except ClientError as e:
                    codes[i] = _status(e)

            threads = [threading.Thread(target=writer, args=(i,)) for i in range(self.N)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            assert codes.count(200) == 1, f"run {run}: codes={codes}"
            assert codes.count(412) == self.N - 1, f"run {run}: codes={codes}"

            winner = codes.index(200)
            final = seed.get_object(Bucket=BUCKET, Key=key)["Body"].read()
            assert final == bodies[winner], f"run {run}: final content is not the winner's payload"


class TestConcurrentPutIfNoneMatchStar:
    """N=8 writers racing to create the same absent key: exactly one must win."""

    N = 8
    RUNS = 10
    PAYLOAD_SIZE = 1024 * 1024

    def test_exactly_one_creator_wins_per_run(self, endpoint_url):
        key = f"cas-create-{uuid.uuid4().hex[:8]}"
        cleanup = _client(endpoint_url)

        for run in range(self.RUNS):
            try:
                cleanup.delete_object(Bucket=BUCKET, Key=key)
            except ClientError:
                pass

            codes = [None] * self.N
            gate = threading.Barrier(self.N)

            def writer(i):
                cl = _client(endpoint_url)
                body = bytes([65 + i]) * self.PAYLOAD_SIZE
                gate.wait()
                try:
                    cl.put_object(Bucket=BUCKET, Key=key, Body=body, IfNoneMatch="*")
                    codes[i] = 200
                except ClientError as e:
                    codes[i] = _status(e)

            threads = [threading.Thread(target=writer, args=(i,)) for i in range(self.N)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            assert codes.count(200) == 1, f"run {run}: codes={codes}"
            assert codes.count(412) == self.N - 1, f"run {run}: codes={codes}"


class TestConcurrentCompleteMultipartUploadIfMatch:
    """Two CompleteMultipartUpload calls racing the same base ETag."""

    RUNS = 3

    def test_one_wins_and_losers_parts_stay_retryable(self, endpoint_url):
        key = f"cmu-race-{uuid.uuid4().hex[:8]}"
        seed = _client(endpoint_url)

        for _ in range(self.RUNS):
            seed.put_object(Bucket=BUCKET, Key=key, Body=b"base")
            base_etag = seed.head_object(Bucket=BUCKET, Key=key)["ETag"]

            cl_a, cl_b = _client(endpoint_url), _client(endpoint_url)
            uid_a, part_etag_a = _single_part_upload(cl_a, key, b"A" * 1024)
            uid_b, part_etag_b = _single_part_upload(cl_b, key, b"B" * 1024)

            results = {}
            gate = threading.Barrier(2)

            def complete(name, cl, upload_id, part_etag):
                gate.wait()
                try:
                    cl.complete_multipart_upload(
                        Bucket=BUCKET,
                        Key=key,
                        UploadId=upload_id,
                        MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part_etag}]},
                        IfMatch=base_etag,
                    )
                    results[name] = 200
                except ClientError as e:
                    results[name] = _status(e)

            threads = [
                threading.Thread(target=complete, args=("a", cl_a, uid_a, part_etag_a)),
                threading.Thread(target=complete, args=("b", cl_b, uid_b, part_etag_b)),
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            codes = list(results.values())
            assert codes.count(200) == 1, f"results={results}"
            assert codes.count(412) == 1, f"results={results}"

            if results["a"] == 412:
                loser_client, loser_upload_id = cl_a, uid_a
            else:
                loser_client, loser_upload_id = cl_b, uid_b

            # The loser's part must still be resolvable so the client can retry.
            parts = loser_client.list_parts(Bucket=BUCKET, Key=key, UploadId=loser_upload_id)
            assert len(parts["Parts"]) == 1

            loser_client.abort_multipart_upload(Bucket=BUCKET, Key=key, UploadId=loser_upload_id)
            seed.delete_object(Bucket=BUCKET, Key=key)


class TestConditionalDeleteRacesOverwrite:
    """A stale If-Match DELETE racing an unconditional overwrite must never
    destroy the newer object — regardless of which side wins the race."""

    RUNS = 5

    def test_stale_delete_never_destroys_newer_object(self, endpoint_url):
        key = f"del-race-{uuid.uuid4().hex[:8]}"
        seed = _client(endpoint_url)

        for _ in range(self.RUNS):
            seed.put_object(Bucket=BUCKET, Key=key, Body=b"old")
            stale_etag = seed.head_object(Bucket=BUCKET, Key=key)["ETag"]

            results = {}
            gate = threading.Barrier(2)

            def deleter():
                cl = _client(endpoint_url)
                gate.wait()
                try:
                    cl.delete_object(Bucket=BUCKET, Key=key, IfMatch=stale_etag)
                    results["del"] = 204
                except ClientError as e:
                    results["del"] = _status(e)

            def overwriter():
                cl = _client(endpoint_url)
                gate.wait()
                # Give the deleter a head start past its early check, so a
                # non-atomic implementation would race its commit against
                # this write.
                time.sleep(0.002)
                cl.put_object(Bucket=BUCKET, Key=key, Body=b"NEW-IMPORTANT-DATA")

            threads = [threading.Thread(target=deleter), threading.Thread(target=overwriter)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            try:
                body = seed.get_object(Bucket=BUCKET, Key=key)["Body"].read()
            except ClientError:
                body = None

            assert body == b"NEW-IMPORTANT-DATA", (
                f"data loss: delete_result={results.get('del')} surviving_body={body!r}"
            )


# ── DeleteObjects batch: version-specific preconditions target the right version ──


class TestDeleteObjectsBatchTargetsExactVersion:
    """Regression pin for the M3 fix: a version-specific batch delete must
    check the precondition against the TARGETED version, not the bucket's
    current latest object."""

    def test_precondition_checked_against_targeted_version_not_latest(self, s3_client):
        bucket = f"cond-del-batch-{uuid.uuid4().hex[:8]}"
        s3_client.create_bucket(Bucket=bucket)
        s3_client.put_bucket_versioning(
            Bucket=bucket, VersioningConfiguration={"Status": "Enabled"},
        )
        try:
            key = "obj"
            s3_client.put_object(Bucket=bucket, Key=key, Body=b"v1")
            old_version = s3_client.list_object_versions(Bucket=bucket)["Versions"][0]
            old_etag = old_version["ETag"]
            old_version_id = old_version["VersionId"]

            # Latest is now v2, with a different ETag than old_etag.
            s3_client.put_object(Bucket=bucket, Key=key, Body=b"v2-longer-body")

            resp = s3_client.delete_objects(
                Bucket=bucket,
                Delete={
                    "Objects": [
                        {"Key": key, "VersionId": old_version_id, "ETag": old_etag},
                    ],
                },
            )

            # Must succeed: the ETag matches the TARGETED (old) version, even
            # though it does not match the bucket's current latest object.
            assert resp.get("Errors", []) == [], resp.get("Errors")
            assert len(resp.get("Deleted", [])) == 1

            # The old version is gone; the latest (v2) survives untouched.
            remaining = s3_client.list_object_versions(Bucket=bucket)["Versions"]
            assert all(v["VersionId"] != old_version_id for v in remaining)
            assert s3_client.get_object(Bucket=bucket, Key=key)["Body"].read() == b"v2-longer-body"
        finally:
            response = s3_client.list_object_versions(Bucket=bucket)
            for v in response.get("Versions", []):
                s3_client.delete_object(Bucket=bucket, Key=v["Key"], VersionId=v["VersionId"])
            for dm in response.get("DeleteMarkers", []):
                s3_client.delete_object(Bucket=bucket, Key=dm["Key"], VersionId=dm["VersionId"])
            s3_client.delete_bucket(Bucket=bucket)
