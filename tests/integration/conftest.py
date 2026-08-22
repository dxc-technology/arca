"""Shared fixtures for Arca integration tests."""

import os
import urllib3
from urllib.parse import urlparse

import boto3
import pytest
from minio import Minio


def pytest_configure(config):
    """Register custom markers used by the 3-node cluster suite.

    Each marker tags a test with the cluster topology it requires; `bin/test
    cluster` orchestrates node up/down between phases and selects the matching
    tests with `pytest -m <marker>`.
    """
    for name, desc in [
        ("cluster_full", "requires all 3 cluster nodes up"),
        ("cluster_two_thirds", "requires exactly 2 of 3 nodes up (quorum still met)"),
        ("cluster_one_third", "requires only 1 of 3 nodes up (no quorum)"),
        ("cluster_catchup_verify", "verifies anti-entropy convergence after a node returns"),
        ("cluster_insufficient_storage", "requires the 507 overlay (one tiny-disk node)"),
        ("cluster_config_drift", "requires the drift overlay (one mismatched-secret node)"),
        ("cluster_config_drift_majority", "requires drift overlays on two nodes (distinct wrong secrets)"),
        ("cluster_partition_before", "seeds state with all 3 up, before a network partition"),
        ("cluster_partition_minority", "requires node 3 partitioned off (process alive, network cut)"),
        ("cluster_partition_healed", "verifies convergence after the partition heals"),
        ("cluster_available_full", "requires the available-mode overlay with all 3 nodes up"),
        ("cluster_available_split", "requires the available overlay with node 3 partitioned"),
        ("cluster_available_converged", "verifies LWW convergence after the available-mode split heals"),
        ("cluster_available_minority", "requires the available overlay with only node 1 up"),
        ("cluster_leader_full", "requires all 3 up; verifies the R6 worker-leader gate"),
        ("cluster_leader_failover", "requires the worker leader stopped; verifies role failover"),
        ("cluster_syncing_seed", "requires all 3 up; seeds state for the R7 readiness phase"),
        ("cluster_syncing_while_down", "requires node 3 down; writes its catch-up data"),
        ("cluster_syncing_readiness", "requires node 3 just restarted (no wait): observes 503 syncing, then 200 with the data present"),
        ("cluster_node_views_full", "requires all 3 up; verifies the R8 ?node= admin proxy and merged view"),
        ("cluster_node_views_degraded", "requires node 3 down; verifies ?node= errors and merged-view degradation"),
        ("cluster_blob_repair_seed", "requires the gc overlay with all 3 up; seeds the proactive blob-repair guinea pig"),
        ("cluster_blob_repair_verify", "requires the runner to have observed the deleted payload file restored on node 3"),
        ("cluster_gc_seed", "requires the gc overlay with all 3 up; seeds the tombstone-GC guinea pig"),
        ("cluster_gc_blocked", "requires node 3 down beyond the 20s test grace; observes the \u00a73.2 GC guard"),
        ("cluster_gc_recovered", "requires node 3 back up; verifies no resurrection and the guard releasing"),
    ]:
        config.addinivalue_line("markers", f"{name}: {desc}")


@pytest.fixture
def endpoint_url():
    """S3 endpoint URL for the Arca server."""
    return os.environ.get("ARCA_ENDPOINT", "http://localhost:9000")


@pytest.fixture
def s3_client(endpoint_url):
    """boto3 S3 client configured to talk to Arca with valid credentials."""
    kwargs = dict(
        endpoint_url=endpoint_url,
        aws_access_key_id=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"
        ),
        aws_secret_access_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
        ),
        region_name="us-east-1",
    )
    ca_bundle = os.environ.get("AWS_CA_BUNDLE")
    if endpoint_url.startswith("https://") and ca_bundle:
        kwargs["verify"] = ca_bundle
    return boto3.client("s3", **kwargs)


@pytest.fixture
def minio_client(endpoint_url):
    """MinIO Python client configured to talk to Arca with valid credentials."""
    parsed = urlparse(endpoint_url)
    secure = parsed.scheme == "https"
    kwargs = dict(
        access_key=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"
        ),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
        ),
        secure=secure,
    )
    ca_bundle = os.environ.get("AWS_CA_BUNDLE")
    if secure and ca_bundle:
        http_client = urllib3.PoolManager(
            cert_reqs="CERT_REQUIRED",
            ca_certs=ca_bundle,
        )
        kwargs["http_client"] = http_client
    return Minio(parsed.netloc, **kwargs)
