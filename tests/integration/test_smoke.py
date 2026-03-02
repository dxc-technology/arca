"""Smoke tests for Arca.

Verifies that unimplemented endpoints return valid S3 XML error responses
(501 NotImplemented). As operations are implemented in later phases, the
corresponding 501 tests are removed from here.

Note: All bucket/object operations are now implemented through Phase 5
(multipart upload). No more 501 smoke tests remain.
"""
