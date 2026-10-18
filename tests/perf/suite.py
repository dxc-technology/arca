#!/usr/bin/env python3
"""
Runner of the Arca performance suite (`bin/perf-test suite`).

bin/perf-test starts and stops the servers and runs warp; this script does the
steps that need an S3 client or the Arca binary, inside the perf-runner image:

    wait            wait until the server answers ListBuckets
    prepare-bucket  create a bucket, optionally with Arca bucket compression
    listing         seed a key tree and time full ListObjectsV2 walks
    seed            seed the data the maintenance benchmark works on
    metrics         fetch Arca's Prometheus metrics (/admin/metrics)
    maintenance     time the offline maintenance commands (server stopped)
    report          generate report.html and summary.json (see report.py)

Every step that produces a result writes it as JSON to --out (or stdout).
The S3 endpoint and the credentials come from ARCA_ENDPOINT,
AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

MIB = 1024 * 1024

# Seeded key tree for the listing walks: DIRS top-level prefixes, each with
# SUBDIRS sub-prefixes; the objects are spread evenly over the leaves.
DIRS = 10
SUBDIRS = 10


def endpoint() -> str:
    return os.environ.get("ARCA_ENDPOINT", "http://arca:9000")


def client(max_pool: int = 64):
    import boto3
    from botocore.config import Config

    return boto3.client(
        "s3", endpoint_url=endpoint(), region_name="us-east-1",
        aws_access_key_id=os.environ.get("AWS_ACCESS_KEY_ID"),
        aws_secret_access_key=os.environ.get("AWS_SECRET_ACCESS_KEY"),
        config=Config(signature_version="s3v4", max_pool_connections=max_pool,
                      retries={"max_attempts": 3, "mode": "standard"},
                      s3={"addressing_style": "path"}),
    )


def signed_request(method: str, url: str, body: bytes = b"", headers: dict | None = None):
    """A SigV4-signed request to an endpoint boto3 has no operation for."""
    import urllib.request

    import botocore.session
    from botocore.auth import S3SigV4Auth
    from botocore.awsrequest import AWSRequest

    creds = botocore.session.get_session().get_credentials()
    req = AWSRequest(method=method, url=url, data=body, headers=headers or {})
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(req)
    http = urllib.request.Request(url, data=body or None, method=method,
                                  headers=dict(req.headers.items()))
    with urllib.request.urlopen(http, timeout=30) as resp:
        return resp.status, resp.read()


def write_result(doc: dict, out: str | None):
    text = json.dumps(doc, indent=2) + "\n"
    if out:
        Path(out).parent.mkdir(parents=True, exist_ok=True)
        Path(out).write_text(text)
    else:
        sys.stdout.write(text)


# --- wait / prepare-bucket / metrics -------------------------------------------

def cmd_wait(args) -> int:
    deadline = time.monotonic() + args.timeout
    last = None
    while time.monotonic() < deadline:
        try:
            client().list_buckets()
            return 0
        except Exception as e:  # noqa: BLE001 - any failure means "not ready yet"
            last = e
            time.sleep(0.5)
    print(f"server at {endpoint()} not ready after {args.timeout}s: {last}", file=sys.stderr)
    return 1


def compression_xml(algorithm: str) -> bytes:
    return (f"<CompressionConfiguration><Algorithm>{algorithm}</Algorithm>"
            f"</CompressionConfiguration>").encode()


def cmd_prepare_bucket(args) -> int:
    s3 = client()
    try:
        s3.create_bucket(Bucket=args.bucket)
    except s3.exceptions.BucketAlreadyOwnedByYou:
        pass
    if args.compression:
        # Arca's bucket compression subresource (guide/compression.md).
        status, _ = signed_request("PUT", f"{endpoint()}/{args.bucket}?compression",
                                   compression_xml(args.compression),
                                   {"Content-Type": "application/xml"})
        if status != 200:
            print(f"PUT ?compression answered {status}", file=sys.stderr)
            return 1
    return 0


def cmd_metrics(args) -> int:
    _, body = signed_request("GET", f"{endpoint()}/admin/metrics")
    Path(args.out).write_bytes(body)
    return 0


# --- listing -------------------------------------------------------------------

def tree_keys(objects: int) -> list[str]:
    """Keys of the listing tree: d00/s00/o0000000, spread evenly over the leaves."""
    return [f"d{i % DIRS:02d}/s{(i // DIRS) % SUBDIRS:02d}/o{i:07d}" for i in range(objects)]


def put_many(s3, bucket: str, keys: list[str], body_for, threads: int) -> None:
    def put(key):
        s3.put_object(Bucket=bucket, Key=key, Body=body_for(key))
    with ThreadPoolExecutor(max_workers=threads) as pool:
        for _ in pool.map(put, keys):
            pass


def walk(s3, bucket: str, **params) -> tuple[int, int]:
    """One full ListObjectsV2 walk: (requests, entries)."""
    requests = entries = 0
    token = None
    while True:
        kw = dict(params, Bucket=bucket, MaxKeys=1000)
        if token:
            kw["ContinuationToken"] = token
        resp = s3.list_objects_v2(**kw)
        requests += 1
        entries += len(resp.get("Contents", [])) + len(resp.get("CommonPrefixes", []))
        if not resp.get("IsTruncated"):
            return requests, entries
        token = resp["NextContinuationToken"]


def walk_versions(s3, bucket: str, **params) -> tuple[int, int]:
    requests = entries = 0
    markers = {}
    while True:
        resp = s3.list_object_versions(Bucket=bucket, MaxKeys=1000, **params, **markers)
        requests += 1
        entries += (len(resp.get("Versions", [])) + len(resp.get("DeleteMarkers", []))
                    + len(resp.get("CommonPrefixes", [])))
        if not resp.get("IsTruncated"):
            return requests, entries
        markers = {"KeyMarker": resp["NextKeyMarker"]}
        if resp.get("NextVersionIdMarker"):
            markers["VersionIdMarker"] = resp["NextVersionIdMarker"]


LISTING_WALKS = [
    ("flat", "every key, no delimiter", walk, {}),
    ("delimiter-root", f"delimiter / at the root: {DIRS} common prefixes", walk, {"Delimiter": "/"}),
    ("delimiter-dir", f"delimiter / under one top-level prefix: {SUBDIRS} common prefixes",
     walk, {"Prefix": "d03/", "Delimiter": "/"}),
    ("prefix-leaf", "the keys of one leaf prefix", walk, {"Prefix": "d07/s04/"}),
    ("prefix-missing", "a prefix that matches nothing", walk, {"Prefix": "zz/"}),
    ("versions", "ListObjectVersions, every version", walk_versions, {}),
]


def cmd_listing(args) -> int:
    s3 = client()
    bucket = args.bucket
    try:
        s3.create_bucket(Bucket=bucket)
    except s3.exceptions.BucketAlreadyOwnedByYou:
        pass
    keys = tree_keys(args.objects)
    start = time.monotonic()
    put_many(s3, bucket, keys, lambda _k: b"x", args.threads)
    seed_s = time.monotonic() - start
    walks = []
    for name, description, fn, params in LISTING_WALKS:
        times = []
        for _ in range(args.repeat):
            t0 = time.monotonic()
            requests, entries = fn(s3, bucket, **params)
            times.append(time.monotonic() - t0)
        walks.append({"name": name, "description": description,
                      "seconds": statistics.median(times), "all_seconds": times,
                      "requests": requests, "entries": entries})
    write_result({"objects": args.objects, "repeat": args.repeat,
                  "seed_seconds": round(seed_s, 3),
                  "seed_puts_per_s": round(args.objects / seed_s, 1) if seed_s else None,
                  "walks": walks}, args.out)
    return 0


# --- seed (maintenance dataset) ------------------------------------------------

def seed_body(key: str, size: int) -> bytes:
    """Compressible, key-dependent content (a log-like line repeated)."""
    line = f"{key} level=info msg=\"perf suite seed\" n={len(key)}\n".encode()
    return (line * (size // len(line) + 1))[:size]


SEED_SIZES = [1024, 4096, 16384, 65536, 262144]


def cmd_seed(args) -> int:
    s3 = client()
    for b in (args.bucket, args.versioned_bucket):
        try:
            s3.create_bucket(Bucket=b)
        except s3.exceptions.BucketAlreadyOwnedByYou:
            pass
    s3.put_bucket_versioning(Bucket=args.versioned_bucket,
                             VersioningConfiguration={"Status": "Enabled"})
    keys = [f"seed/{i % 100:02d}/obj-{i:08d}" for i in range(args.objects)]
    sizes = {k: SEED_SIZES[i % len(SEED_SIZES)] for i, k in enumerate(keys)}
    start = time.monotonic()
    put_many(s3, args.bucket, keys, lambda k: seed_body(k, sizes[k]), args.threads)
    # A tenth of the objects again, in three versions each, on the versioned bucket.
    vkeys = keys[: max(1, args.objects // 10)]
    for _ in range(3):
        put_many(s3, args.versioned_bucket, vkeys, lambda k: seed_body(k, 4096), args.threads)
    # A few multipart objects (composite blobs).
    part = seed_body("multipart", 5 * MIB)
    for n in range(args.multipart):
        key = f"multipart/obj-{n:03d}"
        up = s3.create_multipart_upload(Bucket=args.bucket, Key=key)
        parts = []
        for pn in range(1, 4):
            r = s3.upload_part(Bucket=args.bucket, Key=key, UploadId=up["UploadId"],
                               PartNumber=pn, Body=part)
            parts.append({"PartNumber": pn, "ETag": r["ETag"]})
        s3.complete_multipart_upload(Bucket=args.bucket, Key=key, UploadId=up["UploadId"],
                                     MultipartUpload={"Parts": parts})
    elapsed = time.monotonic() - start
    total = args.objects + len(vkeys) * 3 + args.multipart
    write_result({"objects": args.objects, "versions": len(vkeys) * 3,
                  "multipart_objects": args.multipart, "total_writes": total,
                  "bytes": sum(sizes.values()) + len(vkeys) * 3 * 4096
                  + args.multipart * 3 * 5 * MIB,
                  "seconds": round(elapsed, 3),
                  "writes_per_s": round(total / elapsed, 1) if elapsed else None}, args.out)
    return 0


# --- maintenance ---------------------------------------------------------------

def maintenance_steps(variant: str, bucket: str) -> list[tuple[str, list[str]]]:
    """The offline commands timed by the maintenance benchmark, in order.

    `recover` rebuilds the database from the sidecars, so it runs last: the
    data volume is thrown away after the run.
    """
    steps = [
        ("fsck", ["fsck", "--grace-seconds", "0"]),
        ("fsck --verify-checksums", ["fsck", "--verify-checksums", "--grace-seconds", "0"]),
        ("gc (dry run)", ["gc", "--grace-seconds", "0"]),
    ]
    if variant == "plain":
        # Encrypted blobs are skipped by the offline rewrite tools, and the
        # compress variant has compressed every blob already.
        steps.append(("compress-existing", ["compress-existing", "--bucket", bucket,
                                            "--algorithm", "zstd"]))
    steps += [
        ("recover --dry-run", ["recover", "--dry-run"]),
        ("recover --skip-verify", ["recover", "--skip-verify"]),
    ]
    return steps


def run_timed(argv: list[str]) -> dict:
    """Run a command; wall time, its own peak RSS, exit code and output tail."""
    start = time.monotonic()
    proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    output = proc.stdout.read()
    _, status, usage = os.wait4(proc.pid, 0)
    seconds = time.monotonic() - start
    lines = output.decode(errors="replace").splitlines()
    return {"seconds": round(seconds, 3), "rc": os.waitstatus_to_exitcode(status),
            # ru_maxrss is in KiB on Linux.
            "max_rss_mib": round(usage.ru_maxrss / 1024, 1),
            "output_tail": lines[-15:]}


def count_sidecars(data_dir: str) -> int:
    """Objects in the store, versions and parts included: one sidecar each."""
    return sum(1 for _root, _dirs, files in os.walk(Path(data_dir) / "blobs")
               for f in files if f.endswith(".meta"))


def cmd_maintenance(args) -> int:
    objects = args.objects or count_sidecars(args.data)
    results = []
    for name, cmd in maintenance_steps(args.variant, args.bucket):
        r = run_timed([args.arca, *cmd, "--config-path", args.config])
        r["name"] = name
        r["command"] = "arca " + " ".join(cmd)
        if objects and r["seconds"] > 0:
            r["objects_per_s"] = round(objects / r["seconds"], 1)
        results.append(r)
        print(f"  {name}: {r['seconds']:.2f}s rc={r['rc']}", file=sys.stderr)
    write_result({"variant": args.variant, "objects": objects, "steps": results}, args.out)
    return 0 if all(r["rc"] == 0 for r in results) else 1


# --- report --------------------------------------------------------------------

def cmd_report(args) -> int:
    import report
    return report.main([args.run_dir])


def parse_args(argv: list[str]):
    p = argparse.ArgumentParser(description="Arca performance suite runner")
    sub = p.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("wait")
    s.add_argument("--timeout", type=int, default=60)
    s.set_defaults(fn=cmd_wait)

    s = sub.add_parser("prepare-bucket")
    s.add_argument("--bucket", required=True)
    s.add_argument("--compression", help="Arca bucket compression algorithm")
    s.set_defaults(fn=cmd_prepare_bucket)

    s = sub.add_parser("metrics")
    s.add_argument("--out", required=True)
    s.set_defaults(fn=cmd_metrics)

    s = sub.add_parser("listing")
    s.add_argument("--bucket", default="perf-listing")
    s.add_argument("--objects", type=int, default=10000)
    s.add_argument("--threads", type=int, default=32)
    s.add_argument("--repeat", type=int, default=3)
    s.add_argument("--out")
    s.set_defaults(fn=cmd_listing)

    s = sub.add_parser("seed")
    s.add_argument("--bucket", default="perf-maint")
    s.add_argument("--versioned-bucket", default="perf-maint-versioned")
    s.add_argument("--objects", type=int, default=20000)
    s.add_argument("--multipart", type=int, default=4)
    s.add_argument("--threads", type=int, default=32)
    s.add_argument("--out")
    s.set_defaults(fn=cmd_seed)

    s = sub.add_parser("maintenance")
    s.add_argument("--variant", required=True)
    s.add_argument("--bucket", default="perf-maint")
    s.add_argument("--objects", type=int, default=0,
                   help="objects in the store, for the objects/s figures "
                        "(default: count the sidecars under --data)")
    s.add_argument("--data", default="/data")
    s.add_argument("--arca", default="/usr/local/bin/arca")
    s.add_argument("--config", default="/etc/arca/config.toml")
    s.add_argument("--out")
    s.set_defaults(fn=cmd_maintenance)

    s = sub.add_parser("report")
    s.add_argument("run_dir")
    s.set_defaults(fn=cmd_report)
    return p.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    return args.fn(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
