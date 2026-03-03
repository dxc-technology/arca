"""Integration tests for Phase 1: Credential bootstrap.

Verifies that the server starts with a SQLite database, auto-generates
root credentials, and still returns valid S3 XML responses.

With Phase 6 auth enabled, unauthenticated requests receive 403 AccessDenied.
"""

import xml.etree.ElementTree as ET

import requests


def test_server_responds_after_credential_bootstrap(endpoint_url):
    """Server should reject unauthenticated requests with 403 AccessDenied."""
    resp = requests.get(endpoint_url)
    assert resp.status_code == 403


def test_unauthenticated_returns_valid_s3_xml(endpoint_url):
    """Unauthenticated request should return valid S3 XML error."""
    resp = requests.get(endpoint_url)
    assert resp.status_code == 403
    assert "application/xml" in resp.headers.get("Content-Type", "")

    root = ET.fromstring(resp.text)
    assert root.tag == "Error"
    assert root.find("Code").text == "AccessDenied"
    assert root.find("Message").text is not None
    assert root.find("RequestId").text is not None
