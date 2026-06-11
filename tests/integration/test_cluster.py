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
    cluster_config_drift_majority  drift overlays on BOTH arca-2 and arca-3
                           (distinct wrong secrets) — the aligned node must 503
    cluster_partition_before     all 3 up, seeds state for the partition phase
    cluster_partition_minority   arca-3 partitioned off (process ALIVE, network cut)
    cluster_partition_healed     partition healed (convergence + writability back)
    cluster_available_full       available-mode overlay, all 3 up
    cluster_available_split      available overlay, arca-3 partitioned (split brain)
    cluster_available_converged  available overlay, split healed (LWW winner only)
    cluster_available_minority   available overlay, only node 1 up (still writable)

The partition markers rely on the compose dual-network design: `bin/cluster
partition <n>` cuts a node off the `cluster` network (inter-node traffic — the
seeds use cluster-only aliases) while this runner keeps reaching every node by
its plain service name over the never-partitioned `mgmt` network. After a heal
the node may re-attach with a NEW IP: HAProxy resolves backends once at
startup, so post-heal assertions always target the nodes directly, not the LB.

Cross-phase tests use DETERMINISTIC bucket/key names so a value written in one
phase can be asserted in a later one (the cluster volumes persist across the
node stop/start that happens between phases).
"""

import json
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

# Control-plane catch-up (phases B and D): a bucket and a credential created
# via the LB while node 3 is down, then verified directly on node 3 after its
# re-entry (the control-plane reconcile, not the object manifest, covers them).
CATCHUP_CP_BUCKET = "cluster-catchup-cp"
CATCHUP_CRED_DESC = "cluster-catchup-credential"

# Quorum-mode partition phase: deterministic names across its sub-phases.
PARTITION_BUCKET = "cluster-partition"
PARTITION_READ_KEY = "seeded-before-partition"
PARTITION_READ_BODY = b"seeded on all nodes before the partition"
PARTITION_MAJORITY_KEY = "written-on-majority-side"
PARTITION_MAJORITY_BODY = b"written on the majority side during the partition"

# Available-mode phase: split-brain writes to the same key, LWW at heal.
AVAIL_BUCKET = "cluster-available"
AVAIL_SEED_KEY = "seeded-everywhere"
AVAIL_SEED_BODY = b"seeded on all nodes before the split"
AVAIL_LWW_KEY = "split-brain-object"
AVAIL_LWW_LOSER = b"written FIRST, on the isolated side - must lose LWW"
AVAIL_LWW_WINNER = b"written LAST, on the majority side - must win LWW"

# Drift phase: seeded while ONE node is drifted (quorum still holds), then read
# back while TWO nodes are drifted (writes 503 but reads must keep working).
DRIFT_BUCKET = "cluster-drift"
DRIFT_KEY = "written-with-one-drifted-node"
DRIFT_BODY = b"written with one drifted node excluded from the quorum"


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


def _admin_post(endpoint, path, payload):
    """Signed (SigV4) POST against an /admin/* endpoint, returning parsed JSON."""
    url = f"{endpoint}{path}"
    data = json.dumps(payload)
    req = AWSRequest(
        method="POST", url=url, data=data, headers={"Content-Type": "application/json"}
    )
    S3SigV4Auth(Credentials(ACCESS_KEY, SECRET_KEY), "s3", REGION).add_auth(req)
    resp = requests.post(url, headers=dict(req.headers), data=data, timeout=10)
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


def _wait_put(client, bucket, key, body, timeout=60):
    """Poll until a PUT succeeds (e.g. 503s while a healed node re-forms its
    quorum view), or fail after `timeout`."""
    deadline = time.time() + timeout
    last = "never attempted"
    while time.time() < deadline:
        try:
            client.put_object(Bucket=bucket, Key=key, Body=body)
            return
        except ClientError as e:
            last = e.response["Error"].get("Code", str(e))
        time.sleep(1)
    pytest.fail(f"PUT {bucket}/{key} kept failing for {timeout}s: {last}")


def _wait_bucket(client, bucket, timeout=90):
    """Poll until HeadBucket succeeds, or fail after `timeout`."""
    deadline = time.time() + timeout
    last = "never queried"
    while time.time() < deadline:
        try:
            client.head_bucket(Bucket=bucket)
            return
        except ClientError as e:
            last = e.response["Error"].get("Code", str(e))
        time.sleep(2)
    pytest.fail(f"bucket {bucket} did not appear within {timeout}s: {last}")


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


@pytest.mark.cluster_two_thirds
def test_control_plane_write_while_node_down():
    """Seed CONTROL-PLANE state (a bucket and a credential) while node 3 is down.

    Objects travel via the anti-entropy manifest; buckets and credentials via
    the control-plane reconcile. Both must reach node 3 after its re-entry —
    verified in the catch-up phase.
    """
    lb = _s3(LB)
    _ensure_bucket(lb, CATCHUP_CP_BUCKET)
    created = _admin_post(LB, "/admin/credentials", {"description": CATCHUP_CRED_DESC})
    assert created["access_key_id"]


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


@pytest.mark.cluster_catchup_verify
def test_bucket_catchup_on_returned_node():
    """The bucket created while node 3 was down appears on it via the
    control-plane reconcile."""
    _wait_bucket(_s3(NODES[3]), CATCHUP_CP_BUCKET, timeout=90)


@pytest.mark.cluster_catchup_verify
def test_credential_catchup_on_returned_node():
    """The credential created while node 3 was down appears on it via the
    control-plane reconcile."""
    deadline = time.time() + 90
    last = None
    while time.time() < deadline:
        last = _admin_get(NODES[3], "/admin/credentials")
        if any(c.get("description") == CATCHUP_CRED_DESC for c in last):
            return
        time.sleep(2)
    pytest.fail(
        f"credential '{CATCHUP_CRED_DESC}' did not appear on node 3 within 90s: {last}"
    )


# ── Phase: 507 overlay (arca-3 on a tiny tmpfs) ───────────────────────────────

@pytest.mark.cluster_insufficient_storage
def test_put_exceeding_smallest_node_is_rejected():
    """A PUT larger than the smallest node's free space is refused with 507.

    Cluster capacity is bounded by the smallest node (full replication), so even
    though arca-1/arca-2 have plenty of room, arca-3's tiny tmpfs caps it.
    """
    big_size = 50 * 1024 * 1024  # 50 MiB — far larger than arca-3's 16 MiB tmpfs
    # Wait until arca-3 is alive in the topology AND its tiny free space has
    # gossiped into the cluster minimum: the guard takes the min over LIVE
    # nodes, so checking the free-space number alone can race arca-3's
    # incorporation (its stats are cleared while it is considered dead).
    deadline = time.time() + 45
    while time.time() < deadline:
        data = _admin_get(NODES[1], "/admin/cluster")
        avail = data.get("disk_available_bytes")
        if data.get("live_node_count") == 3 and avail is not None and avail < big_size:
            break
        time.sleep(2)
    else:
        pytest.fail(
            "cluster min free space never dropped below the test object size "
            "with all 3 nodes alive"
        )

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
    # Exactly the drifted node reports config_ok=False — and, with a different
    # secret, it cannot answer the authenticated ping either (R3/H12).
    bad = [n for n in data["nodes"] if n.get("config_ok") is False]
    assert len(bad) >= 1, f"expected at least one node with config_ok=False: {data['nodes']}"
    assert all(n.get("authenticated") is False for n in bad), (
        f"a drifted-secret node must not be authenticated: {bad}"
    )


@pytest.mark.cluster_config_drift
def test_quorum_holds_with_one_drifted_node():
    """One drifted node of three does NOT break the quorum (H7): the two
    aligned nodes still are 2 eligible >= write_quorum 2, so writes succeed —
    while the drifted node is excluded from the count and the fan-out."""
    # Wait until node 1 sees the drift (so we measure the post-exclusion state,
    # not a not-yet-noticed one) and reports exactly 2 eligible nodes.
    deadline = time.time() + 45
    data = None
    while time.time() < deadline:
        data = _admin_get(NODES[1], "/admin/cluster")
        if data.get("config_aligned") is False and data.get("eligible_node_count") == 2:
            break
        time.sleep(2)
    assert data is not None and data.get("eligible_node_count") == 2, (
        f"expected 2 eligible nodes (drifted one excluded), got: {data}"
    )
    assert data.get("has_write_quorum") is True, f"quorum must hold with 2/3 aligned: {data}"

    node1 = _s3(NODES[1])
    _ensure_bucket(node1, DRIFT_BUCKET)
    node1.put_object(Bucket=DRIFT_BUCKET, Key=DRIFT_KEY, Body=DRIFT_BODY)
    assert node1.get_object(Bucket=DRIFT_BUCKET, Key=DRIFT_KEY)["Body"].read() == DRIFT_BODY


# ── Phase: drift-majority overlay (arca-2 AND arca-3 each with its own secret) ─

@pytest.mark.cluster_config_drift_majority
def test_writes_refused_with_drifted_majority():
    """With 2 of 3 nodes drifted (each with a DIFFERENT wrong secret), the
    aligned node alone is 1 eligible < write_quorum 2: it must refuse writes
    with 503 even though all three processes are alive — a drifted node must
    not sustain a quorum it cannot correctly participate in (H7/H12)."""
    deadline = time.time() + 60
    data = None
    while time.time() < deadline:
        data = _admin_get(NODES[1], "/admin/cluster")
        bad = [n for n in data.get("nodes", []) if n.get("config_ok") is False]
        if len(bad) >= 2 and data.get("eligible_node_count") == 1:
            break
        time.sleep(2)
    assert data is not None and data.get("eligible_node_count") == 1, (
        f"expected both drifted nodes excluded (1 eligible), got: {data}"
    )
    assert data.get("live_node_count") == 3, (
        f"all three processes are alive (drift is not death): {data}"
    )
    assert data.get("has_write_quorum") is False, f"no quorum from drifted nodes: {data}"

    node1 = _s3(NODES[1])
    # Data plane: the object write is refused.
    with pytest.raises(ClientError) as exc:
        node1.put_object(Bucket=DRIFT_BUCKET, Key="drift-majority-write", Body=b"x")
    assert _status_code(exc.value) == 503
    assert exc.value.response["Error"]["Code"] == "ServiceUnavailable"

    # Control plane: bucket creation is gated by the same quorum.
    with pytest.raises(ClientError) as exc:
        node1.create_bucket(Bucket="cluster-drift-majority-new")
    assert _status_code(exc.value) == 503

    # Reads stay un-gated: the object seeded in the one-drifted sub-phase
    # (same volumes — only arca-2 was recreated) is still served.
    body = node1.get_object(Bucket=DRIFT_BUCKET, Key=DRIFT_KEY)["Body"].read()
    assert body == DRIFT_BODY


# ── Phase: quorum mode under a REAL network partition (arca-3 isolated) ───────
#
# Unlike the stop/start phases, `bin/cluster partition 3` leaves the arca-3
# process ALIVE and severs only the cluster network — the case the consistency
# modes exist for. This runner keeps reaching it over the mgmt network.


@pytest.mark.cluster_partition_before
def test_seed_object_before_partition():
    """Seed a bucket + object on all nodes; the partition assertions read them."""
    lb = _s3(LB)
    _ensure_bucket(lb, PARTITION_BUCKET)
    lb.put_object(Bucket=PARTITION_BUCKET, Key=PARTITION_READ_KEY, Body=PARTITION_READ_BODY)
    for n in (1, 2, 3):
        _wait_object(_s3(NODES[n]), PARTITION_BUCKET, PARTITION_READ_KEY, PARTITION_READ_BODY, timeout=30)


@pytest.mark.cluster_partition_minority
def test_isolated_node_refuses_writes():
    """The isolated node refuses writes with 503 — even before its membership
    notices the partition, the missing fan-out ACKs close the window (§2.1)."""
    node3 = _s3(NODES[3])
    with pytest.raises(ClientError) as exc:
        node3.put_object(Bucket=PARTITION_BUCKET, Key="isolated-write", Body=b"x")
    assert _status_code(exc.value) == 503
    assert exc.value.response["Error"]["Code"] == "ServiceUnavailable"


@pytest.mark.cluster_partition_minority
def test_isolated_node_still_serves_reads():
    """Reads are never quorum-gated: the isolated node serves what it has."""
    node3 = _s3(NODES[3])
    body = node3.get_object(Bucket=PARTITION_BUCKET, Key=PARTITION_READ_KEY)["Body"].read()
    assert body == PARTITION_READ_BODY


@pytest.mark.cluster_partition_minority
def test_majority_side_still_writes():
    """The 2-node side keeps quorum: writes succeed and replicate within it."""
    node1 = _s3(NODES[1])
    node1.put_object(
        Bucket=PARTITION_BUCKET, Key=PARTITION_MAJORITY_KEY, Body=PARTITION_MAJORITY_BODY
    )
    for n in (1, 2):
        _wait_object(
            _s3(NODES[n]), PARTITION_BUCKET, PARTITION_MAJORITY_KEY,
            PARTITION_MAJORITY_BODY, timeout=30,
        )


@pytest.mark.cluster_partition_healed
def test_partition_write_converges_on_healed_node():
    """After heal, anti-entropy delivers the majority-side write to arca-3."""
    _wait_object(
        _s3(NODES[3]), PARTITION_BUCKET, PARTITION_MAJORITY_KEY,
        PARTITION_MAJORITY_BODY, timeout=90,
    )


@pytest.mark.cluster_partition_healed
def test_healed_node_accepts_writes_again():
    """arca-3 regains the quorum after heal and accepts writes again (its own
    membership view may lag a few health ticks, hence the PUT poll)."""
    node3 = _s3(NODES[3])
    body = b"written on node 3 after the partition healed"
    _wait_put(node3, PARTITION_BUCKET, "post-heal-write", body, timeout=60)
    assert node3.get_object(Bucket=PARTITION_BUCKET, Key="post-heal-write")["Body"].read() == body


# ── Phase: available mode (AP overlay on all 3 nodes) ─────────────────────────


@pytest.mark.cluster_available_full
def test_available_seed_replicates_everywhere():
    """Bucket + object seeded via the LB land on all 3 nodes (as in quorum)."""
    lb = _s3(LB)
    _ensure_bucket(lb, AVAIL_BUCKET)
    lb.put_object(Bucket=AVAIL_BUCKET, Key=AVAIL_SEED_KEY, Body=AVAIL_SEED_BODY)
    for n in (1, 2, 3):
        _wait_object(_s3(NODES[n]), AVAIL_BUCKET, AVAIL_SEED_KEY, AVAIL_SEED_BODY, timeout=30)


@pytest.mark.cluster_available_split
def test_both_sides_accept_writes_to_the_same_key():
    """During the partition BOTH sides accept a write to the same key (AP mode
    never gates on quorum). Isolated side first, majority side last: LWW must
    later keep the majority-side value as the single winner."""
    _s3(NODES[3]).put_object(Bucket=AVAIL_BUCKET, Key=AVAIL_LWW_KEY, Body=AVAIL_LWW_LOSER)
    # Strictly later wall-clock timestamp (the containers share the host clock).
    time.sleep(2)
    _s3(NODES[1]).put_object(Bucket=AVAIL_BUCKET, Key=AVAIL_LWW_KEY, Body=AVAIL_LWW_WINNER)
    # While split, each side holds its own value; convergence is asserted post-heal.
    assert _s3(NODES[3]).get_object(Bucket=AVAIL_BUCKET, Key=AVAIL_LWW_KEY)["Body"].read() == AVAIL_LWW_LOSER
    assert _s3(NODES[1]).get_object(Bucket=AVAIL_BUCKET, Key=AVAIL_LWW_KEY)["Body"].read() == AVAIL_LWW_WINNER


@pytest.mark.cluster_available_converged
def test_lww_winner_survives_everywhere():
    """After heal a single LWW winner remains on every node; the losing write
    disappears without any error ever surfacing to the client that made it."""
    for n in (1, 2, 3):
        _wait_object(_s3(NODES[n]), AVAIL_BUCKET, AVAIL_LWW_KEY, AVAIL_LWW_WINNER, timeout=90)


@pytest.mark.cluster_available_minority
def test_available_minority_still_writable():
    """With 2 of 3 nodes stopped, available mode still accepts writes — the
    same 1/3 topology where quorum mode returns 503 (the one-third phase)."""
    node1 = _s3(NODES[1])
    body = b"accepted with two nodes down (available mode)"
    node1.put_object(Bucket=AVAIL_BUCKET, Key="minority-write", Body=body)
    assert node1.get_object(Bucket=AVAIL_BUCKET, Key="minority-write")["Body"].read() == body
