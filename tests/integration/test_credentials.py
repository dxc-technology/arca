"""Integration tests for Phase 1: Credential bootstrap.

Verifies that the server starts with a SQLite database, auto-generates
root credentials, and still returns valid S3 XML responses.
"""

import xml.etree.ElementTree as ET

import requests


def test_server_responds_after_credential_bootstrap(endpoint_url):
    """Server should respond to requests after credential bootstrap."""
    resp = requests.get(endpoint_url)
    # GET / is ListBuckets, returns 200 since Phase 2
    assert resp.status_code == 200


def test_still_returns_valid_s3_xml(endpoint_url):
    """S3 XML error format should be unchanged after Phase 1 changes."""
    resp = requests.get(f"{endpoint_url}/nonexistent-bucket-for-xml-test")
    assert resp.status_code == 404
    assert "application/xml" in resp.headers.get("Content-Type", "")

    root = ET.fromstring(resp.text)
    assert root.tag == "Error"
    assert root.find("Code").text == "NoSuchBucket"
    assert root.find("Message").text is not None
    assert root.find("Resource").text == "/nonexistent-bucket-for-xml-test"
    assert root.find("RequestId").text is not None
