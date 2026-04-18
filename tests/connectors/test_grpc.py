"""Integration tests for the gRPC notification connector.

The gRPC receiver (`docker/grpc-receiver/`) implements the
`arca.notifications.v1.NotificationService` service on port 50051 and exposes an
HTTP admin interface on port 8080 for the test suite to query captured events.
"""

import json
import os
import time

import pytest
import requests

from conftest import sigv4_request


GRPC_URL = os.environ.get("GRPC_URL", "http://grpc-receiver:50051")
GRPC_ADMIN_URL = os.environ.get("GRPC_ADMIN_URL", "http://grpc-receiver:8080")
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")


def put_notification_config_xml(bucket, grpc_url, events=None, properties=None):
    """Install a bucket notification using the gRPC connector."""
    event_elements = ""
    for evt in (events or ["s3:ObjectCreated:*"]):
        event_elements += f"    <Event>{evt}</Event>\n"

    prop_elements = ""
    for name, value in (properties or {}).items():
        prop_elements += f"    <Property><Name>{name}</Name><Value>{value}</Value></Property>\n"

    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <TopicConfiguration>
    <Id>grpc-test</Id>
    <Topic>{grpc_url}</Topic>
{event_elements}    <ConnectorType>grpc</ConnectorType>
{prop_elements}  </TopicConfiguration>
</NotificationConfiguration>"""

    url = f"{ARCA_ENDPOINT}/{bucket}?notification"
    resp = sigv4_request("PUT", url, data=xml, headers={"Content-Type": "application/xml"})
    assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"


def clear_grpc_messages():
    try:
        requests.delete(f"{GRPC_ADMIN_URL}/messages", timeout=5)
    except requests.RequestException:
        pass


def fetch_grpc_messages():
    resp = requests.get(f"{GRPC_ADMIN_URL}/messages", timeout=5)
    resp.raise_for_status()
    return resp.json()


def wait_for_grpc_messages(min_count=1, timeout=15):
    deadline = time.time() + timeout
    msgs = []
    while time.time() < deadline:
        msgs = fetch_grpc_messages()
        if len(msgs) >= min_count:
            return msgs
        time.sleep(0.5)
    return msgs


class TestGrpcDelivery:
    """End-to-end delivery through the gRPC connector."""

    def setup_method(self):
        clear_grpc_messages()

    def test_put_object_invokes_notify_rpc(self, s3_client, unique_bucket):
        """PutObject must drive one unary Notify call with the S3 event JSON."""
        put_notification_config_xml(unique_bucket, GRPC_URL)
        s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")

        msgs = wait_for_grpc_messages(min_count=1)
        assert len(msgs) >= 1
        payload = json.loads(msgs[0]["event_payload"])
        records = payload.get("Records", [])
        assert len(records) >= 1
        assert records[0]["eventName"] == "s3:ObjectCreated:Put"
        assert records[0]["s3"]["bucket"]["name"] == unique_bucket
        assert records[0]["s3"]["object"]["key"] == "hello.txt"

    def test_auth_token_forwarded_as_bearer_metadata(self, s3_client, unique_bucket):
        """`auth_token` must be forwarded as an `authorization: Bearer ...` gRPC metadata entry."""
        put_notification_config_xml(
            unique_bucket, GRPC_URL,
            properties={"auth_token": "secret-token"},
        )
        s3_client.put_object(Bucket=unique_bucket, Key="auth.txt", Body=b"x")

        msgs = wait_for_grpc_messages(min_count=1)
        assert len(msgs) >= 1
        grpc_meta = msgs[0].get("grpc_metadata", {})
        assert grpc_meta.get("authorization") == "Bearer secret-token"
        # auth_token itself must NOT leak into the user metadata map
        assert "auth_token" not in msgs[0].get("metadata", {})

    def test_user_metadata_forwarded(self, s3_client, unique_bucket):
        """Arbitrary key/value properties must be forwarded via the proto `metadata` map."""
        put_notification_config_xml(
            unique_bucket, GRPC_URL,
            properties={"tenant": "acme", "channel": "alerts"},
        )
        s3_client.put_object(Bucket=unique_bucket, Key="meta.txt", Body=b"x")

        msgs = wait_for_grpc_messages(min_count=1)
        assert len(msgs) >= 1
        meta = msgs[0].get("metadata", {})
        assert meta.get("tenant") == "acme"
        assert meta.get("channel") == "alerts"

    def test_delete_object_invokes_notify_rpc(self, s3_client, unique_bucket):
        """DeleteObject must drive an ObjectRemoved Notify call."""
        put_notification_config_xml(
            unique_bucket, GRPC_URL,
            events=["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
        )
        s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
        time.sleep(2)
        s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")

        msgs = wait_for_grpc_messages(min_count=2, timeout=20)
        removed = [
            m for m in msgs
            if "ObjectRemoved" in m.get("event_payload", "")
        ]
        assert len(removed) >= 1

    def test_multiple_events(self, s3_client, unique_bucket):
        """Multiple S3 operations produce multiple Notify calls."""
        put_notification_config_xml(unique_bucket, GRPC_URL)
        for i in range(3):
            s3_client.put_object(
                Bucket=unique_bucket, Key=f"multi-{i}.txt", Body=f"d{i}".encode()
            )

        msgs = wait_for_grpc_messages(min_count=3, timeout=20)
        assert len(msgs) >= 3
        keys = set()
        for m in msgs:
            payload = json.loads(m["event_payload"])
            for r in payload.get("Records", []):
                keys.add(r["s3"]["object"]["key"])
        for i in range(3):
            assert f"multi-{i}.txt" in keys


class TestGrpcConnectivity:
    """Admin API /admin/notifications/test-connector probe."""

    def test_grpc_connectivity_success(self):
        """Valid gRPC URL must succeed against the Python receiver."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "grpc",
            "url": GRPC_URL,
            "properties": {},
        })
        resp = sigv4_request(
            "POST", url, data=body, headers={"Content-Type": "application/json"}
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is True, f"unexpected response: {data}"
        assert data["connector_type"] == "grpc"

    def test_grpc_connectivity_failure(self):
        """Unreachable gRPC host must fail gracefully with success=false."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "grpc",
            "url": "http://nonexistent-host:50051",
            "properties": {},
        })
        resp = sigv4_request(
            "POST", url, data=body, headers={"Content-Type": "application/json"}
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is False
        assert data["error"] is not None
