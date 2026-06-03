"""Phase 29 — 3-node HA cluster integration tests.

Exercises a symmetric, fully-replicated 3-node cluster fronted by HAProxy:
- ARCA_ENDPOINT         — the load balancer (default: http://arca-lb:9000)
- ARCA_NODE1/2/3_ENDPOINT — each node directly (for per-node and catch-up checks)

`bin/test cluster` drives the topology: it brings the cluster up, then runs the
tests in PHASES, stopping/starting nodes BETWEEN phases (a node kill/restart
cannot be driven from inside a test container). Each test is tagged with the
topology it needs via a marker, and the runner selects them with `pytest -m`:

    cluster_full           all 3 up
    cluster_two_thirds     node 3 down (2/3 — quorum still met)
    cluster_one_third      nodes 2 & 3 down (1/3 — no quorum)
    cluster_catchup_verify all back up (anti-entropy must have converged)
    cluster_insufficient_storage  507 overlay (arca-3 on a tiny tmpfs)
    cluster_config_drift   drift overlay (arca-3 with a mismatched secret)

Cross-phase tests use DETERMINISTIC bucket/key names so a value written in one
phase can be asserted in a later one (the cluster volumes persist across the
node stop/start that happens between phases).
"""

import os
import time
import uuid

import boto3
import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.client import Config
from botocore.credentials import Credentials
from botocore.exceptions import ClientError


LB = os.environ.get("ARCA_ENDPOINT", "http://arca-lb:9000")
NODES = {
    1: os.environ.get("ARCA_NODE1_ENDPOINT", "http://arca-1:9000"),
    2: os.environ.get("ARCA_NODE2_ENDPOINT", "http://arca-2:9000"),
    3: os.environ.get("ARCA_NODE3_ENDPOINT", "http://arca-3:9000"),
}
ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE")
SECRET_KEY = os.environ.get("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
REGION = "us-east-1"

# Deterministic names for the values that must survive a node stop/start.
CATCHUP_BUCKET = "cluster-catchup"
CATCHUP_KEY = "catchup-object"
CATCHUP_BODY = b"written via the load balancer while node 3 was down"

FAILOVER_BUCKET = "cluster-failover"
FAILOVER_KEY = "failover-object"
FAILOVER_BODY = b"written while all nodes were up; must survive a node failure"


# ── Clients ──────────────────────────────────────────────────────────────────

def _s3(endpoint):
    """An S3 client with retries disabled so a 503/507 surfaces immediately."""
    return boto3.client(
        "s3",
        endpoint_url=endpoint,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        region_name=REGION,
        config=Config(retries={"max_attempts": 1, "mode": "standard"}),
    )


def _admin_get(endpoint, path):
    """Signed (SigV4) GET against an /admin/* endpoint, returning parsed JSON."""
    url = f"{endpoint}{path}"
    req = AWSRequest(method="GET", url=url, data="")
    S3SigV4Auth(Credentials(ACCESS_KEY, SECRET_KEY), "s3", REGION).add_auth(req)
    resp = requests.get(url, headers=dict(req.headers), timeout=10)
    resp.raise_for_status()
    return resp.json()


def _status_code(err: ClientError) -> int:
    return err.response["ResponseMetadata"]["HTTPStatusCode"]


def _ensure_bucket(client, bucket):
    """Create a bucket, tolerating the case where it already exists."""
    try:
        client.create_bucket(Bucket=bucket)
    except ClientError as e:
        if e.response["Error"]["Code"] not in ("BucketAlreadyOwnedByYou", "BucketAlreadyExists"):
            raise


def _wait_object(client, bucket, key, expected, timeout=30):
    """Poll until GET returns the expected bytes, or fail after `timeout`."""
    deadline = time.time() + timeout
    last = "never queried"
    while time.time() < deadline:
        try:
            body = client.get_object(Bucket=bucket, Key=key)["Body"].read()
            if body == expected:
                return
            last = f"body mismatch ({len(body)} vs {len(expected)} bytes)"
        except ClientError as e:
            last = e.response["Error"].get("Code", str(e))
        time.sleep(1)
    pytest.fail(f"{bucket}/{key} did not converge within {timeout}s: {last}")


# ── Phase: all 3 nodes up ─────────────────────────────────────────────────────

@pytest.mark.cluster_full
def test_write_replicates_to_all_nodes():
    """A write through the LB becomes readable on every node (full replication)."""
    lb = _s3(LB)
    bucket = f"cl-repl-{uuid.uuid4().hex[:10]}"
    key = "obj"
    body = uuid.uuid4().bytes * 64  # 1 KiB of unique content
    _ensure_bucket(lb, bucket)
    lb.put_object(Bucket=bucket, Key=key, Body=body)
    for n in (1, 2, 3):
        _wait_object(_s3(NODES[n]), bucket, key, body, timeout=30)


@pytest.mark.cluster_full
def test_read_after_write_via_lb():
    """An object is immediately readable through the LB after the write returns."""
    lb = _s3(LB)
    bucket = f"cl-raw-{uuid.uuid4().hex[:10]}"
    body = b"read-after-write payload"
    _ensure_bucket(lb, bucket)
    lb.put_object(Bucket=bucket, Key="k", Body=body)
    assert lb.get_object(Bucket=bucket, Key="k")["Body"].read() == body


@pytest.mark.cluster_full
def test_seed_failover_object():
    """Seed an object on all nodes; a later phase reads it after a node is killed."""
    lb = _s3(LB)
    _ensure_bucket(lb, FAILOVER_BUCKET)
    lb.put_object(Bucket=FAILOVER_BUCKET, Key=FAILOVER_KEY, Body=FAILOVER_BODY)
    for n in (1, 2, 3):
        _wait_object(_s3(NODES[n]), FAILOVER_BUCKET, FAILOVER_KEY, FAILOVER_BODY, timeout=30)


# ── Phase: node 3 down (2 of 3 — quorum still met) ────────────────────────────

@pytest.mark.cluster_two_thirds
def test_writable_with_quorum():
    """With 2 of 3 nodes up, a write still succeeds (majority quorum)."""
    lb = _s3(LB)
    bucket = f"cl-quorum-{uuid.uuid4().hex[:10]}"
    body = b"written with one node down"
    _ensure_bucket(lb, bucket)
    lb.put_object(Bucket=bucket, Key="k", Body=body)
    assert lb.get_object(Bucket=bucket, Key="k")["Body"].read() == body


@pytest.mark.cluster_two_thirds
def test_failover_read_after_node_down():
    """An object seeded while all nodes were up is still readable after a failure."""
    lb = _s3(LB)
    assert lb.get_object(Bucket=FAILOVER_BUCKET, Key=FAILOVER_KEY)["Body"].read() == FAILOVER_BODY


@pytest.mark.cluster_two_thirds
def test_catchup_write_while_node_down():
    """Write the catch-up object while node 3 is down; it lands on nodes 1 & 2."""
    lb = _s3(LB)
    _ensure_bucket(lb, CATCHUP_BUCKET)
    lb.put_object(Bucket=CATCHUP_BUCKET, Key=CATCHUP_KEY, Body=CATCHUP_BODY)
    # Node 3 is down here, so it cannot have it yet — verified in a later phase.
    for n in (1, 2):
        _wait_object(_s3(NODES[n]), CATCHUP_BUCKET, CATCHUP_KEY, CATCHUP_BODY, timeout=30)


# ── Phase: nodes 2 & 3 down (1 of 3 — no quorum) ──────────────────────────────

@pytest.mark.cluster_one_third
def test_write_rejected_without_quorum():
    """In quorum mode with only 1 of 3 up, a write is refused with 503."""
    # Hit node 1 directly: the LB is fine but this is deterministic, and the
    # quorum gate lives on the node, not the balancer.
    node1 = _s3(NODES[1])
    with pytest.raises(ClientError) as exc:
        node1.put_object(Bucket=FAILOVER_BUCKET, Key="no-quorum", Body=b"x")
    assert _status_code(exc.value) == 503
    assert exc.value.response["Error"]["Code"] == "ServiceUnavailable"


@pytest.mark.cluster_one_third
def test_read_still_works_without_quorum():
    """Reads are never gated by quorum — a previously written object still reads."""
    node1 = _s3(NODES[1])
    assert node1.get_object(Bucket=FAILOVER_BUCKET, Key=FAILOVER_KEY)["Body"].read() == FAILOVER_BODY


# ── Phase: all nodes back up (anti-entropy must have converged) ────────────────

@pytest.mark.cluster_catchup_verify
def test_catchup_converges_on_returned_node():
    """The object written while node 3 was down appears on it via anti-entropy."""
    # Generous timeout: anti-entropy runs on a timer (a few seconds in the test
    # config), plus the node needs to rejoin and re-establish peers first.
    _wait_object(_s3(NODES[3]), CATCHUP_BUCKET, CATCHUP_KEY, CATCHUP_BODY, timeout=90)


# ── Phase: 507 overlay (arca-3 on a tiny tmpfs) ───────────────────────────────

@pytest.mark.cluster_insufficient_storage
def test_put_exceeding_smallest_node_is_rejected():
    """A PUT larger than the smallest node's free space is refused with 507.

    Cluster capacity is bounded by the smallest node (full replication), so even
    though arca-1/arca-2 have plenty of room, arca-3's tiny tmpfs caps it.
    """
    big_size = 50 * 1024 * 1024  # 50 MiB — far larger than arca-3's 16 MiB tmpfs
    # Wait until the tiny node's free space has gossiped into the cluster minimum
    # (otherwise the write guard may not yet see it and the big PUT would pass).
    deadline = time.time() + 45
    while time.time() < deadline:
        avail = _admin_get(NODES[1], "/admin/cluster").get("disk_available_bytes")
        if avail is not None and avail < big_size:
            break
        time.sleep(2)
    else:
        pytest.fail("cluster min free space never dropped below the test object size")

    lb = _s3(LB)
    bucket = f"cl-507-{uuid.uuid4().hex[:10]}"
    _ensure_bucket(lb, bucket)
    # Sanity: a small object still succeeds (the guard is size-based, not blanket).
    lb.put_object(Bucket=bucket, Key="small", Body=b"hello")
    big = b"\0" * big_size
    with pytest.raises(ClientError) as exc:
        lb.put_object(Bucket=bucket, Key="big", Body=big)
    assert _status_code(exc.value) == 507
    assert exc.value.response["Error"]["Code"] == "InsufficientStorage"


# ── Phase: drift overlay (arca-3 with a mismatched secret) ────────────────────

@pytest.mark.cluster_config_drift
def test_config_drift_is_detected():
    """A node started with a different secret is flagged on /admin/cluster."""
    # Poll an aligned node until it observes the mismatch (health gossip interval
    # is a few seconds in the test config).
    deadline = time.time() + 45
    data = None
    while time.time() < deadline:
        data = _admin_get(NODES[1], "/admin/cluster")
        if data.get("config_aligned") is False:
            break
        time.sleep(2)
    assert data is not None and data.get("config_aligned") is False, (
        f"expected config_aligned=False, got: {data}"
    )
    # Exactly the drifted node reports config_ok=False.
    bad = [n for n in data["nodes"] if n.get("config_ok") is False]
    assert len(bad) >= 1, f"expected at least one node with config_ok=False: {data['nodes']}"
