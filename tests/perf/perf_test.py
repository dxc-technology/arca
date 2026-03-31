#!/usr/bin/env python3
"""
Performance test suite for Arca S3 server.

Measures throughput and latency for common S3 operations:
  - Small-object PUT/GET/HEAD/DELETE (concurrent)
  - Large-object multipart upload
  - Mixed workload (GET/PUT/DELETE)
  - Listing performance

Usage:
    python perf_test.py [--endpoint URL] [--threads N] [--objects N]
    python perf_test.py --json                         # machine-readable output
    python perf_test.py --json --baseline results.json # compare against baseline
"""

import argparse
import json
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


def report_latencies(name: str, latencies: list[float], errors: int = 0) -> dict:
    """Print a formatted latency report and return structured data."""
    if not latencies:
        print(f"  {name}: no successful operations")
        return {"name": name, "ops": 0, "errors": errors}

    total = len(latencies)
    total_time = sum(latencies)
    ops_sec = total / total_time if total_time > 0 else 0

    result = {
        "name": name,
        "ops": total,
        "errors": errors,
        "ops_per_sec": round(ops_sec, 1),
        "p50_ms": round(percentile(latencies, 50) * 1000, 1),
        "p95_ms": round(percentile(latencies, 95) * 1000, 1),
        "p99_ms": round(percentile(latencies, 99) * 1000, 1),
        "min_ms": round(min(latencies) * 1000, 1),
        "max_ms": round(max(latencies) * 1000, 1),
        "avg_ms": round(statistics.mean(latencies) * 1000, 1),
    }

    print(f"  {name}:")
    print(f"    Operations: {total} ({errors} errors)")
    print(f"    Throughput: {ops_sec:.1f} ops/sec")
    print(f"    Latency (ms):")
    print(f"      p50: {result['p50_ms']}")
    print(f"      p95: {result['p95_ms']}")
    print(f"      p99: {result['p99_ms']}")
    print(f"      min: {result['min_ms']}")
    print(f"      max: {result['max_ms']}")
    print(f"      avg: {result['avg_ms']}")

    return result


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


def test_head_object(client, bucket: str, threads: int, count: int):
    """Concurrent HEAD object (metadata lookup, tests cache effectiveness)."""
    print(f"\n--- HEAD Object ({count} objects, {threads} threads) ---")
    latencies = []
    errors = 0

    def head_one(i):
        _, elapsed = timed(
            lambda: client.head_object(Bucket=bucket, Key=f"perf/put/{i}")
        )
        return elapsed

    with ThreadPoolExecutor(max_workers=threads) as pool:
        futures = {pool.submit(head_one, i): i for i in range(count)}
        for fut in as_completed(futures):
            try:
                latencies.append(fut.result())
            except Exception:
                errors += 1

    return report_latencies("HEAD", latencies, errors)


def test_delete_object(client, bucket: str, threads: int, count: int):
    """Concurrent DELETE object."""
    print(f"\n--- DELETE Object ({count} objects, {threads} threads) ---")

    # Pre-populate objects for deletion.
    data = os.urandom(256)
    for i in range(count):
        client.put_object(Bucket=bucket, Key=f"perf/del/{i}", Body=data)

    latencies = []
    errors = 0

    def del_one(i):
        _, elapsed = timed(
            lambda: client.delete_object(Bucket=bucket, Key=f"perf/del/{i}")
        )
        return elapsed

    with ThreadPoolExecutor(max_workers=threads) as pool:
        futures = {pool.submit(del_one, i): i for i in range(count)}
        for fut in as_completed(futures):
            try:
                latencies.append(fut.result())
            except Exception:
                errors += 1

    return report_latencies("DELETE", latencies, errors)


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


def compare_with_baseline(current: dict, baseline_path: str):
    """Compare current results with a baseline JSON file."""
    try:
        with open(baseline_path) as f:
            baseline = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError) as e:
        print(f"\n  Warning: Could not load baseline: {e}")
        return

    print("\n" + "=" * 60)
    print("  Comparison with baseline")
    print("=" * 60)

    current_by_name = {r["name"]: r for r in current.get("results", [])}
    baseline_by_name = {r["name"]: r for r in baseline.get("results", [])}

    for name in current_by_name:
        if name not in baseline_by_name:
            continue
        cur = current_by_name[name]
        base = baseline_by_name[name]
        if cur.get("ops_per_sec") and base.get("ops_per_sec"):
            ratio = cur["ops_per_sec"] / base["ops_per_sec"]
            direction = "faster" if ratio > 1 else "slower"
            print(f"  {name}: {ratio:.2f}x {direction} "
                  f"({base['ops_per_sec']:.1f} -> {cur['ops_per_sec']:.1f} ops/sec)")
        if cur.get("p50_ms") and base.get("p50_ms"):
            delta = cur["p50_ms"] - base["p50_ms"]
            sign = "+" if delta > 0 else ""
            print(f"    p50: {sign}{delta:.1f}ms "
                  f"({base['p50_ms']:.1f} -> {cur['p50_ms']:.1f})")


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
    parser.add_argument("--json", action="store_true", help="Output JSON results")
    parser.add_argument("--baseline", help="Baseline JSON file for comparison")
    args = parser.parse_args()

    client = create_client(args.endpoint)
    bucket = "arca-perf-test"

    print("=" * 60)
    print("  Arca S3 Performance Test Suite")
    print(f"  Endpoint: {args.endpoint}")
    print(f"  Threads:  {args.threads}")
    print("=" * 60)

    ensure_bucket(client, bucket)
    all_results = []

    try:
        test_small_object_put(client, bucket, args.threads, args.objects, 1024)
        r = test_small_object_get(client, bucket, args.threads, args.objects)
        if isinstance(r, dict):
            all_results.append(r)
        r = test_head_object(client, bucket, args.threads, args.objects)
        if isinstance(r, dict):
            all_results.append(r)
        r = test_delete_object(client, bucket, args.threads, args.objects)
        if isinstance(r, dict):
            all_results.append(r)
        test_large_multipart(client, bucket, args.large_mb)
        test_mixed_workload(client, bucket, args.threads, args.mixed_ops)
        test_listing(client, bucket, args.list_objects)
    finally:
        print("\n--- Cleanup ---")
        cleanup_bucket(client, bucket)
        print("  Done.")

    output = {
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "endpoint": args.endpoint,
        "threads": args.threads,
        "objects": args.objects,
        "results": all_results,
    }

    if args.json:
        json_path = "perf_results.json"
        with open(json_path, "w") as f:
            json.dump(output, f, indent=2)
        print(f"\n  JSON results written to {json_path}")

    if args.baseline:
        compare_with_baseline(output, args.baseline)


if __name__ == "__main__":
    main()
