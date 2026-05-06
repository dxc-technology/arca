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
import math
import os
import statistics
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from random import Random

import boto3
from botocore.config import Config


def create_client(endpoint: str) -> boto3.client:
    """Create an S3 client configured for Arca."""
    # TLS verification: explicit CA bundle path, no verification, or default.
    verify: bool | str = True
    if os.environ.get("ARCA_TLS_NO_VERIFY"):
        verify = False
        import urllib3
        urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)
    elif os.environ.get("AWS_CA_BUNDLE"):
        verify = os.environ["AWS_CA_BUNDLE"]
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
        verify=verify,
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
        return {"name": name, "ops": 0, "errors": errors, "throughput": 0, "throughput_unit": "ops/s"}

    total = len(latencies)
    total_time = sum(latencies)
    ops_sec = total / total_time if total_time > 0 else 0

    result = {
        "name": name,
        "ops": total,
        "errors": errors,
        "ops_per_sec": round(ops_sec, 1),
        "throughput": round(ops_sec, 1),
        "throughput_unit": "ops/s",
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

    return report_latencies("Small PUT", latencies, errors)


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

    return report_latencies("Small GET", latencies, errors)


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

    return {
        "name": "Multipart Upload",
        "throughput": round(throughput_mb, 1),
        "throughput_unit": "MB/s",
        "elapsed_s": round(elapsed, 2),
        "parts": part_num,
    }


def _multipart_upload_one(
    endpoint: str,
    bucket: str,
    key: str,
    total_size: int,
    part_size: int,
) -> dict:
    """Run one multipart upload from start to finish.

    Returns timing breakdown so we can isolate Complete cost from part upload.
    Uses a fresh client per worker to avoid shared-connection contention.
    """
    cli = create_client(endpoint)

    t0 = time.monotonic()
    resp = cli.create_multipart_upload(Bucket=bucket, Key=key)
    upload_id = resp["UploadId"]
    t_create = time.monotonic() - t0

    parts: list[dict] = []
    part_num = 0
    remaining = total_size
    t_parts_start = time.monotonic()
    while remaining > 0:
        part_num += 1
        chunk = min(part_size, remaining)
        body = os.urandom(chunk)
        part_resp = cli.upload_part(
            Bucket=bucket,
            Key=key,
            UploadId=upload_id,
            PartNumber=part_num,
            Body=body,
        )
        parts.append({"PartNumber": part_num, "ETag": part_resp["ETag"]})
        remaining -= chunk
    t_parts = time.monotonic() - t_parts_start

    t_complete_start = time.monotonic()
    cli.complete_multipart_upload(
        Bucket=bucket,
        Key=key,
        UploadId=upload_id,
        MultipartUpload={"Parts": parts},
    )
    t_complete = time.monotonic() - t_complete_start

    return {
        "key": key,
        "size": total_size,
        "parts": part_num,
        "create_s": t_create,
        "parts_s": t_parts,
        "complete_s": t_complete,
        "total_s": t_create + t_parts + t_complete,
    }


def test_parallel_multipart(
    endpoint: str,
    bucket: str,
    parallel: int,
    upload_mb: int,
    part_mb: int,
):
    """Concurrent multipart uploads. Reproduces Percona PBM-like backup pattern.

    Each worker uploads `upload_mb` MiB in `part_mb` MiB parts. We track
    `complete_s` separately because the encrypted concat() pathway is the
    suspected primary bottleneck (re-encrypts the whole object during
    CompleteMultipartUpload).
    """
    print(
        f"\n--- Parallel Multipart "
        f"({parallel} concurrent uploads, {upload_mb}MB each, {part_mb}MB parts) ---"
    )
    part_size = part_mb * 1024 * 1024
    total_size = upload_mb * 1024 * 1024

    keys = [f"perf/parallel/{i:03d}.bin" for i in range(parallel)]
    overall_start = time.monotonic()
    results: list[dict] = []
    errors = 0

    with ThreadPoolExecutor(max_workers=parallel) as pool:
        futures = {
            pool.submit(_multipart_upload_one, endpoint, bucket, k, total_size, part_size): k
            for k in keys
        }
        for fut in as_completed(futures):
            try:
                results.append(fut.result())
            except Exception as e:
                errors += 1
                print(f"  ERROR on {futures[fut]}: {e}")

    overall_elapsed = time.monotonic() - overall_start
    total_bytes = sum(r["size"] for r in results)
    aggregate_mb_s = (total_bytes / (1024 * 1024)) / overall_elapsed if overall_elapsed > 0 else 0

    parts_lats = [r["parts_s"] for r in results]
    complete_lats = [r["complete_s"] for r in results]
    total_lats = [r["total_s"] for r in results]

    p50_complete = percentile(complete_lats, 50)
    p99_complete = percentile(complete_lats, 99)
    avg_complete = statistics.mean(complete_lats) if complete_lats else 0
    p50_parts = percentile(parts_lats, 50)
    p99_parts = percentile(parts_lats, 99)

    print(f"  Wall clock:      {overall_elapsed:.2f}s")
    print(f"  Aggregate:       {aggregate_mb_s:.1f} MB/s")
    print(f"  Successful uploads: {len(results)} ({errors} errors)")
    print(f"  Per-upload parts time:    p50={p50_parts:.2f}s p99={p99_parts:.2f}s")
    print(f"  Per-upload complete time: p50={p50_complete:.3f}s p99={p99_complete:.3f}s avg={avg_complete:.3f}s")

    return {
        "name": "Parallel Multipart",
        "throughput": round(aggregate_mb_s, 1),
        "throughput_unit": "MB/s",
        "parallel": parallel,
        "upload_mb": upload_mb,
        "part_mb": part_mb,
        "uploads": len(results),
        "errors": errors,
        "wall_clock_s": round(overall_elapsed, 2),
        "complete_p50_s": round(p50_complete, 3),
        "complete_p99_s": round(p99_complete, 3),
        "complete_avg_s": round(avg_complete, 3),
        "parts_p50_s": round(p50_parts, 2),
        "parts_p99_s": round(p99_parts, 2),
        "total_p50_s": round(percentile(total_lats, 50), 2),
    }


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

    all_lats = get_lats + put_lats + del_lats
    total_ops = len(all_lats)
    total_time = sum(all_lats) if all_lats else 0
    combined_ops_sec = total_ops / total_time if total_time > 0 else 0
    result = {
        "name": "Mixed Workload",
        "ops": total_ops,
        "errors": errors,
        "ops_per_sec": round(combined_ops_sec, 1),
        "throughput": round(combined_ops_sec, 1),
        "throughput_unit": "ops/s",
    }
    if all_lats:
        result["p50_ms"] = round(percentile(all_lats, 50) * 1000, 1)
        result["p95_ms"] = round(percentile(all_lats, 95) * 1000, 1)
        result["p99_ms"] = round(percentile(all_lats, 99) * 1000, 1)
    return result


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

    return {
        "name": "Listing",
        "throughput": round(object_count / avg, 1) if avg > 0 else 0,
        "throughput_unit": "obj/s",
        "avg_s": round(avg, 3),
        "objects": object_count,
    }


def compute_global_score(results: list[dict]) -> float:
    """Weighted geometric mean of throughput values across all tests.

    The score is a composite metric for comparing performance across runs.
    Higher is better. The absolute value depends on the workload parameters.
    """
    weights = {
        "Small PUT": 0.20,
        "Small GET": 0.25,
        "HEAD": 0.10,
        "DELETE": 0.10,
        "Multipart Upload": 0.10,
        "Mixed Workload": 0.15,
        "Listing": 0.10,
    }
    log_sum = 0.0
    weight_sum = 0.0
    for r in results:
        w = weights.get(r.get("name", ""), 0)
        t = r.get("throughput", 0)
        if w > 0 and t > 0:
            log_sum += w * math.log(t)
            weight_sum += w
    if weight_sum == 0:
        return 0.0
    return math.exp(log_sum / weight_sum)


def print_summary_table(results: list[dict]) -> float:
    """Print a summary table and return the global performance score."""
    print("\n" + "=" * 72)
    print("  PERFORMANCE SUMMARY")
    print("=" * 72)
    print(f"  {'Test':<22s}  {'Throughput':>14s}  {'p50 ms':>8s}  {'p95 ms':>8s}  {'p99 ms':>8s}")
    print("  " + "-" * 68)
    for r in results:
        tput = f"{r.get('throughput', 0):>8.1f} {r.get('throughput_unit', ''):5s}"
        p50 = f"{r['p50_ms']:8.1f}" if "p50_ms" in r else "       -"
        p95 = f"{r['p95_ms']:8.1f}" if "p95_ms" in r else "       -"
        p99 = f"{r['p99_ms']:8.1f}" if "p99_ms" in r else "       -"
        print(f"  {r.get('name', '?'):<22s}  {tput}  {p50}  {p95}  {p99}")
    print("  " + "-" * 68)
    score = compute_global_score(results)
    print(f"\n  Global Performance Index: {score:.1f}")
    print("=" * 72)
    return score


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
    parser.add_argument(
        "--scenarios",
        default="all",
        help="Comma-separated scenarios to run: all, parallel-multipart "
        "(default: all). 'parallel-multipart' alone runs ONLY that scenario, "
        "useful for targeted encryption benchmarks. 'all,parallel-multipart' "
        "runs everything.",
    )
    parser.add_argument(
        "--parallel-uploads",
        type=int,
        default=8,
        help="Concurrent multipart uploads in the parallel-multipart scenario",
    )
    parser.add_argument(
        "--parallel-mb",
        type=int,
        default=512,
        help="Total MiB per multipart upload in the parallel-multipart scenario",
    )
    parser.add_argument(
        "--parallel-part-mb",
        type=int,
        default=8,
        help="MiB per part in the parallel-multipart scenario",
    )
    parser.add_argument("--json", action="store_true", help="Output JSON results")
    parser.add_argument("--baseline", help="Baseline JSON file for comparison")
    parser.add_argument("-q", "--quiet", action="store_true",
                        help="Show only the summary table, suppress per-test details")
    args = parser.parse_args()

    client = create_client(args.endpoint)
    bucket = "arca-perf-test"

    # In quiet mode, suppress all per-test output by redirecting stdout.
    saved_stdout = sys.stdout
    if args.quiet:
        sys.stdout = open(os.devnull, "w")

    print("=" * 60)
    print("  Arca S3 Performance Test Suite")
    print(f"  Endpoint: {args.endpoint}")
    print(f"  Threads:  {args.threads}")
    print("=" * 60)

    ensure_bucket(client, bucket)
    all_results = []

    scenarios = {s.strip() for s in args.scenarios.split(",") if s.strip()}
    run_all = "all" in scenarios
    run_parallel_multipart = "parallel-multipart" in scenarios

    try:
        if run_all:
            all_results.append(
                test_small_object_put(client, bucket, args.threads, args.objects, 1024)
            )
            all_results.append(
                test_small_object_get(client, bucket, args.threads, args.objects)
            )
            all_results.append(
                test_head_object(client, bucket, args.threads, args.objects)
            )
            all_results.append(
                test_delete_object(client, bucket, args.threads, args.objects)
            )
            all_results.append(
                test_large_multipart(client, bucket, args.large_mb)
            )
            all_results.append(
                test_mixed_workload(client, bucket, args.threads, args.mixed_ops)
            )
            all_results.append(
                test_listing(client, bucket, args.list_objects)
            )
        if run_parallel_multipart:
            all_results.append(
                test_parallel_multipart(
                    args.endpoint,
                    bucket,
                    args.parallel_uploads,
                    args.parallel_mb,
                    args.parallel_part_mb,
                )
            )
    finally:
        print("\n--- Cleanup ---")
        cleanup_bucket(client, bucket)
        print("  Done.")
        # Restore stdout before summary table.
        sys.stdout = saved_stdout

    score = print_summary_table(all_results)

    output = {
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "endpoint": args.endpoint,
        "threads": args.threads,
        "objects": args.objects,
        "results": all_results,
        "score": score,
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
