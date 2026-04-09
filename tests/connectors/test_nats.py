"""Integration tests for the NATS notification connector."""

import asyncio
import json
import os
import queue
import threading
import time

import nats as nats_client
import pytest

from conftest import sigv4_request


NATS_URL = os.environ.get("NATS_URL", "nats://nats-receiver:4222")
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")

# Default subject used by the connector when no subject property is specified
DEFAULT_SUBJECT = "arca.notifications"


class NatsSubscriber:
    """Background NATS subscriber that collects messages into a queue."""

    def __init__(self, url, subject):
        self.url = url
        self.subject = subject
        self.messages = queue.Queue()
        self._stop = threading.Event()
        self._ready = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self):
        self._thread.start()
        self._ready.wait(timeout=10)

    def stop(self):
        self._stop.set()
        self._thread.join(timeout=5)

    def _run(self):
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        loop.run_until_complete(self._subscribe())

    async def _subscribe(self):
        nc = await nats_client.connect(self.url)
        sub = await nc.subscribe(self.subject)
        self._ready.set()
        while not self._stop.is_set():
            try:
                msg = await asyncio.wait_for(sub.next_msg(), timeout=0.5)
                try:
                    data = json.loads(msg.data)
                except (json.JSONDecodeError, TypeError):
                    data = msg.data
                self.messages.put(data)
            except asyncio.TimeoutError:
                pass
        await sub.unsubscribe()
        await nc.drain()

    def get_messages(self, timeout=10, min_count=1):
        """Collect messages until min_count is reached or timeout expires."""
        collected = []
        deadline = time.time() + timeout
        while len(collected) < min_count and time.time() < deadline:
            try:
                msg = self.messages.get(timeout=0.5)
                collected.append(msg)
            except queue.Empty:
                pass
        return collected

    def __enter__(self):
        self.start()
        return self

    def __exit__(self, *args):
        self.stop()


def put_notification_config_xml(bucket, nats_url, subject=None, events=None):
    """Configure a bucket notification with NATS connector via raw XML."""
    event_elements = ""
    for evt in (events or ["s3:ObjectCreated:*"]):
        event_elements += f"    <Event>{evt}</Event>\n"

    properties = f'    <Property><Name>subject</Name><Value>{subject}</Value></Property>\n' if subject else ""

    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <QueueConfiguration>
    <Id>nats-test</Id>
    <Queue>{nats_url}</Queue>
{event_elements}    <ConnectorType>nats</ConnectorType>
{properties}  </QueueConfiguration>
</NotificationConfiguration>"""

    url = f"{ARCA_ENDPOINT}/{bucket}?notification"
    resp = sigv4_request("PUT", url, data=xml, headers={"Content-Type": "application/xml"})
    assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"


class TestNatsDelivery:
    """Tests for NATS event delivery."""

    def test_put_object_publishes_to_nats(self, s3_client, unique_bucket):
        """PutObject should publish an S3 event to the default NATS subject."""
        with NatsSubscriber(NATS_URL, DEFAULT_SUBJECT) as sub:
            put_notification_config_xml(unique_bucket, NATS_URL)
            s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")

            messages = sub.get_messages(timeout=10, min_count=1)
            assert len(messages) >= 1
            records = messages[0].get("Records", [])
            assert len(records) >= 1
            assert records[0]["eventName"] == "s3:ObjectCreated:Put"
            assert records[0]["s3"]["bucket"]["name"] == unique_bucket
            assert records[0]["s3"]["object"]["key"] == "hello.txt"

    def test_custom_subject(self, s3_client, unique_bucket):
        """Events should be published to the subject specified in properties."""
        custom_subject = f"test.subject.{unique_bucket}"
        with NatsSubscriber(NATS_URL, custom_subject) as sub:
            put_notification_config_xml(unique_bucket, NATS_URL, subject=custom_subject)
            s3_client.put_object(Bucket=unique_bucket, Key="custom.txt", Body=b"data")

            messages = sub.get_messages(timeout=10, min_count=1)
            assert len(messages) >= 1
            records = messages[0].get("Records", [])
            assert len(records) >= 1
            assert records[0]["s3"]["object"]["key"] == "custom.txt"

    def test_delete_object_publishes_to_nats(self, s3_client, unique_bucket):
        """DeleteObject should publish an s3:ObjectRemoved event."""
        with NatsSubscriber(NATS_URL, DEFAULT_SUBJECT) as sub:
            put_notification_config_xml(
                unique_bucket, NATS_URL,
                events=["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
            )
            s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
            # Consume the create event
            sub.get_messages(timeout=5, min_count=1)

            s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")
            messages = sub.get_messages(timeout=10, min_count=1)
            assert len(messages) >= 1
            records = messages[0].get("Records", [])
            assert len(records) >= 1
            assert "ObjectRemoved" in records[0]["eventName"]

    def test_multiple_events(self, s3_client, unique_bucket):
        """Multiple S3 operations should produce multiple NATS messages."""
        with NatsSubscriber(NATS_URL, DEFAULT_SUBJECT) as sub:
            put_notification_config_xml(unique_bucket, NATS_URL)

            for i in range(3):
                s3_client.put_object(
                    Bucket=unique_bucket, Key=f"multi-{i}.txt", Body=f"data-{i}".encode()
                )

            messages = sub.get_messages(timeout=15, min_count=3)
            assert len(messages) >= 3
            keys = set()
            for msg in messages:
                for r in msg.get("Records", []):
                    keys.add(r["s3"]["object"]["key"])
            for i in range(3):
                assert f"multi-{i}.txt" in keys

    def test_event_payload_format(self, s3_client, unique_bucket):
        """Verify the NATS message payload matches the S3 event record format."""
        with NatsSubscriber(NATS_URL, DEFAULT_SUBJECT) as sub:
            put_notification_config_xml(unique_bucket, NATS_URL)
            s3_client.put_object(Bucket=unique_bucket, Key="format.txt", Body=b"test data")

            messages = sub.get_messages(timeout=10, min_count=1)
            assert len(messages) >= 1
            payload = messages[0]

            assert "Records" in payload
            record = payload["Records"][0]
            assert record["eventVersion"] == "2.1"
            assert record["eventSource"] == "arca:s3"
            assert "eventTime" in record
            assert record["eventName"] == "s3:ObjectCreated:Put"

            s3 = record["s3"]
            assert s3["s3SchemaVersion"] == "1.0"
            assert s3["bucket"]["name"] == unique_bucket
            assert s3["object"]["key"] == "format.txt"
            assert s3["object"]["size"] > 0


class TestNatsConnectivity:
    """Tests for the admin API connector test endpoint."""

    def test_nats_connectivity_success(self):
        """Admin API test-connector with valid NATS URL should succeed."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "nats",
            "url": NATS_URL,
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is True
        assert data["connector_type"] == "nats"
        assert data["status"] == "OK"

    def test_nats_connectivity_failure(self):
        """Admin API test-connector with bad NATS URL should fail gracefully."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "nats",
            "url": "nats://nonexistent-host:4222",
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is False
        assert data["error"] is not None
