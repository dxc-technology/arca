#!/usr/bin/env python3
"""
Performance test suite for Arca S3 server.

Measures throughput and latency for common S3 operations:
  - Small-object PUT/GET (concurrent)
  - Large-object multipart upload
  - Mixed workload (GET/PUT/DELETE)
  - Listing performance

Usage:
    python perf_test.py [--endpoint URL] [--threads N] [--objects N]
"""

import argparse
import os
import statistics
import string
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from random import Random

import boto3
from botocore.config import Config


def create_client(endpoint: str) -> boto3.client:
    """Create an S3 client configured for Arca."""
    return boto3.client(
        "s3",
        endpoint_url=endpoint,
        aws_access_key_id=os.environ.get(
            "AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"
        ),
        aws_secret_access_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        ),
        region_name="us-east-1",
        config=Config(
            signature_version="s3v4",
            retries={"max_attempts": 0},
        ),
    )


def timed(fn):
    """Execute fn and return (result, elapsed_seconds)."""
    start = time.monotonic()
    result = fn()
    elapsed = time.monotonic() - start
    return result, elapsed


def percentile(data: list[float], p: float) -> float:
    """Return the p-th percentile of sorted data."""
    if not data:
        return 0.0
    data_sorted = sorted(data)
    k = (len(data_sorted) - 1) * (p / 100)
    f = int(k)
    c = f + 1
    if c >= len(data_sorted):
        return data_sorted[f]
    return data_sorted[f] + (k - f) * (data_sorted[c] - data_sorted[f])


def report_latencies(name: str, latencies: list[float], errors: int = 0):
    """Print a formatted latency report."""
    if not latencies:
        print(f"  {name}: no successful operations")
        return

    total = len(latencies)
    total_time = sum(latencies)
    ops_sec = total / total_time if total_time > 0 else 0

    print(f"  {name}:")
    print(f"    Operations: {total} ({errors} errors)")
    print(f"    Throughput: {ops_sec:.1f} ops/sec")
    print(f"    Latency (ms):")
    print(f"      p50: {percentile(latencies, 50)*1000:.1f}")
    print(f"      p95: {percentile(latencies, 95)*1000:.1f}")
    print(f"      p99: {percentile(latencies, 99)*1000:.1f}")
    print(f"      min: {min(latencies)*1000:.1f}")
    print(f"      max: {max(latencies)*1000:.1f}")
    print(f"      avg: {statistics.mean(latencies)*1000:.1f}")


def ensure_bucket(client, bucket: str):
    """Create bucket if it doesn't exist."""
    try:
        client.create_bucket(Bucket=bucket)
    except client.exceptions.BucketAlreadyOwnedByYou:
        pass


def cleanup_bucket(client, bucket: str):
    """Delete all objects and the bucket."""
    try:
        paginator = client.get_paginator("list_objects_v2")
        for page in paginator.paginate(Bucket=bucket):
            for obj in page.get("Contents", []):
                client.delete_object(Bucket=bucket, Key=obj["Key"])
        client.delete_bucket(Bucket=bucket)
    except Exception:
        pass


def test_small_object_put(client, bucket: str, threads: int, count: int, size: int):
    """Concurrent small-object PUT."""
    print(f"\n--- Small Object PUT ({count} objects, {size}B, {threads} threads) ---")
    data = os.urandom(size)
    latencies = []
    errors = 0

    def put_one(i):
        _, elapsed = timed(
            lambda: client.put_object(Bucket=bucket, Key=f"perf/put/{i}", Body=data)
        )
        return elapsed

    with ThreadPoolExecutor(max_workers=threads) as pool:
        futures = {pool.submit(put_one, i): i for i in range(count)}
        for fut in as_completed(futures):
            try:
                latencies.append(fut.result())
            except Exception:
                errors += 1

    report_latencies("PUT", latencies, errors)
    return latencies


def test_small_object_get(client, bucket: str, threads: int, count: int):
    """Concurrent small-object GET (objects must exist from PUT test)."""
    print(f"\n--- Small Object GET ({count} objects, {threads} threads) ---")
    latencies = []
    errors = 0

    def get_one(i):
        _, elapsed = timed(
            lambda: client.get_object(Bucket=bucket, Key=f"perf/put/{i}")
        )
        return elapsed

    with ThreadPoolExecutor(max_workers=threads) as pool:
        futures = {pool.submit(get_one, i): i for i in range(count)}
        for fut in as_completed(futures):
            try:
                latencies.append(fut.result())
            except Exception:
                errors += 1

    report_latencies("GET", latencies, errors)
    return latencies


def test_large_multipart(client, bucket: str, size_mb: int):
    """Large-object multipart upload."""
    print(f"\n--- Large Multipart Upload ({size_mb}MB) ---")
    part_size = 5 * 1024 * 1024  # 5MB minimum
    key = "perf/large-object"
    total_size = size_mb * 1024 * 1024

    start = time.monotonic()

    resp = client.create_multipart_upload(Bucket=bucket, Key=key)
    upload_id = resp["UploadId"]

    parts = []
    part_num = 0
    remaining = total_size
    while remaining > 0:
        part_num += 1
        chunk_size = min(part_size, remaining)
        data = os.urandom(chunk_size)
        part_resp = client.upload_part(
            Bucket=bucket,
            Key=key,
            UploadId=upload_id,
            PartNumber=part_num,
            Body=data,
        )
        parts.append({"PartNumber": part_num, "ETag": part_resp["ETag"]})
        remaining -= chunk_size

    client.complete_multipart_upload(
        Bucket=bucket,
        Key=key,
        UploadId=upload_id,
        MultipartUpload={"Parts": parts},
    )

    elapsed = time.monotonic() - start
    throughput_mb = size_mb / elapsed

    print(f"  Upload time: {elapsed:.2f}s")
    print(f"  Throughput: {throughput_mb:.1f} MB/s")
    print(f"  Parts: {part_num}")


def test_mixed_workload(client, bucket: str, threads: int, ops: int):
    """Mixed workload: 70% GET, 20% PUT, 10% DELETE."""
    print(f"\n--- Mixed Workload ({ops} ops, {threads} threads) ---")

    # Pre-populate some objects
    for i in range(100):
        client.put_object(
            Bucket=bucket, Key=f"perf/mixed/{i}", Body=os.urandom(1024)
        )

    rng = Random(42)
    get_lats, put_lats, del_lats = [], [], []
    errors = 0

    def do_op(op_idx):
        r = rng.random()
        try:
            if r < 0.7:
                i = rng.randint(0, 99)
                _, elapsed = timed(
                    lambda: client.get_object(
                        Bucket=bucket, Key=f"perf/mixed/{i}"
                    )
                )
                return ("get", elapsed)
            elif r < 0.9:
                i = rng.randint(100, 999)
                _, elapsed = timed(
                    lambda: client.put_object(
                        Bucket=bucket,
                        Key=f"perf/mixed/{i}",
                        Body=os.urandom(1024),
                    )
                )
                return ("put", elapsed)
            else:
                i = rng.randint(100, 999)
                _, elapsed = timed(
                    lambda: client.delete_object(
                        Bucket=bucket, Key=f"perf/mixed/{i}"
                    )
                )
                return ("del", elapsed)
        except Exception:
            return ("error", 0)

    with ThreadPoolExecutor(max_workers=threads) as pool:
        futures = [pool.submit(do_op, i) for i in range(ops)]
        for fut in as_completed(futures):
            kind, lat = fut.result()
            if kind == "get":
                get_lats.append(lat)
            elif kind == "put":
                put_lats.append(lat)
            elif kind == "del":
                del_lats.append(lat)
            else:
                errors += 1

    report_latencies("GET (70%)", get_lats)
    report_latencies("PUT (20%)", put_lats)
    report_latencies("DELETE (10%)", del_lats)
    print(f"  Total errors: {errors}")


def test_listing(client, bucket: str, object_count: int):
    """List performance with many objects."""
    print(f"\n--- Listing Performance ({object_count} objects) ---")

    # Populate
    print(f"  Populating {object_count} objects...")
    data = b"x"
    for i in range(object_count):
        client.put_object(Bucket=bucket, Key=f"perf/list/{i:06d}", Body=data)

    # Time a full listing
    latencies = []
    for _ in range(3):
        start = time.monotonic()
        count = 0
        paginator = client.get_paginator("list_objects_v2")
        for page in paginator.paginate(Bucket=bucket, Prefix="perf/list/"):
            count += len(page.get("Contents", []))
        elapsed = time.monotonic() - start
        latencies.append(elapsed)

    avg = statistics.mean(latencies)
    print(f"  Objects listed: {object_count}")
    print(f"  Time (avg of 3): {avg:.3f}s")
    print(f"  Objects/sec: {object_count/avg:.0f}")


def main():
    parser = argparse.ArgumentParser(description="Arca S3 Performance Tests")
    parser.add_argument(
        "--endpoint",
        default=os.environ.get("ARCA_ENDPOINT", "http://localhost:9000"),
    )
    parser.add_argument("--threads", type=int, default=10)
    parser.add_argument("--objects", type=int, default=500)
    parser.add_argument("--list-objects", type=int, default=1000)
    parser.add_argument("--large-mb", type=int, default=50)
    parser.add_argument("--mixed-ops", type=int, default=500)
    args = parser.parse_args()

    client = create_client(args.endpoint)
    bucket = "arca-perf-test"

    print("=" * 60)
    print("  Arca S3 Performance Test Suite")
    print(f"  Endpoint: {args.endpoint}")
    print(f"  Threads:  {args.threads}")
    print("=" * 60)

    ensure_bucket(client, bucket)

    try:
        test_small_object_put(client, bucket, args.threads, args.objects, 1024)
        test_small_object_get(client, bucket, args.threads, args.objects)
        test_large_multipart(client, bucket, args.large_mb)
        test_mixed_workload(client, bucket, args.threads, args.mixed_ops)
        test_listing(client, bucket, args.list_objects)
    finally:
        print("\n--- Cleanup ---")
        cleanup_bucket(client, bucket)
        print("  Done.")


if __name__ == "__main__":
    main()
