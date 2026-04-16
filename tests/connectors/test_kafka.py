"""Integration tests for the Kafka notification connector."""

import json
import os
import queue
import threading
import time

import pytest
from kafka import KafkaConsumer

from conftest import sigv4_request


KAFKA_URL = os.environ.get("KAFKA_URL", "kafka-receiver:9092")
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")

DEFAULT_TOPIC = "arca-notifications"


class KafkaSubscriber:
    """Background Kafka consumer that collects messages into a queue."""

    def __init__(self, bootstrap_servers, topic):
        self.bootstrap_servers = bootstrap_servers
        self.topic = topic
        self.messages = queue.Queue()
        self._stop = threading.Event()
        self._ready = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self):
        self._thread.start()
        self._ready.wait(timeout=30)

    def stop(self):
        self._stop.set()
        self._thread.join(timeout=10)

    def _run(self):
        consumer = KafkaConsumer(
            self.topic,
            bootstrap_servers=self.bootstrap_servers,
            auto_offset_reset="earliest",
            consumer_timeout_ms=1000,
            value_deserializer=lambda m: json.loads(m.decode("utf-8")),
            group_id=f"test-{self.topic}-{int(time.time())}",
        )
        self._ready.set()

        while not self._stop.is_set():
            records = consumer.poll(timeout_ms=500)
            for tp, msgs in records.items():
                for msg in msgs:
                    self.messages.put(msg.value)

        consumer.close()

    def get_messages(self, timeout=15, min_count=1):
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


def put_notification_config_xml(bucket, kafka_url, topic=None, events=None):
    """Configure a bucket notification with Kafka connector via raw XML."""
    event_elements = ""
    for evt in (events or ["s3:ObjectCreated:*"]):
        event_elements += f"    <Event>{evt}</Event>\n"

    properties = ""
    if topic:
        properties += f"    <Property><Name>topic</Name><Value>{topic}</Value></Property>\n"

    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <QueueConfiguration>
    <Id>kafka-test</Id>
    <Queue>{kafka_url}</Queue>
{event_elements}    <ConnectorType>kafka</ConnectorType>
{properties}  </QueueConfiguration>
</NotificationConfiguration>"""

    url = f"{ARCA_ENDPOINT}/{bucket}?notification"
    resp = sigv4_request("PUT", url, data=xml, headers={"Content-Type": "application/xml"})
    assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"


class TestKafkaDelivery:
    """Tests for Kafka event delivery."""

    def test_put_object_produces_to_kafka(self, s3_client, unique_bucket):
        """PutObject should produce an S3 event to the default Kafka topic."""
        with KafkaSubscriber(KAFKA_URL, DEFAULT_TOPIC) as sub:
            put_notification_config_xml(unique_bucket, KAFKA_URL)
            s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")

            messages = sub.get_messages(timeout=15, min_count=1)
            assert len(messages) >= 1
            records = messages[0].get("Records", [])
            assert len(records) >= 1
            assert records[0]["eventName"] == "s3:ObjectCreated:Put"
            assert records[0]["s3"]["bucket"]["name"] == unique_bucket
            assert records[0]["s3"]["object"]["key"] == "hello.txt"

    def test_custom_topic(self, s3_client, unique_bucket):
        """Events should be produced to the topic specified in properties."""
        custom_topic = f"test-{unique_bucket}"
        with KafkaSubscriber(KAFKA_URL, custom_topic) as sub:
            put_notification_config_xml(unique_bucket, KAFKA_URL, topic=custom_topic)
            s3_client.put_object(Bucket=unique_bucket, Key="custom.txt", Body=b"data")

            messages = sub.get_messages(timeout=15, min_count=1)
            assert len(messages) >= 1

    def test_delete_object_produces_to_kafka(self, s3_client, unique_bucket):
        """DeleteObject should produce an s3:ObjectRemoved event."""
        with KafkaSubscriber(KAFKA_URL, DEFAULT_TOPIC) as sub:
            put_notification_config_xml(
                unique_bucket, KAFKA_URL,
                events=["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
            )
            s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
            # Consume the create event
            sub.get_messages(timeout=10, min_count=1)

            s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")
            messages = sub.get_messages(timeout=15, min_count=1)
            assert len(messages) >= 1
            records = messages[0].get("Records", [])
            assert len(records) >= 1
            assert "ObjectRemoved" in records[0]["eventName"]

    def test_multiple_events(self, s3_client, unique_bucket):
        """Multiple S3 operations should produce multiple Kafka messages."""
        with KafkaSubscriber(KAFKA_URL, DEFAULT_TOPIC) as sub:
            put_notification_config_xml(unique_bucket, KAFKA_URL)

            for i in range(3):
                s3_client.put_object(
                    Bucket=unique_bucket, Key=f"multi-{i}.txt", Body=f"data-{i}".encode()
                )

            messages = sub.get_messages(timeout=20, min_count=3)
            assert len(messages) >= 3

    def test_event_payload_format(self, s3_client, unique_bucket):
        """Verify the Kafka message payload matches the S3 event record format."""
        with KafkaSubscriber(KAFKA_URL, DEFAULT_TOPIC) as sub:
            put_notification_config_xml(unique_bucket, KAFKA_URL)
            s3_client.put_object(Bucket=unique_bucket, Key="format.txt", Body=b"test data")

            messages = sub.get_messages(timeout=15, min_count=1)
            assert len(messages) >= 1
            payload = messages[0]

            assert "Records" in payload
            record = payload["Records"][0]
            assert record["eventVersion"] == "2.1"
            assert record["eventSource"] == "arca:s3"
            assert "eventTime" in record
            assert record["eventName"] == "s3:ObjectCreated:Put"


class TestKafkaConnectivity:
    """Tests for the admin API connector test endpoint."""

    def test_kafka_connectivity_success(self):
        """Admin API test-connector with valid Kafka URL should succeed."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "kafka",
            "url": KAFKA_URL,
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is True
        assert data["connector_type"] == "kafka"

    def test_kafka_connectivity_failure(self):
        """Admin API test-connector with bad Kafka URL should fail gracefully."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "kafka",
            "url": "nonexistent-host:9092",
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is False
        assert data["error"] is not None
