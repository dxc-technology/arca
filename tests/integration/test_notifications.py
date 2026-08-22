"""Integration tests for S3 bucket notification configuration and event delivery."""

import json
import os
import time
import uuid

import pytest
import requests


WEBHOOK_RECEIVER_URL = os.environ.get("WEBHOOK_RECEIVER_URL", "http://webhook-receiver:8765")


@pytest.fixture
def unique_bucket(s3_client):
    """Create a unique test bucket and clean up after the test."""
    name = f"notif-test-{uuid.uuid4().hex[:8]}"
    s3_client.create_bucket(Bucket=name)
    yield name
    # Cleanup: remove all objects and delete bucket
    try:
        objs = s3_client.list_objects_v2(Bucket=name).get("Contents", [])
        for obj in objs:
            s3_client.delete_object(Bucket=name, Key=obj["Key"])
        s3_client.delete_bucket(Bucket=name)
    except Exception:
        pass


@pytest.fixture(autouse=True)
def clear_webhook_events():
    """Clear all captured events on the webhook receiver before each test."""
    try:
        requests.delete(f"{WEBHOOK_RECEIVER_URL}/events", timeout=5)
    except Exception:
        pass
    yield


def get_webhook_events(timeout=5, min_count=1):
    """Poll the webhook receiver until at least min_count events are captured."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            resp = requests.get(f"{WEBHOOK_RECEIVER_URL}/events", timeout=2)
            if resp.ok:
                events = resp.json()
                if len(events) >= min_count:
                    return events
        except Exception:
            pass
        time.sleep(0.3)
    # Return whatever we have (may be less than min_count)
    try:
        resp = requests.get(f"{WEBHOOK_RECEIVER_URL}/events", timeout=2)
        return resp.json() if resp.ok else []
    except Exception:
        return []


# ── Configuration tests ──

class TestNotificationConfiguration:
    def test_put_topic_configuration(self, s3_client, unique_bucket):
        """PUT TopicConfiguration and verify GET roundtrip."""
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "TopicConfigurations": [{
                    "Id": "hook1",
                    "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectCreated:*"],
                }]
            },
        )

        config = s3_client.get_bucket_notification_configuration(Bucket=unique_bucket)
        topics = config.get("TopicConfigurations", [])
        assert len(topics) == 1
        assert topics[0]["Id"] == "hook1"
        assert topics[0]["TopicArn"] == f"{WEBHOOK_RECEIVER_URL}/webhook"
        assert "s3:ObjectCreated:*" in topics[0]["Events"]

    def test_put_queue_configuration(self, s3_client, unique_bucket):
        """PUT QueueConfiguration and verify GET roundtrip."""
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "QueueConfigurations": [{
                    "Id": "q1",
                    "QueueArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectRemoved:*"],
                }]
            },
        )

        config = s3_client.get_bucket_notification_configuration(Bucket=unique_bucket)
        queues = config.get("QueueConfigurations", [])
        assert len(queues) == 1
        assert queues[0]["Id"] == "q1"

    def test_put_lambda_configuration(self, s3_client, unique_bucket):
        """PUT LambdaFunctionConfiguration and verify GET roundtrip."""
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "LambdaFunctionConfigurations": [{
                    "Id": "cf1",
                    "LambdaFunctionArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectCreated:Put"],
                }]
            },
        )

        config = s3_client.get_bucket_notification_configuration(Bucket=unique_bucket)
        lambdas = config.get("LambdaFunctionConfigurations", [])
        assert len(lambdas) == 1
        assert lambdas[0]["Id"] == "cf1"

    def test_put_multiple_configurations(self, s3_client, unique_bucket):
        """PUT multiple configurations across different types."""
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "TopicConfigurations": [{
                    "Id": "t1",
                    "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectCreated:Put"],
                }],
                "QueueConfigurations": [{
                    "Id": "q1",
                    "QueueArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectRemoved:Delete"],
                }],
            },
        )

        config = s3_client.get_bucket_notification_configuration(Bucket=unique_bucket)
        assert len(config.get("TopicConfigurations", [])) == 1
        assert len(config.get("QueueConfigurations", [])) == 1

    def test_put_with_prefix_filter(self, s3_client, unique_bucket):
        """PUT with key prefix filter."""
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "TopicConfigurations": [{
                    "Id": "filtered",
                    "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectCreated:*"],
                    "Filter": {
                        "Key": {
                            "FilterRules": [
                                {"Name": "prefix", "Value": "images/"},
                            ]
                        }
                    },
                }]
            },
        )

        config = s3_client.get_bucket_notification_configuration(Bucket=unique_bucket)
        topics = config.get("TopicConfigurations", [])
        assert len(topics) == 1
        rules = topics[0].get("Filter", {}).get("Key", {}).get("FilterRules", [])
        assert any(r["Name"] == "prefix" and r["Value"] == "images/" for r in rules)

    def test_put_with_suffix_filter(self, s3_client, unique_bucket):
        """PUT with key suffix filter."""
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "TopicConfigurations": [{
                    "Id": "suffix-hook",
                    "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectCreated:*"],
                    "Filter": {
                        "Key": {
                            "FilterRules": [
                                {"Name": "suffix", "Value": ".jpg"},
                            ]
                        }
                    },
                }]
            },
        )

        config = s3_client.get_bucket_notification_configuration(Bucket=unique_bucket)
        rules = config["TopicConfigurations"][0]["Filter"]["Key"]["FilterRules"]
        assert any(r["Name"] == "suffix" and r["Value"] == ".jpg" for r in rules)

    def test_put_empty_removes_config(self, s3_client, unique_bucket):
        """PUT empty configuration removes existing config."""
        # First, set a config
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "TopicConfigurations": [{
                    "Id": "temp",
                    "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectCreated:*"],
                }]
            },
        )
        # Then, clear it
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={},
        )

        config = s3_client.get_bucket_notification_configuration(Bucket=unique_bucket)
        assert len(config.get("TopicConfigurations", [])) == 0
        assert len(config.get("QueueConfigurations", [])) == 0
        assert len(config.get("LambdaFunctionConfigurations", [])) == 0

    def test_get_empty_returns_empty(self, s3_client, unique_bucket):
        """GET on a bucket with no notification config returns empty."""
        config = s3_client.get_bucket_notification_configuration(Bucket=unique_bucket)
        assert len(config.get("TopicConfigurations", [])) == 0
        assert len(config.get("QueueConfigurations", [])) == 0

    def test_put_nonexistent_bucket(self, s3_client):
        """PUT on a non-existent bucket returns NoSuchBucket."""
        with pytest.raises(Exception) as exc_info:
            s3_client.put_bucket_notification_configuration(
                Bucket="no-such-bucket-ever",
                NotificationConfiguration={
                    "TopicConfigurations": [{
                        "Id": "hook1",
                        "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                        "Events": ["s3:ObjectCreated:*"],
                    }]
                },
            )
        assert "NoSuchBucket" in str(exc_info.value) or "404" in str(exc_info.value)


# ── Webhook delivery tests ──

class TestNotificationDelivery:
    def _configure_webhook(self, s3_client, bucket, events=None, prefix=None, suffix=None):
        """Helper to configure a webhook on a bucket."""
        config = {
            "Id": "test-hook",
            "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
            "Events": events or ["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
        }
        if prefix or suffix:
            rules = []
            if prefix:
                rules.append({"Name": "prefix", "Value": prefix})
            if suffix:
                rules.append({"Name": "suffix", "Value": suffix})
            config["Filter"] = {"Key": {"FilterRules": rules}}

        s3_client.put_bucket_notification_configuration(
            Bucket=bucket,
            NotificationConfiguration={"TopicConfigurations": [config]},
        )

    def test_put_object_triggers_webhook(self, s3_client, unique_bucket):
        """PUT object should trigger a webhook with s3:ObjectCreated:Put event."""
        self._configure_webhook(s3_client, unique_bucket)

        s3_client.put_object(Bucket=unique_bucket, Key="test.txt", Body=b"hello")

        events = get_webhook_events(timeout=10, min_count=1)
        assert len(events) >= 1
        payload = events[0]["payload"]
        records = payload.get("Records", [])
        assert len(records) >= 1
        assert records[0]["eventName"] == "s3:ObjectCreated:Put"
        assert records[0]["s3"]["bucket"]["name"] == unique_bucket
        assert records[0]["s3"]["object"]["key"] == "test.txt"

    def test_delete_object_triggers_webhook(self, s3_client, unique_bucket):
        """DELETE object should trigger a webhook with s3:ObjectRemoved:Delete event."""
        self._configure_webhook(s3_client, unique_bucket)

        # Create then delete
        s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
        time.sleep(1)  # Let the create event be processed
        requests.delete(f"{WEBHOOK_RECEIVER_URL}/events", timeout=5)  # Clear

        s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")

        events = get_webhook_events(timeout=10, min_count=1)
        assert len(events) >= 1
        payload = events[0]["payload"]
        records = payload.get("Records", [])
        assert len(records) >= 1
        assert "ObjectRemoved" in records[0]["eventName"]

    def test_copy_object_triggers_webhook(self, s3_client, unique_bucket):
        """COPY object should trigger s3:ObjectCreated:Copy event."""
        self._configure_webhook(s3_client, unique_bucket)

        s3_client.put_object(Bucket=unique_bucket, Key="source.txt", Body=b"data")
        time.sleep(1)
        requests.delete(f"{WEBHOOK_RECEIVER_URL}/events", timeout=5)  # Clear

        s3_client.copy_object(
            Bucket=unique_bucket,
            Key="copy.txt",
            CopySource=f"{unique_bucket}/source.txt",
        )

        events = get_webhook_events(timeout=10, min_count=1)
        assert len(events) >= 1
        payload = events[0]["payload"]
        records = payload.get("Records", [])
        assert len(records) >= 1
        assert records[0]["eventName"] == "s3:ObjectCreated:Copy"

    def test_event_payload_format(self, s3_client, unique_bucket):
        """Verify the event payload matches the S3 event record format."""
        self._configure_webhook(s3_client, unique_bucket)

        s3_client.put_object(Bucket=unique_bucket, Key="format-test.txt", Body=b"test data")

        events = get_webhook_events(timeout=10, min_count=1)
        assert len(events) >= 1
        payload = events[0]["payload"]

        # Validate top-level structure
        assert "Records" in payload
        record = payload["Records"][0]

        # Validate required fields
        assert record["eventVersion"] == "2.1"
        assert record["eventSource"] == "arca:s3"
        assert "eventTime" in record
        assert record["eventName"] == "s3:ObjectCreated:Put"

        # Validate s3 section
        s3 = record["s3"]
        assert s3["s3SchemaVersion"] == "1.0"
        assert s3["bucket"]["name"] == unique_bucket
        assert s3["object"]["key"] == "format-test.txt"
        assert s3["object"]["size"] > 0
        assert "configurationId" in s3

    def test_filter_prefix_match(self, s3_client, unique_bucket):
        """Only objects matching the prefix filter should trigger the webhook."""
        self._configure_webhook(s3_client, unique_bucket, prefix="images/")

        s3_client.put_object(Bucket=unique_bucket, Key="images/photo.jpg", Body=b"img")

        events = get_webhook_events(timeout=10, min_count=1)
        assert len(events) >= 1
        assert events[0]["payload"]["Records"][0]["s3"]["object"]["key"] == "images/photo.jpg"

    def test_filter_prefix_no_match(self, s3_client, unique_bucket):
        """Objects NOT matching the prefix filter should NOT trigger the webhook."""
        self._configure_webhook(s3_client, unique_bucket, prefix="images/")

        s3_client.put_object(Bucket=unique_bucket, Key="docs/readme.md", Body=b"doc")

        # Wait briefly and verify no events
        events = get_webhook_events(timeout=3, min_count=1)
        # Should have 0 events (the prefix didn't match)
        matching = [e for e in events if e["payload"].get("Records", [{}])[0].get("s3", {}).get("object", {}).get("key") == "docs/readme.md"]
        assert len(matching) == 0

    def test_batch_delete_triggers_webhooks(self, s3_client, unique_bucket):
        """DeleteObjects (batch) should trigger a webhook for each deleted object."""
        self._configure_webhook(s3_client, unique_bucket)

        # Create objects
        for i in range(3):
            s3_client.put_object(Bucket=unique_bucket, Key=f"batch-{i}.txt", Body=b"x")
        time.sleep(1)
        requests.delete(f"{WEBHOOK_RECEIVER_URL}/events", timeout=5)  # Clear

        # Batch delete
        s3_client.delete_objects(
            Bucket=unique_bucket,
            Delete={"Objects": [{"Key": f"batch-{i}.txt"} for i in range(3)]},
        )

        events = get_webhook_events(timeout=10, min_count=3)
        assert len(events) >= 3
        event_keys = set()
        for e in events:
            for r in e["payload"].get("Records", []):
                event_keys.add(r["s3"]["object"]["key"])
        for i in range(3):
            assert f"batch-{i}.txt" in event_keys


# ── Admin API tests ──

class TestNotificationAdminApi:
    def test_event_log_admin_api(self, s3_client, unique_bucket, endpoint_url):
        """Verify notification events are visible via the admin API."""
        # Configure webhook and trigger an event
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "TopicConfigurations": [{
                    "Id": "admin-test",
                    "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectCreated:*"],
                }]
            },
        )
        s3_client.put_object(Bucket=unique_bucket, Key="admin-test.txt", Body=b"hello")

        # Wait for delivery
        time.sleep(2)

        # Query admin API
        import hmac
        import hashlib
        import datetime

        # Use direct HTTP with admin credentials
        admin_url = f"{endpoint_url}/admin/notifications/events?bucket={unique_bucket}&limit=10"
        access_key = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
        secret_key = os.environ.get("AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv")

        # Use boto3 session to sign the request (SigV4)
        import botocore.auth
        import botocore.credentials
        from botocore.awsrequest import AWSRequest

        credentials = botocore.credentials.Credentials(access_key, secret_key)
        request = AWSRequest(method="GET", url=admin_url)
        # S3SigV4Auth, not the generic SigV4Auth: the S3 flavour sends the
        # x-amz-content-sha256 header it signed with. The generic one signs
        # the real payload hash over http but never sends the header, and the
        # server (which then assumes UNSIGNED-PAYLOAD) correctly answers 403.
        botocore.auth.S3SigV4Auth(credentials, "s3", "us-east-1").add_auth(request)

        resp = requests.get(admin_url, headers=dict(request.headers), timeout=10)
        assert resp.status_code == 200
        data = resp.json()
        assert "entries" in data
        assert data["total"] >= 1
        # Verify at least one entry for our bucket
        buckets = [e["bucket"] for e in data["entries"]]
        assert unique_bucket in buckets


# ── Connector architecture tests ──

class TestConnectorArchitecture:
    def test_webhook_auth_token_delivered(self, s3_client, unique_bucket):
        """Webhook with auth_token sends Authorization: Bearer header."""
        # Configure webhook with auth_token via Arca extension XML elements
        xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <TopicConfiguration>
    <Id>auth-test</Id>
    <Topic>{WEBHOOK_RECEIVER_URL}/webhook</Topic>
    <Event>s3:ObjectCreated:*</Event>
    <Property><Name>auth_token</Name><Value>test-secret-token-123</Value></Property>
  </TopicConfiguration>
</NotificationConfiguration>"""

        import hashlib
        import botocore.auth
        import botocore.credentials
        from botocore.awsrequest import AWSRequest
        endpoint = os.environ.get("S3_ENDPOINT", "http://arca:9000")
        url = f"{endpoint}/{unique_bucket}?notification"
        access_key = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
        secret_key = os.environ.get("AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv")
        credentials = botocore.credentials.Credentials(access_key, secret_key)
        content_sha = hashlib.sha256(xml.encode()).hexdigest()
        aws_req = AWSRequest(method="PUT", url=url, data=xml, headers={
            "Content-Type": "application/xml",
            "x-amz-content-sha256": content_sha,
        })
        botocore.auth.SigV4Auth(credentials, "s3", "us-east-1").add_auth(aws_req)
        resp = requests.put(url, data=xml, headers=dict(aws_req.headers), timeout=10)

        assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"

        # Trigger event
        s3_client.put_object(Bucket=unique_bucket, Key="auth-test.txt", Body=b"hello")

        # Check webhook receiver captured the Authorization header
        events = get_webhook_events(timeout=10, min_count=1)
        assert len(events) >= 1
        auth_header = events[0].get("authorization", "")
        assert auth_header == "Bearer test-secret-token-123", f"Expected Bearer token, got: {auth_header}"

    def test_connector_type_defaults_to_webhook(self, s3_client, unique_bucket):
        """Configs without ConnectorType default to webhook and still work."""
        # Standard S3 notification config (no ConnectorType element)
        s3_client.put_bucket_notification_configuration(
            Bucket=unique_bucket,
            NotificationConfiguration={
                "TopicConfigurations": [{
                    "Id": "default-connector",
                    "TopicArn": f"{WEBHOOK_RECEIVER_URL}/webhook",
                    "Events": ["s3:ObjectCreated:*"],
                }]
            },
        )

        # Trigger event
        s3_client.put_object(Bucket=unique_bucket, Key="default.txt", Body=b"test")

        # Verify delivery worked (connector defaulted to webhook)
        events = get_webhook_events(timeout=10, min_count=1)
        assert len(events) >= 1
        records = events[0]["payload"].get("Records", [])
        assert len(records) >= 1
        assert records[0]["eventName"] == "s3:ObjectCreated:Put"

    def test_connector_type_roundtrip_xml(self, s3_client, unique_bucket):
        """ConnectorType and Property elements survive PUT/GET roundtrip."""
        xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <TopicConfiguration>
    <Id>roundtrip-test</Id>
    <Topic>{WEBHOOK_RECEIVER_URL}/webhook</Topic>
    <Event>s3:ObjectCreated:*</Event>
    <Property><Name>auth_token</Name><Value>my-secret</Value></Property>
  </TopicConfiguration>
</NotificationConfiguration>"""

        # PUT with SigV4 (include content hash for S3 auth)
        import hashlib
        import botocore.auth
        import botocore.credentials
        from botocore.awsrequest import AWSRequest
        endpoint = os.environ.get("S3_ENDPOINT", "http://arca:9000")
        url = f"{endpoint}/{unique_bucket}?notification"
        access_key = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
        secret_key = os.environ.get("AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv")
        credentials = botocore.credentials.Credentials(access_key, secret_key)
        content_sha = hashlib.sha256(xml.encode()).hexdigest()
        aws_req = AWSRequest(method="PUT", url=url, data=xml, headers={
            "Content-Type": "application/xml",
            "x-amz-content-sha256": content_sha,
        })
        botocore.auth.SigV4Auth(credentials, "s3", "us-east-1").add_auth(aws_req)
        resp = requests.put(url, data=xml, headers=dict(aws_req.headers), timeout=10)
        assert resp.status_code in (200, 204), f"PUT failed: {resp.status_code}"

        # GET the config back
        aws_req = AWSRequest(method="GET", url=url, headers={
            "x-amz-content-sha256": hashlib.sha256(b"").hexdigest(),
        })
        botocore.auth.SigV4Auth(credentials, "s3", "us-east-1").add_auth(aws_req)
        resp = requests.get(url, headers=dict(aws_req.headers), timeout=10)
        assert resp.status_code == 200

        # Verify Property element is present in response XML
        response_xml = resp.text
        assert "auth_token" in response_xml
        assert "my-secret" in response_xml
