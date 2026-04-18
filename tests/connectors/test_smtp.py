"""Integration tests for the SMTP notification connector.

These tests use Mailpit (https://mailpit.axllent.org/) as the SMTP sink. Mailpit
accepts any sender/recipient and exposes a JSON API for asserting delivery.
"""

import json
import os
import time

import pytest
import requests

from conftest import sigv4_request


SMTP_URL = os.environ.get("SMTP_URL", "smtp://smtp-receiver:1025")
SMTP_ADMIN_URL = os.environ.get("SMTP_ADMIN_URL", "http://smtp-receiver:8025")
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")


def put_notification_config_xml(bucket, smtp_url, events=None, properties=None):
    """Install a bucket notification using the SMTP connector."""
    event_elements = ""
    for evt in (events or ["s3:ObjectCreated:*"]):
        event_elements += f"    <Event>{evt}</Event>\n"

    prop_elements = ""
    for name, value in (properties or {}).items():
        prop_elements += f"    <Property><Name>{name}</Name><Value>{value}</Value></Property>\n"

    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <CloudFunctionConfiguration>
    <Id>smtp-test</Id>
    <CloudFunction>{smtp_url}</CloudFunction>
{event_elements}    <ConnectorType>smtp</ConnectorType>
{prop_elements}  </CloudFunctionConfiguration>
</NotificationConfiguration>"""

    url = f"{ARCA_ENDPOINT}/{bucket}?notification"
    resp = sigv4_request("PUT", url, data=xml, headers={"Content-Type": "application/xml"})
    assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"


def clear_mailpit():
    """Drop all messages from the Mailpit inbox (best-effort)."""
    try:
        requests.delete(f"{SMTP_ADMIN_URL}/api/v1/messages", timeout=5)
    except requests.RequestException:
        pass


def fetch_mailpit_messages():
    """Return the list of Mailpit messages (most-recent first)."""
    resp = requests.get(f"{SMTP_ADMIN_URL}/api/v1/messages", timeout=5)
    resp.raise_for_status()
    data = resp.json()
    return data.get("messages", [])


def fetch_mailpit_message_body(msg_id):
    """Return the text body of a single Mailpit message."""
    resp = requests.get(f"{SMTP_ADMIN_URL}/api/v1/message/{msg_id}", timeout=5)
    resp.raise_for_status()
    data = resp.json()
    # Mailpit exposes "Text" for text/plain parts
    return data.get("Text", "")


def wait_for_messages(min_count=1, timeout=15):
    """Poll Mailpit until at least `min_count` messages are captured."""
    deadline = time.time() + timeout
    msgs = []
    while time.time() < deadline:
        msgs = fetch_mailpit_messages()
        if len(msgs) >= min_count:
            return msgs
        time.sleep(0.5)
    return msgs


class TestSmtpDelivery:
    """End-to-end delivery through the SMTP connector."""

    def setup_method(self):
        clear_mailpit()

    def test_put_object_sends_email(self, s3_client, unique_bucket):
        """PutObject must deliver an email to the default recipient."""
        put_notification_config_xml(
            unique_bucket, SMTP_URL,
            properties={"to": "ops@example.com"},
        )
        s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")

        msgs = wait_for_messages(min_count=1)
        assert len(msgs) >= 1
        # Subject should mention the S3 event when no custom subject is set
        subject = msgs[0].get("Subject", "")
        assert "Arca" in subject
        # Body should embed the S3 event JSON
        body = fetch_mailpit_message_body(msgs[0]["ID"])
        assert "ObjectCreated" in body
        assert "hello.txt" in body

    def test_custom_subject(self, s3_client, unique_bucket):
        """A custom `subject` property must override the default."""
        put_notification_config_xml(
            unique_bucket, SMTP_URL,
            properties={"to": "ops@example.com", "subject": "Custom Arca Alert"},
        )
        s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")

        msgs = wait_for_messages(min_count=1)
        assert any(m.get("Subject") == "Custom Arca Alert" for m in msgs)

    def test_custom_sender(self, s3_client, unique_bucket):
        """The `from` property must override the default sender."""
        put_notification_config_xml(
            unique_bucket, SMTP_URL,
            properties={"to": "ops@example.com", "from": "arca-notify@example.com"},
        )
        s3_client.put_object(Bucket=unique_bucket, Key="sender.txt", Body=b"x")

        msgs = wait_for_messages(min_count=1)
        assert len(msgs) >= 1
        sender = (msgs[0].get("From") or {}).get("Address", "")
        assert sender == "arca-notify@example.com"

    def test_delete_object_sends_email(self, s3_client, unique_bucket):
        """DeleteObject must deliver an ObjectRemoved email."""
        put_notification_config_xml(
            unique_bucket, SMTP_URL,
            events=["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
            properties={"to": "ops@example.com"},
        )
        s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
        time.sleep(2)
        s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")

        msgs = wait_for_messages(min_count=2, timeout=20)
        # At least one ObjectRemoved subject/body expected
        removed = [
            m for m in msgs
            if "Removed" in m.get("Subject", "")
            or "Removed" in fetch_mailpit_message_body(m["ID"])
        ]
        assert len(removed) >= 1

    def test_multiple_events(self, s3_client, unique_bucket):
        """Multiple S3 operations produce multiple emails."""
        put_notification_config_xml(
            unique_bucket, SMTP_URL,
            properties={"to": "ops@example.com"},
        )
        for i in range(3):
            s3_client.put_object(
                Bucket=unique_bucket, Key=f"multi-{i}.txt", Body=f"data-{i}".encode()
            )

        msgs = wait_for_messages(min_count=3, timeout=20)
        assert len(msgs) >= 3


class TestSmtpConnectivity:
    """Admin API /admin/notifications/test-connector probe."""

    def test_smtp_connectivity_success(self):
        """Valid SMTP URL must succeed against the Mailpit receiver."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "smtp",
            "url": SMTP_URL,
            "properties": {},
        })
        resp = sigv4_request(
            "POST", url, data=body, headers={"Content-Type": "application/json"}
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is True, f"unexpected response: {data}"
        assert data["connector_type"] == "smtp"

    def test_smtp_connectivity_failure(self):
        """Unreachable SMTP host must fail gracefully with success=false."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "smtp",
            "url": "smtp://nonexistent-host:25",
            "properties": {},
        })
        resp = sigv4_request(
            "POST", url, data=body, headers={"Content-Type": "application/json"}
        )
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is False
        assert data["error"] is not None
