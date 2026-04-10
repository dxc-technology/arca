"""Integration tests for the MongoDB notification connector."""

import json
import os
import time

import pymongo
import pytest

from conftest import sigv4_request


MONGODB_URL = os.environ.get("MONGODB_URL", "mongodb://mongodb-receiver:27017")
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")

DEFAULT_DATABASE = "arca"
DEFAULT_COLLECTION = "arca_notifications"


def _mongo_client():
    """Create a pymongo client to the MongoDB receiver."""
    return pymongo.MongoClient(MONGODB_URL)


def _wait_for_document(database, collection, key, timeout=10):
    """Poll the collection until a document with the given object key appears."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        client = _mongo_client()
        try:
            coll = client[database][collection]
            doc = coll.find_one({"key": key})
            if doc:
                return json.loads(doc["payload"])
        finally:
            client.close()
        time.sleep(0.5)
    return None


def put_notification_config_xml(bucket, mongo_url, database=None, collection=None, events=None):
    """Configure a bucket notification with MongoDB connector via raw XML."""
    event_elements = ""
    for evt in (events or ["s3:ObjectCreated:*"]):
        event_elements += f"    <Event>{evt}</Event>\n"

    properties = ""
    if database:
        properties += f'    <Property><Name>database</Name><Value>{database}</Value></Property>\n'
    if collection:
        properties += f'    <Property><Name>collection</Name><Value>{collection}</Value></Property>\n'

    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <TopicConfiguration>
    <Id>mongo-test</Id>
    <Topic>{mongo_url}</Topic>
{event_elements}    <ConnectorType>mongodb</ConnectorType>
{properties}  </TopicConfiguration>
</NotificationConfiguration>"""

    url = f"{ARCA_ENDPOINT}/{bucket}?notification"
    resp = sigv4_request("PUT", url, data=xml, headers={"Content-Type": "application/xml"})
    assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"


class TestMongodbDelivery:
    """Tests for MongoDB event delivery."""

    def test_put_object_inserts_document(self, s3_client, unique_bucket):
        """PutObject should insert a document into the default collection."""
        put_notification_config_xml(unique_bucket, MONGODB_URL)
        s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")

        payload = _wait_for_document(DEFAULT_DATABASE, DEFAULT_COLLECTION, "hello.txt")
        assert payload is not None
        records = payload.get("Records", [])
        assert len(records) >= 1
        assert records[0]["eventName"] == "s3:ObjectCreated:Put"
        assert records[0]["s3"]["bucket"]["name"] == unique_bucket

    def test_custom_collection(self, s3_client, unique_bucket):
        """Events should be inserted into the collection specified in properties."""
        custom_coll = "custom_events"
        put_notification_config_xml(
            unique_bucket, MONGODB_URL,
            database=DEFAULT_DATABASE, collection=custom_coll,
        )
        s3_client.put_object(Bucket=unique_bucket, Key="custom.txt", Body=b"data")

        payload = _wait_for_document(DEFAULT_DATABASE, custom_coll, "custom.txt")
        assert payload is not None

    def test_delete_object_inserts_document(self, s3_client, unique_bucket):
        """DeleteObject should insert an s3:ObjectRemoved event."""
        put_notification_config_xml(
            unique_bucket, MONGODB_URL,
            events=["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
        )
        s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
        time.sleep(1)
        s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")

        deadline = time.time() + 10
        while time.time() < deadline:
            client = _mongo_client()
            try:
                coll = client[DEFAULT_DATABASE][DEFAULT_COLLECTION]
                doc = coll.find_one({
                    "key": "to-delete.txt",
                    "event_name": {"$regex": "ObjectRemoved"},
                })
                if doc:
                    payload = json.loads(doc["payload"])
                    assert "ObjectRemoved" in payload["Records"][0]["eventName"]
                    return
            finally:
                client.close()
            time.sleep(0.5)
        pytest.fail("Delete event not found in MongoDB")

    def test_event_payload_format(self, s3_client, unique_bucket):
        """Verify the inserted payload matches the S3 event record format."""
        put_notification_config_xml(unique_bucket, MONGODB_URL)
        s3_client.put_object(Bucket=unique_bucket, Key="format.txt", Body=b"test data")

        payload = _wait_for_document(DEFAULT_DATABASE, DEFAULT_COLLECTION, "format.txt")
        assert payload is not None
        record = payload["Records"][0]
        assert record["eventVersion"] == "2.1"
        assert record["eventSource"] == "arca:s3"
        assert "eventTime" in record
        assert record["s3"]["object"]["key"] == "format.txt"

    def test_auto_creates_collection(self, s3_client, unique_bucket):
        """MongoDB auto-creates collections on first insert."""
        auto_coll = f"auto_{unique_bucket.replace('-', '_')}"
        put_notification_config_xml(
            unique_bucket, MONGODB_URL,
            database=DEFAULT_DATABASE, collection=auto_coll,
        )
        s3_client.put_object(Bucket=unique_bucket, Key="auto.txt", Body=b"data")

        payload = _wait_for_document(DEFAULT_DATABASE, auto_coll, "auto.txt")
        assert payload is not None


class TestMongodbConnectivity:
    """Tests for the admin API connector test endpoint."""

    def test_mongodb_connectivity_success(self):
        """Admin API test-connector with valid MongoDB URL should succeed."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "mongodb",
            "url": MONGODB_URL,
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is True
        assert data["connector_type"] == "mongodb"

    def test_mongodb_connectivity_failure(self):
        """Admin API test-connector with bad MongoDB URL should fail gracefully."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "mongodb",
            "url": "mongodb://nonexistent-host:27017",
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is False
        assert data["error"] is not None
