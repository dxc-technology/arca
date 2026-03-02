"""Smoke tests for Arca.

Verifies that unimplemented endpoints return valid S3 XML error responses
(501 NotImplemented). As operations are implemented in later phases, the
corresponding 501 tests are removed from here.
"""


def test_post_object_returns_501(endpoint_url):
    """POST /{bucket}/{key} should return 501."""
    import requests

    resp = requests.post(f"{endpoint_url}/test-bucket/test-key")
    assert resp.status_code == 501
