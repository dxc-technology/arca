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


def test_post_object_returns_501(endpoint_url):
    """POST /{bucket}/{key} should return 501."""
    resp = requests.post(f"{endpoint_url}/test-bucket/test-key")
    assert resp.status_code == 501
