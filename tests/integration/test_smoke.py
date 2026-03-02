"""Smoke tests for Arca.

Verifies that unimplemented endpoints return valid S3 XML error responses
(501 NotImplemented). As operations are implemented in later phases, the
corresponding 501 tests are removed from here.
"""

import xml.etree.ElementTree as ET

import requests


def test_bucket_get_returns_501(endpoint_url):
    """GET /{bucket} (ListObjectsV2) should return 501 — not implemented until Phase 4."""
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


def test_put_object_returns_501(endpoint_url):
    """PUT /{bucket}/{key} should return 501."""
    resp = requests.put(f"{endpoint_url}/test-bucket/test-key", data=b"hello")
    assert resp.status_code == 501


def test_delete_object_returns_501(endpoint_url):
    """DELETE /{bucket}/{key} should return 501."""
    resp = requests.delete(f"{endpoint_url}/test-bucket/test-key")
    assert resp.status_code == 501


def test_head_object_returns_501(endpoint_url):
    """HEAD /{bucket}/{key} should return 501."""
    resp = requests.head(f"{endpoint_url}/test-bucket/test-key")
    assert resp.status_code == 501


def test_post_object_returns_501(endpoint_url):
    """POST /{bucket}/{key} should return 501."""
    resp = requests.post(f"{endpoint_url}/test-bucket/test-key")
    assert resp.status_code == 501
