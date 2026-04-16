"""Integration tests for the Elasticsearch notification connector."""

import json
import os
import time

import pytest
import requests as http_requests

from conftest import sigv4_request


ELASTICSEARCH_URL = os.environ.get("ELASTICSEARCH_URL", "http://elasticsearch-receiver:9200")
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")

DEFAULT_INDEX = "arca-notifications"


def put_notification_config_xml(bucket, es_url, index=None, events=None, properties=None):
    """Configure a bucket notification with Elasticsearch connector via raw XML."""
    event_elements = ""
    for evt in (events or ["s3:ObjectCreated:*"]):
        event_elements += f"    <Event>{evt}</Event>\n"

    prop_elements = ""
    if index:
        prop_elements += f"    <Property><Name>index</Name><Value>{index}</Value></Property>\n"
    for name, value in (properties or {}).items():
        prop_elements += f"    <Property><Name>{name}</Name><Value>{value}</Value></Property>\n"

    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <TopicConfiguration>
    <Id>es-test</Id>
    <Topic>{es_url}</Topic>
{event_elements}    <ConnectorType>elasticsearch</ConnectorType>
{prop_elements}  </TopicConfiguration>
</NotificationConfiguration>"""

    url = f"{ARCA_ENDPOINT}/{bucket}?notification"
    resp = sigv4_request("PUT", url, data=xml, headers={"Content-Type": "application/xml"})
    assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"


def wait_for_es_doc(index, key, timeout=15):
    """Poll Elasticsearch until a document with the given key appears."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            resp = http_requests.get(
                f"{ELASTICSEARCH_URL}/{index}/_search",
                params={"q": f"key:{key}"},
                timeout=5,
            )
            if resp.status_code == 200:
                hits = resp.json().get("hits", {}).get("hits", [])
                if hits:
                    return hits
        except Exception:
            pass
        time.sleep(1)
    return []


def cleanup_es_index(index):
    """Delete an ES index (best effort, for test cleanup)."""
    try:
        http_requests.delete(f"{ELASTICSEARCH_URL}/{index}", timeout=5)
    except Exception:
        pass


class TestElasticsearchDelivery:
    """Tests for Elasticsearch event delivery."""

    def test_put_object_indexes_document(self, s3_client, unique_bucket):
        """PutObject should index an event document in Elasticsearch."""
        test_index = f"test-{unique_bucket}"
        put_notification_config_xml(unique_bucket, ELASTICSEARCH_URL, index=test_index)
        s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")

        hits = wait_for_es_doc(test_index, "hello.txt")
        assert len(hits) >= 1
        doc = hits[0]["_source"]
        assert doc["event_name"] == "s3:ObjectCreated:Put"
        assert doc["bucket"] == unique_bucket
        assert doc["key"] == "hello.txt"
        assert "payload" in doc

        cleanup_es_index(test_index)

    def test_default_index(self, s3_client, unique_bucket):
        """Without explicit index, events should go to the default index."""
        put_notification_config_xml(unique_bucket, ELASTICSEARCH_URL)
        s3_client.put_object(Bucket=unique_bucket, Key=f"default-{unique_bucket}.txt", Body=b"data")

        hits = wait_for_es_doc(DEFAULT_INDEX, f"default-{unique_bucket}.txt")
        assert len(hits) >= 1

    def test_delete_object_indexes_document(self, s3_client, unique_bucket):
        """DeleteObject should index an ObjectRemoved event."""
        test_index = f"test-del-{unique_bucket}"
        put_notification_config_xml(
            unique_bucket, ELASTICSEARCH_URL, index=test_index,
            events=["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
        )
        s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
        time.sleep(3)
        s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")

        # Wait for the remove event
        deadline = time.time() + 15
        remove_hits = []
        while time.time() < deadline and not remove_hits:
            try:
                resp = http_requests.get(
                    f"{ELASTICSEARCH_URL}/{test_index}/_search",
                    params={"q": "event_name:*ObjectRemoved*"},
                    timeout=5,
                )
                if resp.status_code == 200:
                    remove_hits = resp.json().get("hits", {}).get("hits", [])
            except Exception:
                pass
            if not remove_hits:
                time.sleep(1)

        assert len(remove_hits) >= 1

        cleanup_es_index(test_index)

    def test_multiple_events(self, s3_client, unique_bucket):
        """Multiple S3 operations should produce multiple ES documents."""
        test_index = f"test-multi-{unique_bucket}"
        put_notification_config_xml(unique_bucket, ELASTICSEARCH_URL, index=test_index)

        for i in range(3):
            s3_client.put_object(Bucket=unique_bucket, Key=f"multi-{i}.txt", Body=f"data-{i}".encode())

        # Wait for all 3 documents
        deadline = time.time() + 20
        count = 0
        while time.time() < deadline and count < 3:
            try:
                resp = http_requests.get(
                    f"{ELASTICSEARCH_URL}/{test_index}/_search",
                    params={"q": f"bucket:{unique_bucket}", "size": 10},
                    timeout=5,
                )
                if resp.status_code == 200:
                    count = resp.json().get("hits", {}).get("total", {}).get("value", 0)
            except Exception:
                pass
            if count < 3:
                time.sleep(1)

        assert count >= 3

        cleanup_es_index(test_index)

    def test_event_payload_format(self, s3_client, unique_bucket):
        """Verify the indexed document has the expected fields."""
        test_index = f"test-fmt-{unique_bucket}"
        put_notification_config_xml(unique_bucket, ELASTICSEARCH_URL, index=test_index)
        s3_client.put_object(Bucket=unique_bucket, Key="format.txt", Body=b"test data")

        hits = wait_for_es_doc(test_index, "format.txt")
        assert len(hits) >= 1
        doc = hits[0]["_source"]
        assert "id" in doc
        assert "event_name" in doc
        assert "bucket" in doc
        assert "key" in doc
        assert "event_time" in doc
        assert "payload" in doc
        assert "created_at" in doc

        # Verify payload is valid JSON containing S3 event record
        payload = json.loads(doc["payload"])
        assert "Records" in payload

        cleanup_es_index(test_index)


class TestElasticsearchConnectivity:
    """Tests for the admin API connector test endpoint."""

    def test_elasticsearch_connectivity_success(self):
        """Admin API test-connector with valid ES URL should succeed."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "elasticsearch",
            "url": ELASTICSEARCH_URL,
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is True
        assert data["connector_type"] == "elasticsearch"

    def test_elasticsearch_connectivity_failure(self):
        """Admin API test-connector with bad ES URL should fail gracefully."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "elasticsearch",
            "url": "http://nonexistent-host:9200",
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is False
        assert data["error"] is not None
