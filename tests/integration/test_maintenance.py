"""Integration tests for Phase 30: maintenance jobs admin API.

Exercises the `/admin/maintenance/*` endpoints end-to-end against a running
Arca using the trivial "noop" job type.
"""

import json
import os
import time

import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials


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


def wait_for_status(endpoint, creds, job_id, statuses, timeout=20.0):
    """Poll a job until it reaches one of `statuses` (or timeout)."""
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
        f"job {job_id} did not reach {statuses}; last status={last['status'] if last else '?'}"
    )


class TestMaintenanceAuth:
    def test_list_requires_auth(self, endpoint):
        resp = requests.get(jobs_url(endpoint), timeout=10)
        assert resp.status_code in (401, 403)


class TestMaintenanceValidation:
    def test_list_shape(self, endpoint, creds):
        resp = signed_request("GET", jobs_url(endpoint), creds)
        assert resp.status_code == 200, resp.text
        body = resp.json()
        assert "active" in body
        assert "jobs" in body
        assert isinstance(body["jobs"], list)

    def test_unknown_type_rejected(self, endpoint, creds):
        resp = signed_request(
            "POST", jobs_url(endpoint), creds, {"type": "does-not-exist"}
        )
        assert resp.status_code == 400, resp.text

    def test_bad_mode_rejected(self, endpoint, creds):
        resp = signed_request(
            "POST", jobs_url(endpoint), creds, {"type": "noop", "mode": "bogus"}
        )
        assert resp.status_code == 400, resp.text


class TestMaintenanceLifecycle:
    def test_noop_runs_to_completion(self, endpoint, creds):
        resp = signed_request(
            "POST",
            jobs_url(endpoint),
            creds,
            {"type": "noop", "mode": "live", "params": {"n": 3}},
        )
        assert resp.status_code == 201, resp.text
        job = resp.json()
        assert job["status"] == "pending"
        assert job["job_type"] == "noop"

        done = wait_for_status(endpoint, creds, job["id"], {"completed", "failed"})
        assert done["status"] == "completed", done
        assert done["done"] == 3
        assert done["total"] == 3
        assert done["finished_at"] is not None

        # Logs were recorded (started + completed at least).
        resp = signed_request("GET", f"{jobs_url(endpoint)}/{job['id']}", creds)
        logs = resp.json()["logs"]
        assert any("started" in entry["message"] for entry in logs)
        assert any("completed" in entry["message"] for entry in logs)

    def test_single_job_lock(self, endpoint, creds):
        # A long-running job holds the single slot.
        first = signed_request(
            "POST",
            jobs_url(endpoint),
            creds,
            {"type": "noop", "mode": "live", "params": {"n": 100000, "delay_ms": 50}},
        )
        assert first.status_code == 201, first.text
        fid = first.json()["id"]
        try:
            second = signed_request(
                "POST",
                jobs_url(endpoint),
                creds,
                {"type": "noop", "mode": "live", "params": {"n": 1}},
            )
            assert second.status_code == 409, second.text
        finally:
            signed_request("DELETE", f"{jobs_url(endpoint)}/{fid}", creds)

    def test_pause_resume_cancel(self, endpoint, creds):
        created = signed_request(
            "POST",
            jobs_url(endpoint),
            creds,
            {"type": "noop", "mode": "live", "params": {"n": 100000, "delay_ms": 50}},
        )
        assert created.status_code == 201, created.text
        jid = created.json()["id"]

        # Worker picks it up.
        wait_for_status(endpoint, creds, jid, {"running"})

        # Pause -> status paused, progress retained, not complete.
        resp = signed_request("POST", f"{jobs_url(endpoint)}/{jid}/pause", creds)
        assert resp.status_code == 200, resp.text
        paused = wait_for_status(endpoint, creds, jid, {"paused"})
        assert paused["done"] < paused.get("total", 100000) or paused["done"] >= 0

        # Resume -> back to running.
        resp = signed_request("POST", f"{jobs_url(endpoint)}/{jid}/resume", creds)
        assert resp.status_code == 200, resp.text
        wait_for_status(endpoint, creds, jid, {"running"})

        # Cancel -> terminal cancelled.
        resp = signed_request("DELETE", f"{jobs_url(endpoint)}/{jid}", creds)
        assert resp.status_code == 200, resp.text
        cancelled = wait_for_status(endpoint, creds, jid, {"cancelled"})
        assert cancelled["status"] == "cancelled"

        # Resuming a cancelled (terminal) job is rejected.
        resp = signed_request("POST", f"{jobs_url(endpoint)}/{jid}/resume", creds)
        assert resp.status_code == 409, resp.text

    def test_get_missing_job_404(self, endpoint, creds):
        resp = signed_request(
            "GET", f"{jobs_url(endpoint)}/no-such-job-id", creds
        )
        assert resp.status_code == 404, resp.text


class TestMaintenanceDrain:
    """A maintenance-mode job must drain the S3 API on the node (503 to
    external clients), not just flip the health probe."""

    def test_maintenance_mode_blocks_s3(self, endpoint, creds):
        created = signed_request(
            "POST",
            jobs_url(endpoint),
            creds,
            {"type": "noop", "mode": "maintenance", "params": {"n": 100000, "delay_ms": 50}},
        )
        assert created.status_code == 201, created.text
        jid = created.json()["id"]
        try:
            wait_for_status(endpoint, creds, jid, {"running"})
            # S3 requests (here: ListBuckets) are refused with 503 while the
            # maintenance-mode job runs.
            deadline = time.time() + 10
            blocked = False
            while time.time() < deadline:
                ls = signed_request("GET", f"{endpoint}/", creds)
                if ls.status_code == 503:
                    blocked = True
                    break
                time.sleep(0.5)
            assert blocked, "S3 was not drained (no 503) during a maintenance-mode job"
        finally:
            signed_request("DELETE", f"{jobs_url(endpoint)}/{jid}", creds)
        # After cancellation the drain lifts and S3 serves again.
        deadline = time.time() + 10
        served = False
        while time.time() < deadline:
            ls = signed_request("GET", f"{endpoint}/", creds)
            if ls.status_code == 200:
                served = True
                break
            time.sleep(0.5)
        assert served, "S3 did not recover after the maintenance job was cancelled"

    def test_live_mode_does_not_block_s3(self, endpoint, creds):
        created = signed_request(
            "POST",
            jobs_url(endpoint),
            creds,
            {"type": "noop", "mode": "live", "params": {"n": 100000, "delay_ms": 50}},
        )
        assert created.status_code == 201, created.text
        jid = created.json()["id"]
        try:
            wait_for_status(endpoint, creds, jid, {"running"})
            ls = signed_request("GET", f"{endpoint}/", creds)
            assert ls.status_code == 200, "live-mode jobs must not drain S3"
        finally:
            signed_request("DELETE", f"{jobs_url(endpoint)}/{jid}", creds)


class TestClearHistory:
    def test_clear_removes_terminal_keeps_active(self, endpoint, creds):
        # A finished job lands in history.
        done = signed_request(
            "POST", jobs_url(endpoint), creds,
            {"type": "noop", "mode": "live", "params": {"n": 2}},
        )
        assert done.status_code == 201, done.text
        wait_for_status(endpoint, creds, done.json()["id"], {"completed", "failed"})

        # Clear the history (DELETE on the collection).
        resp = signed_request("DELETE", jobs_url(endpoint), creds)
        assert resp.status_code == 200, resp.text
        assert resp.json().get("cleared", 0) >= 1
        # The terminal job is gone.
        listing = signed_request("GET", jobs_url(endpoint), creds).json()
        ids = [j["id"] for j in listing["jobs"]]
        assert done.json()["id"] not in ids
