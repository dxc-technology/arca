"""Integration tests for Phase 30: HOT re-encryption maintenance jobs.

Drives the `/admin/maintenance/*` admin API to encrypt and then decrypt
existing plain objects on a running Arca, proving the copy-on-write
re-encryption works end-to-end against the S3 data path:

  * an ``encrypt`` job re-encrypts every plain object (new blob, swaps the
    row to ``encryption_algorithm = AES256``);
  * a ``decrypt`` job reverses it.

The server is started by ``bin/test recrypt`` with a master key present but
global encryption disabled (the per-bucket fragment), so plain PUTs land
unencrypted while the server is still able to encrypt on demand.

Encryption is detected via the boto3 ``ServerSideEncryption`` field on
head_object/get_object responses (the ``x-amz-server-side-encryption``
header), exactly as in test_encryption.py / test_per_bucket_encryption.py.
"""

import json
import os
import time

import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials
from botocore.exceptions import ClientError


# Skip the module unless launched via `bin/test recrypt` (master key present,
# global encryption off — plain default puts that the server can re-encrypt).
pytestmark = pytest.mark.skipif(
    not os.environ.get("ARCA_PER_BUCKET_ENCRYPTION"),
    reason="Re-encryption tests require ARCA_PER_BUCKET_ENCRYPTION (bin/test recrypt)",
)


# A fixed bucket name (delete-and-recreate per run keeps cleanup trivial and
# the test deterministic — no random/time-based names).
BUCKET = "test-recrypt-bucket"

# Three objects with known, distinct contents (a few KB each).
OBJECTS = {
    "alpha.bin": b"alpha-" + (b"A" * 4096),
    "beta.bin": b"beta-" + (b"B" * 4096),
    "gamma.bin": b"gamma-" + (b"C" * 4096),
}


# --- maintenance admin-API helpers (mirrors test_maintenance.py) ---


@pytest.fixture
def endpoint(endpoint_url):
    return endpoint_url


@pytest.fixture
def creds():
    return Credentials(
        access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
        ),
    )


def signed_request(method, url, creds, data=None):
    headers = {}
    if data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data) if isinstance(data, dict) else data
    aws_req = AWSRequest(method=method, url=url, data=data or "", headers=headers)
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)
    return requests.request(
        method, url, headers=dict(aws_req.headers), data=data, timeout=10
    )


def jobs_url(endpoint):
    return f"{endpoint}/admin/maintenance/jobs"


def cancel_active(endpoint, creds):
    """Best-effort: cancel any active job so the single-job slot is free."""
    resp = signed_request("GET", jobs_url(endpoint), creds)
    if resp.status_code != 200:
        return
    active = resp.json().get("active")
    if active:
        signed_request("DELETE", f"{jobs_url(endpoint)}/{active['id']}", creds)


@pytest.fixture(autouse=True)
def clean_slot(endpoint, creds):
    """Free the single-job slot before and after each test."""
    cancel_active(endpoint, creds)
    yield
    cancel_active(endpoint, creds)


def wait_for_status(endpoint, creds, job_id, statuses, timeout=60.0):
    """Poll a job until it reaches one of `statuses` (or timeout).

    Re-encryption progresses on a ~1s worker tick, so the default timeout is
    generous (60s).
    """
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        resp = signed_request("GET", f"{jobs_url(endpoint)}/{job_id}", creds)
        assert resp.status_code == 200, resp.text
        last = resp.json()["job"]
        if last["status"] in statuses:
            return last
        time.sleep(0.5)
    raise AssertionError(
        f"job {job_id} did not reach {statuses}; "
        f"last status={last['status'] if last else '?'} "
        f"done={last.get('done') if last else '?'} "
        f"total={last.get('total') if last else '?'} "
        f"last_error={last.get('last_error') if last else '?'}"
    )


# --- bucket lifecycle ---


@pytest.fixture
def bucket(s3_client):
    """Provide an empty, plain (no per-bucket encryption) bucket per run.

    Delete-and-recreate so the test is self-cleaning and deterministic.
    """
    _empty_and_delete(s3_client, BUCKET)
    s3_client.create_bucket(Bucket=BUCKET)
    yield BUCKET
    _empty_and_delete(s3_client, BUCKET)


def _empty_and_delete(s3_client, bucket):
    try:
        resp = s3_client.list_objects_v2(Bucket=bucket)
        for obj in resp.get("Contents", []):
            s3_client.delete_object(Bucket=bucket, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=bucket)
    except ClientError:
        pass


def _put_objects(s3_client, bucket):
    for key, data in OBJECTS.items():
        s3_client.put_object(Bucket=bucket, Key=key, Body=data)


# --- tests ---


class TestHotReEncryption:
    """encrypt then decrypt existing plain objects via the maintenance API."""

    def test_encrypt_then_decrypt_roundtrip(self, s3_client, bucket, endpoint, creds):
        # Seed plain objects (the bucket has NO per-bucket encryption).
        _put_objects(s3_client, bucket)

        # Sanity: a freshly-put object is plain (no SSE header).
        head = s3_client.head_object(Bucket=bucket, Key="alpha.bin")
        assert head.get("ServerSideEncryption") is None

        # --- encrypt job ---
        resp = signed_request(
            "POST",
            jobs_url(endpoint),
            creds,
            {"type": "encrypt", "mode": "maintenance", "params": {"bucket": bucket}},
        )
        assert resp.status_code == 201, resp.text
        job = resp.json()
        assert job["job_type"] == "encrypt"

        done = wait_for_status(endpoint, creds, job["id"], {"completed", "failed"})
        assert done["status"] == "completed", done
        assert done["total"] >= len(OBJECTS)
        assert done["done"] == done["total"]

        # Every object is now encrypted AND still decrypts to the original bytes.
        for key, data in OBJECTS.items():
            get = s3_client.get_object(Bucket=bucket, Key=key)
            assert get["Body"].read() == data
            assert get.get("ServerSideEncryption") == "AES256"
            head = s3_client.head_object(Bucket=bucket, Key=key)
            assert head.get("ServerSideEncryption") == "AES256"

        # --- decrypt job ---
        resp = signed_request(
            "POST",
            jobs_url(endpoint),
            creds,
            {"type": "decrypt", "mode": "maintenance", "params": {"bucket": bucket}},
        )
        assert resp.status_code == 201, resp.text
        job = resp.json()
        assert job["job_type"] == "decrypt"

        done = wait_for_status(endpoint, creds, job["id"], {"completed", "failed"})
        assert done["status"] == "completed", done
        assert done["done"] == done["total"]

        # Every object is plain again AND still returns the original bytes.
        for key, data in OBJECTS.items():
            get = s3_client.get_object(Bucket=bucket, Key=key)
            assert get["Body"].read() == data
            assert get.get("ServerSideEncryption") is None
            head = s3_client.head_object(Bucket=bucket, Key=key)
            assert head.get("ServerSideEncryption") is None

    def test_encrypt_unknown_bucket_completes_with_no_candidates(
        self, endpoint, creds
    ):
        """A job whose bucket filter matches nothing still completes cleanly."""
        resp = signed_request(
            "POST",
            jobs_url(endpoint),
            creds,
            {
                "type": "encrypt",
                "mode": "maintenance",
                "params": {"bucket": "no-such-bucket-for-recrypt"},
            },
        )
        assert resp.status_code == 201, resp.text
        job = resp.json()

        done = wait_for_status(endpoint, creds, job["id"], {"completed", "failed"})
        assert done["status"] == "completed", done
        assert done["total"] == 0
        assert done["done"] == 0
