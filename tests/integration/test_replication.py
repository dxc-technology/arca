"""Phase 28 — Replication integration tests.

Exercises the replication end-to-end against two Arca instances:
- ARCA_ENDPOINT          — the source instance (default: http://arca:9000)
- ARCA_REPLICA_ENDPOINT  — the destination instance (default: http://arca-replica:9000)

The tests:
1. Configure replication (credential + rule) on the source.
2. Write objects on the source.
3. Poll the replica until the object appears (or times out).
4. Assert `x-amz-replication-status` headers on both sides.
5. Verify the two-way mirror configuration does not produce infinite loops.
"""

import os
import time
import uuid

import boto3
import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials


SOURCE_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")
REPLICA_ENDPOINT = os.environ.get("ARCA_REPLICA_ENDPOINT", "http://arca-replica:9000")
ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
SECRET_KEY = os.environ.get("AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv")
REGION = "us-east-1"


# ── Fixtures ────────────────────────────────────────────────────────────────

def _s3(endpoint):
    return boto3.client(
        "s3",
        endpoint_url=endpoint,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        region_name=REGION,
    )


@pytest.fixture
def source():
    return _s3(SOURCE_ENDPOINT)


@pytest.fixture
def replica():
    return _s3(REPLICA_ENDPOINT)


@pytest.fixture
def bucket_name():
    # Same bucket name on both sides — makes the mirror test symmetric.
    return f"repl-{uuid.uuid4().hex[:10]}"


@pytest.fixture
def repl_bucket(source, replica, bucket_name):
    """Create a versioned bucket on both sides; clean up after the test."""
    for client in (source, replica):
        client.create_bucket(Bucket=bucket_name)
        client.put_bucket_versioning(
            Bucket=bucket_name,
            VersioningConfiguration={"Status": "Enabled"},
        )
    yield bucket_name
    # Teardown: best-effort wipe on both sides.
    for client in (source, replica):
        _wipe(client, bucket_name)


def _wipe(client, bucket):
    try:
        versions = client.list_object_versions(Bucket=bucket)
        for v in versions.get("Versions", []) + versions.get("DeleteMarkers", []):
            client.delete_object(Bucket=bucket, Key=v["Key"], VersionId=v["VersionId"])
        # Drop any replication configuration first so the delete succeeds.
        _sign_and_send("DELETE", SOURCE_ENDPOINT, f"/{bucket}/?replication=", b"")
        _sign_and_send("DELETE", REPLICA_ENDPOINT, f"/{bucket}/?replication=", b"")
        client.delete_bucket(Bucket=bucket)
    except Exception:
        pass


# ── Helpers: signed requests + admin calls ──────────────────────────────────

_CREDS = Credentials(access_key=ACCESS_KEY, secret_key=SECRET_KEY)


def _sign_and_send(method, endpoint, path_and_query, body, extra_headers=None):
    """Send a SigV4-signed request to an Arca endpoint.

    ``path_and_query`` is written as ``/bucket/key?query`` and is passed through
    verbatim to the server (after being joined to the endpoint host).
    """
    url = f"{endpoint.rstrip('/')}{path_and_query}"
    headers = dict(extra_headers or {})
    req = AWSRequest(method=method, url=url, data=body or "", headers=headers)
    # S3SigV4Auth matches S3's canonical-URI handling (no double-encoding).
    S3SigV4Auth(_CREDS, "s3", REGION).add_auth(req)
    prepared = dict(req.headers.items())
    resp = requests.request(method, url, data=body, headers=prepared, timeout=15)
    return resp


def _put_bucket_replication(endpoint, bucket, rule_id, dest_endpoint, dest_bucket,
                            credential_ref, prefix="", delete_markers=True):
    """Upload an Arca-flavoured ReplicationConfiguration via a signed PUT."""
    dmr = "Enabled" if delete_markers else "Disabled"
    filter_xml = f"<Prefix>{prefix}</Prefix>" if prefix else "<Prefix></Prefix>"
    xml = f"""<ReplicationConfiguration>
  <Role>arn:aws:iam::test:role/x</Role>
  <Rule>
    <ID>{rule_id}</ID>
    <Status>Enabled</Status>
    <Priority>1</Priority>
    <Filter>{filter_xml}</Filter>
    <Destination>
      <Bucket>{dest_bucket}</Bucket>
      <Endpoint>{dest_endpoint}</Endpoint>
      <Region>{REGION}</Region>
      <CredentialRef>{credential_ref}</CredentialRef>
    </Destination>
    <DeleteMarkerReplication><Status>{dmr}</Status></DeleteMarkerReplication>
  </Rule>
</ReplicationConfiguration>""".encode("utf-8")
    resp = _sign_and_send("PUT", endpoint, f"/{bucket}/?replication=", xml)
    assert resp.status_code == 200, f"put_bucket_replication failed: {resp.status_code} {resp.text}"


def _upsert_credential(endpoint, name, access_key, secret_key):
    """Upload a destination credential via the admin API."""
    body = (
        '{"access_key_id":"' + access_key + '","secret_access_key":"' + secret_key + '"}'
    ).encode("utf-8")
    resp = _sign_and_send(
        "POST", endpoint, f"/admin/replication/credentials/{name}", body,
        extra_headers={"Content-Type": "application/json"},
    )
    assert resp.status_code == 200, f"upsert_credential failed: {resp.status_code} {resp.text}"


def _list_journal(endpoint, bucket=None, status=None):
    path = "/admin/replication/journal"
    params = []
    if bucket:
        params.append(f"bucket={bucket}")
    if status:
        params.append(f"status={status}")
    if params:
        path = path + "?" + "&".join(params)
    resp = _sign_and_send("GET", endpoint, path, b"")
    assert resp.status_code == 200, f"list_journal failed: {resp.status_code} {resp.text}"
    return resp.json()


def _wait_for_object(client, bucket, key, timeout=30):
    """Poll HEAD until the object exists, or raise."""
    deadline = time.time() + timeout
    last_exc = None
    while time.time() < deadline:
        try:
            return client.head_object(Bucket=bucket, Key=key)
        except Exception as e:
            last_exc = e
            time.sleep(0.5)
    raise AssertionError(f"object {bucket}/{key} did not appear within {timeout}s: {last_exc}")


def _wait_for_delete_marker(client, bucket, key, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        versions = client.list_object_versions(Bucket=bucket, Prefix=key)
        for marker in versions.get("DeleteMarkers", []):
            if marker["Key"] == key:
                return marker
        time.sleep(0.5)
    raise AssertionError(f"delete marker for {bucket}/{key} did not appear within {timeout}s")


def _wait_for_status(endpoint, bucket, key, expected_status, timeout=30):
    """Poll HeadObject on the given side and wait for the replication-status header."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        resp = _sign_and_send("HEAD", endpoint, f"/{bucket}/{key}", b"")
        if resp.status_code == 200:
            last = resp.headers.get("x-amz-replication-status")
            if last == expected_status:
                return last
        time.sleep(0.5)
    raise AssertionError(
        f"{bucket}/{key} on {endpoint} did not reach status {expected_status} "
        f"within {timeout}s (last={last})"
    )


# ── Setup helper: credential + rule on the source ───────────────────────────

def _configure_source_to_replica(bucket):
    _upsert_credential(SOURCE_ENDPOINT, "replica-creds", ACCESS_KEY, SECRET_KEY)
    _put_bucket_replication(
        SOURCE_ENDPOINT, bucket, "rule-1",
        dest_endpoint=REPLICA_ENDPOINT, dest_bucket=bucket,
        credential_ref="replica-creds",
    )


# ── Tests ───────────────────────────────────────────────────────────────────

class TestBasicReplication:
    def test_put_object_replicates(self, source, replica, repl_bucket):
        _configure_source_to_replica(repl_bucket)

        payload = b"hello replica"
        source.put_object(Bucket=repl_bucket, Key="doc.txt", Body=payload)

        # Poll the replica until the object appears.
        _wait_for_object(replica, repl_bucket, "doc.txt")
        resp = replica.get_object(Bucket=repl_bucket, Key="doc.txt")
        assert resp["Body"].read() == payload

        # Source should stamp COMPLETED once the worker succeeds.
        status = _wait_for_status(SOURCE_ENDPOINT, repl_bucket, "doc.txt", "COMPLETED")
        assert status == "COMPLETED"

        # Replica should stamp REPLICA because of the loop-prevention header.
        replica_status = _wait_for_status(
            REPLICA_ENDPOINT, repl_bucket, "doc.txt", "REPLICA"
        )
        assert replica_status == "REPLICA"

    def test_delete_marker_replicates(self, source, replica, repl_bucket):
        _configure_source_to_replica(repl_bucket)
        source.put_object(Bucket=repl_bucket, Key="x", Body=b"1")
        _wait_for_object(replica, repl_bucket, "x")

        # Delete on source creates a delete marker (versioned bucket).
        source.delete_object(Bucket=repl_bucket, Key="x")

        # Replica should get a delete marker too.
        _wait_for_delete_marker(replica, repl_bucket, "x")

    def test_tags_replicate(self, source, replica, repl_bucket):
        _configure_source_to_replica(repl_bucket)
        source.put_object(Bucket=repl_bucket, Key="tagged", Body=b"data")
        _wait_for_object(replica, repl_bucket, "tagged")

        source.put_object_tagging(
            Bucket=repl_bucket, Key="tagged",
            Tagging={"TagSet": [{"Key": "env", "Value": "prod"}]},
        )

        deadline = time.time() + 30
        found = None
        while time.time() < deadline:
            try:
                tags = replica.get_object_tagging(Bucket=repl_bucket, Key="tagged")
                tag_set = {t["Key"]: t["Value"] for t in tags.get("TagSet", [])}
                if tag_set.get("env") == "prod":
                    found = tag_set
                    break
            except Exception:
                pass
            time.sleep(0.5)
        assert found is not None, "tags did not replicate"


class TestLoopPrevention:
    def test_mirror_does_not_loop(self, source, replica, repl_bucket):
        # Configure mirror rules on BOTH sides, same bucket name.
        _upsert_credential(SOURCE_ENDPOINT, "replica-creds", ACCESS_KEY, SECRET_KEY)
        _upsert_credential(REPLICA_ENDPOINT, "source-creds", ACCESS_KEY, SECRET_KEY)
        _put_bucket_replication(
            SOURCE_ENDPOINT, repl_bucket, "rule-a-to-b",
            dest_endpoint=REPLICA_ENDPOINT, dest_bucket=repl_bucket,
            credential_ref="replica-creds",
        )
        _put_bucket_replication(
            REPLICA_ENDPOINT, repl_bucket, "rule-b-to-a",
            dest_endpoint=SOURCE_ENDPOINT, dest_bucket=repl_bucket,
            credential_ref="source-creds",
        )

        source.put_object(Bucket=repl_bucket, Key="once.txt", Body=b"one")
        _wait_for_object(replica, repl_bucket, "once.txt")

        # Wait for stable state on both sides, then snapshot journal counts.
        _wait_for_status(SOURCE_ENDPOINT, repl_bucket, "once.txt", "COMPLETED")
        _wait_for_status(REPLICA_ENDPOINT, repl_bucket, "once.txt", "REPLICA")

        time.sleep(3)  # settle

        src_journal = _list_journal(SOURCE_ENDPOINT, bucket=repl_bucket)
        rep_journal = _list_journal(REPLICA_ENDPOINT, bucket=repl_bucket)
        # Exactly one journal entry on the source (the client-originated PUT).
        put_rows_src = [e for e in src_journal["entries"] if e["event_type"] == "put"]
        assert len(put_rows_src) == 1, f"loop detected on source: {src_journal}"
        # Zero journal entries on the replica — the loop-prevention header made
        # the inbound write a REPLICA, which must NOT be re-emitted.
        put_rows_rep = [e for e in rep_journal["entries"] if e["event_type"] == "put"]
        assert len(put_rows_rep) == 0, f"loop detected on replica: {rep_journal}"
