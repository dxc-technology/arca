"""Smoke tests for Arca Phase 0.

Verifies that the server returns valid S3 XML error responses (501 NotImplemented)
for all endpoints.
"""

import xml.etree.ElementTree as ET

import requests


def test_root_returns_501_with_s3_xml_error(endpoint_url):
    """GET / should return 501 with a valid S3 XML error."""
    resp = requests.get(endpoint_url)
    assert resp.status_code == 501
    assert "application/xml" in resp.headers.get("Content-Type", "")

    root = ET.fromstring(resp.text)
    assert root.tag == "Error"
    assert root.find("Code").text == "NotImplemented"
    assert root.find("Message").text is not None
    assert root.find("Resource").text == "/"
    assert root.find("RequestId").text is not None


def test_bucket_endpoint_returns_501(endpoint_url):
    """GET /{bucket} should return 501 with S3 XML error."""
    resp = requests.get(f"{endpoint_url}/my-bucket")
    assert resp.status_code == 501

    root = ET.fromstring(resp.text)
    assert root.find("Code").text == "NotImplemented"
    assert root.find("Resource").text == "/my-bucket"


def test_object_endpoint_returns_501(endpoint_url):
    """GET /{bucket}/{key} should return 501 with S3 XML error."""
    resp = requests.get(f"{endpoint_url}/my-bucket/my-key")
    assert resp.status_code == 501

    root = ET.fromstring(resp.text)
    assert root.find("Code").text == "NotImplemented"


def test_put_bucket_returns_501(endpoint_url):
    """PUT /{bucket} should return 501."""
    resp = requests.put(f"{endpoint_url}/test-bucket")
    assert resp.status_code == 501


def test_put_object_returns_501(endpoint_url):
    """PUT /{bucket}/{key} should return 501."""
    resp = requests.put(f"{endpoint_url}/test-bucket/test-key", data=b"hello")
    assert resp.status_code == 501


def test_delete_bucket_returns_501(endpoint_url):
    """DELETE /{bucket} should return 501."""
    resp = requests.delete(f"{endpoint_url}/test-bucket")
    assert resp.status_code == 501


def test_delete_object_returns_501(endpoint_url):
    """DELETE /{bucket}/{key} should return 501."""
    resp = requests.delete(f"{endpoint_url}/test-bucket/test-key")
    assert resp.status_code == 501


def test_head_bucket_returns_501(endpoint_url):
    """HEAD /{bucket} should return 501."""
    resp = requests.head(f"{endpoint_url}/test-bucket")
    assert resp.status_code == 501


def test_head_object_returns_501(endpoint_url):
    """HEAD /{bucket}/{key} should return 501."""
    resp = requests.head(f"{endpoint_url}/test-bucket/test-key")
    assert resp.status_code == 501


def test_post_object_returns_501(endpoint_url):
    """POST /{bucket}/{key} should return 501."""
    resp = requests.post(f"{endpoint_url}/test-bucket/test-key")
    assert resp.status_code == 501
