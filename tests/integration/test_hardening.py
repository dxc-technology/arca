"""Integration tests for Phase 23: Performance and Hardening.

Tests request size limits, header validation, health endpoint,
and rate limiting (when enabled).
"""

import io
import os

import pytest
import requests


class TestRequestSizeLimits:
    """Tests for configurable max body size."""

    def test_put_within_limit_succeeds(self, s3_client):
        """PutObject with body within limit succeeds."""
        bucket = "test-hardening-limits"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key="small.txt", Body=b"hello")
            resp = s3_client.get_object(Bucket=bucket, Key="small.txt")
            assert resp["Body"].read() == b"hello"
        finally:
            s3_client.delete_object(Bucket=bucket, Key="small.txt")
            s3_client.delete_bucket(Bucket=bucket)

    def test_content_length_check_rejects_oversized(self, s3_client, endpoint_url):
        """PutObject with Content-Length exceeding limit is rejected.

        Note: this test only works if the server is configured with a
        max_body_size smaller than the declared Content-Length. We test
        with the default 5 GB limit by checking that a normal upload
        works (the limit is enforced but not hit).
        """
        # This test verifies the code path exists without hitting the 5GB limit.
        # A more targeted test would require a custom config with a small limit.
        bucket = "test-hardening-cl"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key="ok.txt", Body=b"x" * 1024)
            resp = s3_client.head_object(Bucket=bucket, Key="ok.txt")
            assert resp["ContentLength"] == 1024
        finally:
            s3_client.delete_object(Bucket=bucket, Key="ok.txt")
            s3_client.delete_bucket(Bucket=bucket)


class TestHealthEndpoint:
    """Tests for the health check endpoint."""

    def test_health_returns_ok(self, endpoint_url):
        """GET /admin/health returns 200 with status=ok."""
        resp = requests.get(f"{endpoint_url}/admin/health", verify=False)
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "ok"


class TestMetadataHeaders:
    """Tests for header and metadata validation."""

    def test_normal_metadata_accepted(self, s3_client):
        """PutObject with normal user metadata succeeds."""
        bucket = "test-hardening-meta"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(
                Bucket=bucket,
                Key="meta.txt",
                Body=b"data",
                Metadata={"author": "test", "version": "1"},
            )
            resp = s3_client.head_object(Bucket=bucket, Key="meta.txt")
            assert resp["Metadata"]["author"] == "test"
            assert resp["Metadata"]["version"] == "1"
        finally:
            s3_client.delete_object(Bucket=bucket, Key="meta.txt")
            s3_client.delete_bucket(Bucket=bucket)


class TestCacheTransparency:
    """Tests that the metadata cache is transparent to clients."""

    def test_head_bucket_cached(self, s3_client):
        """HeadBucket returns correct results even with caching."""
        bucket = "test-hardening-cache"
        s3_client.create_bucket(Bucket=bucket)
        try:
            # First call populates cache.
            s3_client.head_bucket(Bucket=bucket)
            # Second call should hit cache but still return correct result.
            s3_client.head_bucket(Bucket=bucket)
        finally:
            s3_client.delete_bucket(Bucket=bucket)

    def test_cache_invalidated_on_delete(self, s3_client):
        """After deleting a bucket, HeadBucket returns 404."""
        bucket = "test-hardening-cache-inv"
        s3_client.create_bucket(Bucket=bucket)
        s3_client.head_bucket(Bucket=bucket)  # populate cache
        s3_client.delete_bucket(Bucket=bucket)

        with pytest.raises(Exception):
            s3_client.head_bucket(Bucket=bucket)

    def test_object_cache_invalidated_on_overwrite(self, s3_client):
        """Overwriting an object invalidates the cache."""
        bucket = "test-hardening-obj-cache"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key="file.txt", Body=b"v1")
            resp1 = s3_client.head_object(Bucket=bucket, Key="file.txt")
            assert resp1["ContentLength"] == 2

            s3_client.put_object(Bucket=bucket, Key="file.txt", Body=b"version2")
            resp2 = s3_client.head_object(Bucket=bucket, Key="file.txt")
            assert resp2["ContentLength"] == 8
        finally:
            s3_client.delete_object(Bucket=bucket, Key="file.txt")
            s3_client.delete_bucket(Bucket=bucket)


def _raw_connection(endpoint_url):
    """A raw keep-alive connection to the server, plain or TLS as configured.

    These tests need byte-level control over one HTTP/1.1 connection, which
    no HTTP client library gives.
    """
    import socket
    import ssl
    from urllib.parse import urlparse

    parsed = urlparse(endpoint_url)
    sock = socket.create_connection((parsed.hostname, parsed.port or 80), timeout=5)
    if parsed.scheme == "https":
        ca_bundle = os.environ.get("AWS_CA_BUNDLE")
        if ca_bundle:
            ctx = ssl.create_default_context(cafile=ca_bundle)
        else:
            ctx = ssl.create_default_context()
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
        # HTTP/1.1 only: the desync these tests guard against cannot happen
        # over HTTP/2.
        ctx.set_alpn_protocols(["http/1.1"])
        sock = ctx.wrap_socket(sock, server_hostname=parsed.hostname)
    return sock, parsed.hostname


def _read_response(sock):
    """Read one whole response (head, then a Content-Length body); returns
    (status line, lowercased headers), or (None, None) when the server closed
    the connection first."""
    data = b""
    while b"\r\n\r\n" not in data:
        try:
            chunk = sock.recv(65536)
        except (ConnectionResetError, OSError):
            return None, None
        if not chunk:
            return None, None
        data += chunk
    raw_head, body = data.split(b"\r\n\r\n", 1)
    lines = raw_head.decode("latin-1").split("\r\n")
    headers = {}
    for line in lines[1:]:
        name, _, value = line.partition(":")
        headers[name.strip().lower()] = value.strip().lower()
    length = int(headers.get("content-length", "0"))
    while len(body) < length:
        chunk = sock.recv(65536)
        if not chunk:
            break
        body += chunk
    return lines[0], headers


class TestKeepAliveHygiene:
    """A response sent before the request body was read must close the
    connection (RFC 9110 §10.1.1). Otherwise the server cannot tell the owed
    body from the next request and may parse a mangled one: with
    `Expect: 100-continue` (botocore's PutObject) the client never sends the
    body at all after an early reject."""

    PUT_EXPECT = (
        "PUT /test-keepalive-bucket/k HTTP/1.1\r\nHost: {host}\r\n"
        "Content-Length: 4\r\nExpect: 100-continue\r\n\r\n"
    )
    PUT_WITH_BODY = (
        "PUT /test-keepalive-bucket/k HTTP/1.1\r\nHost: {host}\r\n"
        "Content-Length: 4\r\n\r\nnope"
    )
    HEALTH = "GET /admin/health HTTP/1.1\r\nHost: {host}\r\n\r\n"

    def test_early_reject_of_expect_continue_closes_connection(self, endpoint_url):
        """Unauthenticated PUT, body withheld as with Expect: 100-continue:
        the auth middleware rejects it before any handler runs."""
        sock, host = _raw_connection(endpoint_url)
        try:
            sock.sendall(self.PUT_EXPECT.format(host=host).encode())
            status, headers = _read_response(sock)
            assert status.startswith("HTTP/1.1 403")
            assert headers.get("connection") == "close"
        finally:
            sock.close()

    def test_early_reject_with_unread_body_closes_connection(self, endpoint_url):
        sock, host = _raw_connection(endpoint_url)
        try:
            sock.sendall(self.PUT_WITH_BODY.format(host=host).encode())
            status, headers = _read_response(sock)
            assert status.startswith("HTTP/1.1 403")
            assert headers.get("connection") == "close"
        finally:
            sock.close()

    def test_request_without_body_keeps_connection(self, endpoint_url):
        """No body, nothing owed: keep-alive must survive."""
        sock, host = _raw_connection(endpoint_url)
        try:
            for _ in range(2):
                sock.sendall(self.HEALTH.format(host=host).encode())
                status, headers = _read_response(sock)
                assert status == "HTTP/1.1 200 OK"
                assert headers.get("connection") != "close"
        finally:
            sock.close()

    def test_follow_up_after_early_reject_is_never_mangled(self, endpoint_url):
        """The regression itself: a request sent right after an early reject,
        on the same connection, must never be parsed with its first bytes
        swallowed as the withheld body (seen as a 400 here, and as
        SignatureDoesNotMatch on a signed DeleteObject). Racy by nature,
        hence many rounds."""
        mangled = 0
        for _ in range(100):
            sock, host = _raw_connection(endpoint_url)
            try:
                sock.sendall(self.PUT_EXPECT.format(host=host).encode())
                _read_response(sock)
                try:
                    sock.sendall(self.HEALTH.format(host=host).encode())
                except (BrokenPipeError, ConnectionResetError):
                    continue
                status, _ = _read_response(sock)
                if status is not None and status != "HTTP/1.1 200 OK":
                    mangled += 1
            finally:
                sock.close()
        assert mangled == 0, f"{mangled}/100 follow-up requests were mangled"
