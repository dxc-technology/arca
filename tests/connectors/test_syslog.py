"""Integration tests for the Syslog (RFC 5424) notification connector."""

import json
import os
import time

import pytest

from conftest import sigv4_request


SYSLOG_UDP_URL = os.environ.get("SYSLOG_UDP_URL", "udp://syslog-receiver:514")
SYSLOG_TCP_URL = os.environ.get("SYSLOG_TCP_URL", "tcp://syslog-receiver:1514")
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")


def put_notification_config_xml(bucket, syslog_url, events=None, properties=None):
    """Configure a bucket notification with Syslog connector via raw XML."""
    event_elements = ""
    for evt in (events or ["s3:ObjectCreated:*"]):
        event_elements += f"    <Event>{evt}</Event>\n"

    prop_elements = ""
    for name, value in (properties or {}).items():
        prop_elements += f"    <Property><Name>{name}</Name><Value>{value}</Value></Property>\n"

    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <CloudFunctionConfiguration>
    <Id>syslog-test</Id>
    <CloudFunction>{syslog_url}</CloudFunction>
{event_elements}    <ConnectorType>syslog</ConnectorType>
{prop_elements}  </CloudFunctionConfiguration>
</NotificationConfiguration>"""

    url = f"{ARCA_ENDPOINT}/{bucket}?notification"
    resp = sigv4_request("PUT", url, data=xml, headers={"Content-Type": "application/xml"})
    assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"


def get_delivered_events(bucket, limit=20):
    """Fetch delivered notification events for a bucket from the admin API."""
    url = f"{ARCA_ENDPOINT}/admin/notifications/events?bucket={bucket}&limit={limit}"
    resp = sigv4_request("GET", url)
    assert resp.status_code == 200
    entries = resp.json().get("entries", [])
    return [e for e in entries if e.get("delivery_status") == "delivered"]


class TestSyslogDelivery:
    """Tests for Syslog event delivery."""

    def test_put_object_sends_syslog_udp(self, s3_client, unique_bucket):
        """PutObject should send a syslog message via UDP."""
        put_notification_config_xml(unique_bucket, SYSLOG_UDP_URL)
        s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")
        time.sleep(5)

        delivered = get_delivered_events(unique_bucket)
        assert len(delivered) >= 1

    def test_put_object_sends_syslog_tcp(self, s3_client, unique_bucket):
        """PutObject should send a syslog message via TCP."""
        put_notification_config_xml(unique_bucket, SYSLOG_TCP_URL)
        s3_client.put_object(Bucket=unique_bucket, Key="tcp-test.txt", Body=b"data")
        time.sleep(5)

        delivered = get_delivered_events(unique_bucket)
        assert len(delivered) >= 1

    def test_custom_facility_and_severity(self, s3_client, unique_bucket):
        """Custom facility and severity should be accepted."""
        put_notification_config_xml(
            unique_bucket, SYSLOG_UDP_URL,
            properties={"facility": "daemon", "severity": "warning"},
        )
        s3_client.put_object(Bucket=unique_bucket, Key="custom.txt", Body=b"data")
        time.sleep(5)

        delivered = get_delivered_events(unique_bucket)
        assert len(delivered) >= 1

    def test_delete_object_sends_syslog(self, s3_client, unique_bucket):
        """DeleteObject should send a syslog message."""
        put_notification_config_xml(
            unique_bucket, SYSLOG_UDP_URL,
            events=["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
        )
        s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
        time.sleep(3)
        s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")
        time.sleep(5)

        delivered = get_delivered_events(unique_bucket)
        remove_events = [e for e in delivered if "ObjectRemoved" in e.get("event_name", "")]
        assert len(remove_events) >= 1

    def test_multiple_events(self, s3_client, unique_bucket):
        """Multiple S3 operations should produce multiple syslog messages."""
        put_notification_config_xml(unique_bucket, SYSLOG_UDP_URL)
        for i in range(3):
            s3_client.put_object(Bucket=unique_bucket, Key=f"multi-{i}.txt", Body=f"data-{i}".encode())
        time.sleep(5)

        delivered = get_delivered_events(unique_bucket)
        assert len(delivered) >= 3


class TestSyslogConnectivity:
    """Tests for the admin API connector test endpoint."""

    def test_syslog_connectivity_success_udp(self):
        """Admin API test-connector with valid syslog UDP URL should succeed."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "syslog",
            "url": SYSLOG_UDP_URL,
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is True
        assert data["connector_type"] == "syslog"

    def test_syslog_connectivity_failure(self):
        """Admin API test-connector with unreachable TCP syslog should fail."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "syslog",
            "url": "tcp://nonexistent-host:514",
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is False
        assert data["error"] is not None
