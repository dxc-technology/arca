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
    cluster_leader_full          all 3 up; verifies the R6 worker-leader gate
    cluster_leader_failover      worker leader stopped; verifies role failover
    cluster_syncing_seed         all 3 up, seeds state for the readiness phase
    cluster_syncing_while_down   node 3 down; writes the data it must catch up on
    cluster_syncing_readiness    node 3 JUST restarted (no wait): 503 syncing must
                                 hold until the catch-up completes, then 200

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

# R5 (TD-016/D4/N1) catch-up state, seeded with all 3 up (phase A), mutated
# while node 3 is down (phase B), verified on node 3 after re-entry (phase D).
# Server-generated ids (user_id, grant_id, upload_id) are recovered in later
# phases by their deterministic username / grant name / object key.
R5_USERNAME = "cluster-r5-user"
R5_GRANT_NAME = "cluster-r5-grant"
R5_LOCK_BUCKET = "cluster-r5-lock"
R5_LOCK_KEY = "locked-object"
R5_LOCK_BODY = b"object-lock guinea pig"
R5_TAG_KEY = "cluster-r5-env"
R5_TAG_VALUE = "staging"
R5_MP_BUCKET = "cluster-r5-mp"
R5_MP_ABORT_KEY = "mp-aborted-while-node-down"
R5_MP_CATCHUP_KEY = "mp-created-while-node-down"
R5_MP_CATCHUP_BODY = b"single part uploaded while node 3 was down"


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


def _admin_request(method, endpoint, path, payload=None):
    """Signed (SigV4) request against an /admin/* endpoint."""
    url = f"{endpoint}{path}"
    data = json.dumps(payload) if payload is not None else ""
    headers = {"Content-Type": "application/json"} if payload is not None else {}
    req = AWSRequest(method=method, url=url, data=data, headers=headers)
    S3SigV4Auth(Credentials(ACCESS_KEY, SECRET_KEY), "s3", REGION).add_auth(req)
    resp = requests.request(method, url, headers=dict(req.headers), data=data, timeout=10)
    resp.raise_for_status()
    return resp.json() if resp.content else None


def _r5_user_id(endpoint):
    """Resolves the R5 user's server-generated id by its deterministic username."""
    users = _admin_get(endpoint, "/admin/users")
    for u in users:
        if u.get("username") == R5_USERNAME:
            return u["user_id"]
    return None


def _r5_grant_id(endpoint):
    """Resolves the R5 grant's server-generated id by its deterministic name."""
    grants = _admin_get(endpoint, "/admin/grants")
    for g in grants:
        if g.get("name") == R5_GRANT_NAME:
            return g["grant_id"]
    return None


def _mp_upload_id(client, bucket, key):
    """Finds the in-progress multipart upload for `key`, if any."""
    uploads = client.list_multipart_uploads(Bucket=bucket).get("Uploads", [])
    for u in uploads:
        if u["Key"] == key:
            return u["UploadId"]
    return None


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


@pytest.mark.cluster_full
def test_r5_seed_control_state_everywhere():
    """Seed the R5 control-plane state with all 3 nodes up.

    A user with an attached grant, an Object-Lock bucket with one object, and
    an in-progress multipart upload — each verified to have replicated to node
    3 BEFORE it goes down, so the phase-B mutations (detach, retention change,
    abort) are real changes node 3 must learn at re-entry via the snapshot
    reconcile, not state it never had.
    """
    # User + grant + attachment via the LB.
    user = _admin_request("POST", LB, "/admin/users", {"username": R5_USERNAME})
    grant = _admin_request(
        "POST",
        LB,
        "/admin/grants",
        {
            "name": R5_GRANT_NAME,
            "description": "r5 catch-up grant",
            "document": {
                "Version": "2012-10-17",
                "Statement": [
                    {
                        "Effect": "Allow",
                        "Action": ["s3:GetObject"],
                        "Resource": ["arn:aws:s3:::cluster-r5-*/*"],
                    }
                ],
            },
        },
    )
    _admin_request(
        "PUT", LB, f"/admin/users/{user['user_id']}/grants/{grant['grant_id']}"
    )

    # Object-Lock bucket (versioned by construction) with one object.
    lb = _s3(LB)
    try:
        lb.create_bucket(Bucket=R5_LOCK_BUCKET, ObjectLockEnabledForBucket=True)
    except ClientError as e:
        if e.response["Error"]["Code"] not in ("BucketAlreadyOwnedByYou", "BucketAlreadyExists"):
            raise
    lb.put_object(Bucket=R5_LOCK_BUCKET, Key=R5_LOCK_KEY, Body=R5_LOCK_BODY)

    # An in-progress multipart upload with one part, to be aborted in phase B.
    _ensure_bucket(lb, R5_MP_BUCKET)
    mp = lb.create_multipart_upload(Bucket=R5_MP_BUCKET, Key=R5_MP_ABORT_KEY)
    lb.upload_part(
        Bucket=R5_MP_BUCKET,
        Key=R5_MP_ABORT_KEY,
        UploadId=mp["UploadId"],
        PartNumber=1,
        Body=b"part to be discarded by the abort",
    )

    # Everything must be visible on node 3 before the phase ends.
    node3 = _s3(NODES[3])
    deadline = time.time() + 60
    while time.time() < deadline:
        uid = _r5_user_id(NODES[3])
        attached = bool(uid) and any(
            g.get("name") == R5_GRANT_NAME
            for g in _admin_get(NODES[3], f"/admin/users/{uid}/grants")
        )
        mp_seen = _mp_upload_id(node3, R5_MP_BUCKET, R5_MP_ABORT_KEY) is not None
        if attached and mp_seen:
            break
        time.sleep(2)
    else:
        pytest.fail("R5 seed state did not replicate to node 3 within 60s")
    _wait_object(node3, R5_LOCK_BUCKET, R5_LOCK_KEY, R5_LOCK_BODY, timeout=30)


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


@pytest.mark.cluster_two_thirds
def test_r5_detach_grant_while_node_down():
    """Revoke the R5 grant attachment while node 3 is down (TD-016).

    Node 3 holds the attachment: only the user_grant tombstone in the snapshot
    reconcile can revoke it at re-entry — real-time fan-out cannot reach a dead
    node, and before R5 this family was not reconciled at all.
    """
    uid = _r5_user_id(LB)
    gid = _r5_grant_id(LB)
    assert uid and gid, "R5 user/grant must exist from phase A"
    _admin_request("DELETE", LB, f"/admin/users/{uid}/grants/{gid}")


@pytest.mark.cluster_two_thirds
def test_r5_bucket_config_and_tags_while_node_down():
    """Change bucket_config (versioning) and the bucket tag set while node 3 is
    down (TD-016): both families must reach it via the snapshot reconcile."""
    lb = _s3(LB)
    lb.put_bucket_versioning(
        Bucket=CATCHUP_CP_BUCKET, VersioningConfiguration={"Status": "Enabled"}
    )
    lb.put_bucket_tagging(
        Bucket=FAILOVER_BUCKET,
        Tagging={"TagSet": [{"Key": R5_TAG_KEY, "Value": R5_TAG_VALUE}]},
    )


@pytest.mark.cluster_two_thirds
def test_r5_retention_change_while_node_down():
    """Set Object-Lock retention while node 3 is down (N1): the lock UPDATE now
    stamps a fresh seq, so the changed-since manifest carries it at re-entry."""
    import datetime

    lb = _s3(LB)
    lb.put_object_retention(
        Bucket=R5_LOCK_BUCKET,
        Key=R5_LOCK_KEY,
        Retention={
            "Mode": "GOVERNANCE",
            "RetainUntilDate": datetime.datetime(2030, 1, 1, tzinfo=datetime.timezone.utc),
        },
    )


@pytest.mark.cluster_two_thirds
def test_r5_abort_multipart_while_node_down():
    """Abort the phase-A multipart upload while node 3 is down (D4): node 3
    still holds the upload row — only the multipart tombstone can close it at
    re-entry (and stop it from resurrecting on nodes 1/2)."""
    lb = _s3(LB)
    upload_id = _mp_upload_id(lb, R5_MP_BUCKET, R5_MP_ABORT_KEY)
    assert upload_id, "phase-A multipart upload must still be in progress"
    lb.abort_multipart_upload(
        Bucket=R5_MP_BUCKET, Key=R5_MP_ABORT_KEY, UploadId=upload_id
    )


@pytest.mark.cluster_two_thirds
def test_r5_create_multipart_while_node_down():
    """Begin a NEW multipart upload while node 3 is down (D4): at re-entry node
    3 must learn the upload AND its part row from the snapshot, then be able to
    Complete it by fetching the part bytes from a peer."""
    lb = _s3(LB)
    mp = lb.create_multipart_upload(Bucket=R5_MP_BUCKET, Key=R5_MP_CATCHUP_KEY)
    lb.upload_part(
        Bucket=R5_MP_BUCKET,
        Key=R5_MP_CATCHUP_KEY,
        UploadId=mp["UploadId"],
        PartNumber=1,
        Body=R5_MP_CATCHUP_BODY,
    )


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


@pytest.mark.cluster_catchup_verify
def test_r5_detach_reconciled_on_returned_node():
    """The grant attachment revoked while node 3 was down is gone on it (TD-016):
    the user_grant tombstone won over node 3's stale alive row."""
    deadline = time.time() + 90
    last = "user not found"
    while time.time() < deadline:
        uid = _r5_user_id(NODES[3])
        if uid:
            grants = _admin_get(NODES[3], f"/admin/users/{uid}/grants")
            if not any(g.get("name") == R5_GRANT_NAME for g in grants):
                return
            last = f"attachment still present: {[g.get('name') for g in grants]}"
        time.sleep(2)
    pytest.fail(f"grant detach did not reconcile to node 3 within 90s: {last}")


@pytest.mark.cluster_catchup_verify
def test_r5_bucket_config_and_tags_reconciled_on_returned_node():
    """The versioning flip and the bucket tag set changed while node 3 was down
    arrive via the snapshot reconcile (TD-016)."""
    node3 = _s3(NODES[3])
    deadline = time.time() + 90
    last = "never queried"
    while time.time() < deadline:
        try:
            versioning = node3.get_bucket_versioning(Bucket=CATCHUP_CP_BUCKET).get("Status")
            tags = {
                t["Key"]: t["Value"]
                for t in node3.get_bucket_tagging(Bucket=FAILOVER_BUCKET)["TagSet"]
            }
            if versioning == "Enabled" and tags.get(R5_TAG_KEY) == R5_TAG_VALUE:
                return
            last = f"versioning={versioning}, tags={tags}"
        except ClientError as e:
            last = e.response["Error"].get("Code", str(e))
        time.sleep(2)
    pytest.fail(f"bucket config/tags did not reconcile to node 3 within 90s: {last}")


@pytest.mark.cluster_catchup_verify
def test_r5_retention_reconciled_on_returned_node():
    """The retention set while node 3 was down appears on it (N1): the lock
    UPDATE stamped a fresh seq, so the changed-since manifest re-delivered the
    row with the lock columns."""
    node3 = _s3(NODES[3])
    deadline = time.time() + 90
    last = "never queried"
    while time.time() < deadline:
        try:
            retention = node3.get_object_retention(
                Bucket=R5_LOCK_BUCKET, Key=R5_LOCK_KEY
            )["Retention"]
            if retention.get("Mode") == "GOVERNANCE":
                return
            last = f"retention={retention}"
        except ClientError as e:
            last = e.response["Error"].get("Code", str(e))
        time.sleep(2)
    pytest.fail(f"retention change did not reconcile to node 3 within 90s: {last}")


@pytest.mark.cluster_catchup_verify
def test_r5_aborted_multipart_gone_everywhere():
    """The upload aborted while node 3 was down is closed on it at re-entry and
    does NOT resurrect on nodes 1/2 from node 3's stale row (D4 tombstone)."""
    deadline = time.time() + 90
    while time.time() < deadline:
        if _mp_upload_id(_s3(NODES[3]), R5_MP_BUCKET, R5_MP_ABORT_KEY) is None:
            break
        time.sleep(2)
    else:
        pytest.fail("aborted multipart upload still listed on node 3 after 90s")
    for n in (1, 2):
        assert _mp_upload_id(_s3(NODES[n]), R5_MP_BUCKET, R5_MP_ABORT_KEY) is None, (
            f"aborted upload resurrected on node {n}"
        )


@pytest.mark.cluster_catchup_verify
def test_r5_multipart_completes_on_returned_node():
    """The upload begun while node 3 was down can be COMPLETED on node 3 (D4):
    the upload + part rows arrive via the snapshot reconcile, and the part
    bytes — never fanned out to a dead node — are fetched from a peer by the
    concat pre-check (or already repaired by anti-entropy)."""
    node3 = _s3(NODES[3])
    deadline = time.time() + 90
    upload_id, parts = None, []
    while time.time() < deadline:
        upload_id = _mp_upload_id(node3, R5_MP_BUCKET, R5_MP_CATCHUP_KEY)
        if upload_id:
            parts = node3.list_parts(
                Bucket=R5_MP_BUCKET, Key=R5_MP_CATCHUP_KEY, UploadId=upload_id
            ).get("Parts", [])
            if parts:
                break
        time.sleep(2)
    else:
        pytest.fail(
            f"multipart catch-up incomplete on node 3 after 90s: "
            f"upload_id={upload_id}, parts={parts}"
        )
    node3.complete_multipart_upload(
        Bucket=R5_MP_BUCKET,
        Key=R5_MP_CATCHUP_KEY,
        UploadId=upload_id,
        MultipartUpload={
            "Parts": [{"PartNumber": p["PartNumber"], "ETag": p["ETag"]} for p in parts]
        },
    )
    _wait_object(node3, R5_MP_BUCKET, R5_MP_CATCHUP_KEY, R5_MP_CATCHUP_BODY, timeout=30)


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


# ── Phase: worker-leader gate (R6, decision H5) ───────────────────────────────
#
# The lifecycle evaluator is a cluster-singleton: only the node with the lowest
# node_id among the ELIGIBLE nodes runs the tick. The cluster configs set
# [lifecycle] interval_seconds = 5 so an expiry is observable within seconds.
# The runner stops the leader container between the two sub-phases (it finds it
# via /admin/health?verbose=1 → .cluster.worker_leader).

LEADER_BUCKET = "cluster-leader-lifecycle"
LEADER_KEY_FULL = "expires-with-all-nodes-up"
LEADER_KEY_FAILOVER = "expires-after-leader-failover"
# Any past date makes the rule expire immediately (Days: 0 is rejected).
LEADER_EXPIRE_DATE = "2020-01-01T00:00:00Z"


def _verbose_health(endpoint):
    """The /admin/health?verbose=1 JSON, or None if the node is unreachable."""
    try:
        resp = requests.get(f"{endpoint}/admin/health?verbose=1", timeout=5)
        resp.raise_for_status()
        return resp.json()
    except requests.RequestException:
        return None


def _leader_claims():
    """Maps node number → worker_leader claim, for the reachable nodes only."""
    claims = {}
    for n, endpoint in NODES.items():
        health = _verbose_health(endpoint)
        if health is not None and health.get("cluster"):
            claims[n] = health["cluster"]["worker_leader"]
    return claims


def _expiry_audit_count(key):
    """Total Lifecycle::ExpireObject audit entries for `key` across the
    reachable nodes. The audit log is node-local and the expiring node is the
    only one that writes the entry (peers apply the replicated delete without
    auditing), so this counts how many nodes ran the expiry.

    The `::` in the operation value MUST be pre-encoded: SigV4 signs the
    canonical (percent-encoded) query, so a raw `:` sent on the wire makes the
    server compute a different canonical string -> SignatureDoesNotMatch."""
    total = 0
    for endpoint in NODES.values():
        try:
            page = _admin_get(
                endpoint,
                "/admin/audit?operation=Lifecycle%3A%3AExpireObject"
                f"&bucket={LEADER_BUCKET}&limit=1000",
            )
        except (requests.ConnectionError, requests.Timeout):
            continue  # a stopped node has no audit log to count
        total += sum(1 for e in page["entries"] if e.get("key") == key)
    return total


def _assert_exactly_one_expiry(key):
    """The audit entry lands right AFTER the replicated delete, so poll for it;
    then re-check after a full lifecycle interval, so a second node expiring on
    its own tick (a broken gate) is caught rather than raced past."""
    deadline = time.time() + 15
    while time.time() < deadline and _expiry_audit_count(key) < 1:
        time.sleep(1)
    count = _expiry_audit_count(key)
    assert count == 1, f"expected exactly one expiry audit entry, got {count}"
    time.sleep(7)  # > the 5s [lifecycle] interval of the test configs
    count = _expiry_audit_count(key)
    assert count == 1, f"a second node also expired {key}: {count} audit entries"


def _put_and_wait_expiry(key):
    """PUT an object into the lifecycle bucket via a live node, then poll until
    the (already-past-date) rule expires it everywhere that is reachable."""
    live = [n for n, h in ((n, _verbose_health(e)) for n, e in NODES.items()) if h]
    assert live, "no reachable node"
    client = _s3(NODES[live[0]])
    _wait_put(client, LEADER_BUCKET, key, b"doomed by the lifecycle rule")

    deadline = time.time() + 60
    while time.time() < deadline:
        gone = True
        for n in live:
            try:
                _s3(NODES[n]).head_object(Bucket=LEADER_BUCKET, Key=key)
                gone = False
            except ClientError as e:
                if _status_code(e) != 404:
                    gone = False
        if gone:
            return
        time.sleep(2)
    pytest.fail(f"{LEADER_BUCKET}/{key} was not expired within 60s")


@pytest.mark.cluster_leader_full
def test_exactly_one_worker_leader():
    """With all 3 nodes up and eligible, exactly one claims the worker-leader
    role, and it is the node with the lowest node_id."""
    claims = _leader_claims()
    assert len(claims) == 3, f"expected 3 reachable nodes, got {claims}"
    leaders = [n for n, is_leader in claims.items() if is_leader]
    assert len(leaders) == 1, f"expected exactly one leader, got {claims}"

    node_ids = {
        n: _verbose_health(e)["cluster"]["node_id"] for n, e in NODES.items()
    }
    assert node_ids[leaders[0]] == min(node_ids.values()), (
        f"leader {leaders[0]} should hold the lowest node_id: {node_ids}"
    )


@pytest.mark.cluster_leader_full
def test_lifecycle_expiry_happens_exactly_once():
    """An already-expired lifecycle rule deletes the object ONCE cluster-wide:
    only the leader evaluates the rule (its audit log records the expiry), and
    its delete replicates to the peers without further evaluation."""
    lb = _s3(LB)
    _ensure_bucket(lb, LEADER_BUCKET)
    lb.put_bucket_lifecycle_configuration(
        Bucket=LEADER_BUCKET,
        LifecycleConfiguration={
            "Rules": [
                {
                    "ID": "expire-immediately",
                    "Status": "Enabled",
                    "Filter": {"Prefix": ""},
                    "Expiration": {"Date": LEADER_EXPIRE_DATE},
                }
            ]
        },
    )
    _put_and_wait_expiry(LEADER_KEY_FULL)
    _assert_exactly_one_expiry(LEADER_KEY_FULL)


@pytest.mark.cluster_leader_failover
def test_new_leader_elected_after_leader_stop():
    """With the previous leader stopped, exactly one of the survivors takes
    the role (the next-lowest eligible node_id) at its membership tick."""
    claims = _leader_claims()
    assert len(claims) == 2, f"expected 2 reachable nodes, got {claims}"
    leaders = [n for n, is_leader in claims.items() if is_leader]
    assert len(leaders) == 1, f"expected exactly one leader, got {claims}"


@pytest.mark.cluster_leader_failover
def test_failover_expiry_happens_exactly_once():
    """The lifecycle singleton keeps working after the leader is gone: a new
    object in the same (already-configured) bucket is expired exactly once,
    by the new leader."""
    _put_and_wait_expiry(LEADER_KEY_FAILOVER)
    _assert_exactly_one_expiry(LEADER_KEY_FAILOVER)


# ── Phase: syncing readiness (R7, review D2) ──────────────────────────────────
#
# A node returning from downtime must not serve stale 404s/partial listings:
# /admin/health answers 503 {"status":"syncing"} until its first anti-entropy
# pass toward every eligible peer completes, so the LB keeps it out of rotation
# exactly while it could give wrong answers. The runner restarts arca-3 and
# starts this sub-phase IMMEDIATELY (no wait_live), so the polling below can
# observe the syncing window (membership 3s + anti-entropy tick 5s in the test
# configs) before it closes.

SYNC_BUCKET = "cluster-syncing"
SYNC_SEED_KEY = "seeded-before-downtime"
SYNC_SEED_BODY = b"seeded on all three nodes before the downtime"
SYNC_CATCHUP_KEY = "written-while-node-3-was-down"
SYNC_CATCHUP_BODY = b"node 3 must already hold this when its health turns 200"


@pytest.mark.cluster_syncing_seed
def test_syncing_seed_data_on_all_nodes():
    """Seed an object with all 3 nodes up; verify it reached node 3."""
    lb = _s3(LB)
    _ensure_bucket(lb, SYNC_BUCKET)
    lb.put_object(Bucket=SYNC_BUCKET, Key=SYNC_SEED_KEY, Body=SYNC_SEED_BODY)
    _wait_object(_s3(NODES[3]), SYNC_BUCKET, SYNC_SEED_KEY, SYNC_SEED_BODY)


@pytest.mark.cluster_syncing_while_down
def test_syncing_write_catchup_data_while_node_down():
    """With node 3 down, write the object it will have to catch up on."""
    lb = _s3(LB)
    lb.put_object(Bucket=SYNC_BUCKET, Key=SYNC_CATCHUP_KEY, Body=SYNC_CATCHUP_BODY)
    assert (
        lb.get_object(Bucket=SYNC_BUCKET, Key=SYNC_CATCHUP_KEY)["Body"].read()
        == SYNC_CATCHUP_BODY
    )


@pytest.mark.cluster_syncing_readiness
def test_restarted_node_syncs_before_reporting_ready():
    """From the instant node 3 restarts: every health answer before the first
    200 must be a 503 "syncing" (with the M4 Retry-After hint), the verbose
    health must stay inspectable (200) meanwhile, and the moment the plain
    health turns 200 the catch-up object must ALREADY be readable on the node
    — readiness implies correctness, not just liveness."""
    deadline = time.time() + 120
    saw_syncing = False
    ready = False
    while time.time() < deadline:
        try:
            resp = requests.get(f"{NODES[3]}/admin/health", timeout=2)
        except requests.RequestException:
            time.sleep(0.2)  # still booting — connection errors are fine
            continue
        if resp.status_code == 200:
            ready = True
            break
        assert resp.status_code == 503, f"unexpected health status {resp.status_code}"
        body = resp.json()
        assert body.get("status") == "syncing", f"unexpected 503 body: {body}"
        assert resp.headers.get("Retry-After") == "5", "M4: retriable 503 must hint a delay"
        if not saw_syncing:
            saw_syncing = True
            # The verbose health never 503s: an operator/console can always
            # inspect the node; the status field carries the state.
            verbose = _verbose_health(NODES[3])
            assert verbose is not None and verbose["status"] == "syncing"
            assert verbose["cluster"]["syncing"] is True
        time.sleep(0.2)
    assert ready, "node 3 never reported ready within 120s"
    assert saw_syncing, "the syncing window was never observed (raced past it?)"

    # Readiness implies correctness: the data written during the downtime is
    # already there the moment the health says ok — no polling allowed here.
    node3 = _s3(NODES[3])
    assert (
        node3.get_object(Bucket=SYNC_BUCKET, Key=SYNC_CATCHUP_KEY)["Body"].read()
        == SYNC_CATCHUP_BODY
    )
    assert (
        node3.get_object(Bucket=SYNC_BUCKET, Key=SYNC_SEED_KEY)["Body"].read()
        == SYNC_SEED_BODY
    )

    # The admin topology now reports the sync state: not syncing, first pass
    # done toward every peer, per-peer cursors exposed (D2 — exposed lag).
    cluster = _admin_get(NODES[3], "/admin/cluster")
    assert cluster["syncing"] is False
    peer_syncs = [n["sync"] for n in cluster["nodes"] if not n["local"]]
    assert peer_syncs, "expected per-peer sync detail in /admin/cluster"
    for sync in peer_syncs:
        assert sync["first_pass_done"] is True
        assert sync["skipped_entries"] == 0
        assert "hwm" in sync
