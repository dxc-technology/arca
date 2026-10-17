#!/usr/bin/env python3
"""
S3 Compatibility Report Generator for Arca.

Parses JUnit XML output from pytest and generates:
  - A self-contained HTML dashboard
  - A terminal summary with ANSI colors
  - An updated passlist of passing tests

Usage:
    python report.py results.xml report.html [passlist.txt]
"""

import sys
import xml.etree.ElementTree as ET
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path


# Explicit overrides for tests whose names don't indicate their true category
CATEGORY_OVERRIDES = {
    "test_100_continue": "ACL/Policy",
    "test_object_raw_get": "Anonymous",
    "test_object_raw_get_bucket_gone": "Anonymous",
    "test_object_raw_get_object_gone": "Anonymous",
    "test_object_delete_key_bucket_gone": "Anonymous",
    "test_bucket_list_objects_anonymous": "Anonymous",
    "test_bucket_listv2_objects_anonymous": "Anonymous",
    "test_list_buckets_anonymous": "Anonymous",
    "test_list_multipart_upload_owner": "ACL/Policy",
    "test_bucket_create_exists_nonowner": "ACL/Policy",
    "test_object_copy_not_owned_bucket": "ACL/Policy",
    "test_object_copy_not_owned_object_bucket": "ACL/Policy",
    "test_bucket_list_return_data": "ACL/Policy",
    "test_object_read_unreadable": "ACL/Policy",
    "test_put_obj_enc_conflict_c_s3": "Encryption",
    "test_lifecycle_multipart_expiration": "Lifecycle",
    # RGW-specific extensions (not AWS S3 API)
    "test_bucket_head_extended": "RGW Extensions",
    "test_head_bucket_usage": "RGW Extensions",
    "test_account_usage": "RGW Extensions",
    "test_bucket_list_unordered": "RGW Extensions",
    "test_bucket_listv2_unordered": "RGW Extensions",
    # Not-in-MVP features
    "test_get_object_torrent": "Not Implemented",
    "test_list_buckets_paginated": "Not Implemented",
    "test_get_object_attributes": "Not Implemented",
    "test_get_multipart_object_attributes": "Not Implemented",
    "test_get_paginated_multipart_object_attributes": "Not Implemented",
    "test_get_single_multipart_object_attributes": "Not Implemented",
    "test_multipart_get_part": "Not Implemented",
    "test_multipart_single_get_part": "Not Implemented",
    "test_non_multipart_get_part": "Not Implemented",
}


def categorize_test(name: str) -> str:
    """Infer category from test function name.

    Unimplemented-feature categories are checked first to prevent tests
    from being miscategorized under generic 'Bucket', 'Object', or
    'Multipart' when they actually test unimplemented features.
    """
    if name in CATEGORY_OVERRIDES:
        return CATEGORY_OVERRIDES[name]

    name_lower = name.lower()

    # --- Unimplemented features (checked first) ---

    # Versioning — before List and Bucket/Object
    if any(k in name_lower for k in ("versioning", "versioned", "version_id",
                                      "version_", "delete_marker", "_current_")):
        return "Versioning"

    # Logging — before Bucket/Object/Copy (logging tests call PutBucketPolicy)
    if "logging" in name_lower:
        return "Logging"

    # Presigned/POST — before ACL/Policy (POST tests have "policy" for signing)
    if any(k in name_lower for k in ("presign", "post_object", "post_",
                                      "x_amz_expires")):
        return "Presigned/POST"

    # ACL/Policy — before Bucket/Object
    if any(k in name_lower for k in ("_acl", "policy", "grant", "permission",
                                      "public_access", "publicread",
                                      "publicreadwrite", "block_public",
                                      "public_block", "ownership",
                                      "owner_enforced", "owner_preferred",
                                      "object_writer", "access_bucket",
                                      "bucket_owner")):
        return "ACL/Policy"

    # Checksums — before Multipart/Object
    if any(k in name_lower for k in ("checksum", "cksum")):
        return "Checksums"

    # Encryption — before Multipart/Object
    if any(k in name_lower for k in ("encrypt", "sse_", "sse-", "kms")):
        return "Encryption"

    # Tagging — before Object
    if any(k in name_lower for k in ("tagging", "_tags")):
        return "Tagging"

    # Object Lock — before Object
    if any(k in name_lower for k in ("object_lock", "retention", "legal_hold",
                                      "governance", "compliance")):
        return "Object Lock"

    # Anonymous access
    if any(k in name_lower for k in ("anonymous", "_anon_")):
        return "Anonymous"

    if "cors" in name_lower:
        return "CORS"

    if "lifecycle" in name_lower:
        return "Lifecycle"

    if any(k in name_lower for k in ("notification", "event")):
        return "Notifications"

    if any(k in name_lower for k in ("select", "sql")):
        return "S3 Select"

    if any(k in name_lower for k in ("assume_role", "web_identity",
                                      "session_token")):
        return "STS/IAM"

    # --- Implemented features ---

    if any(k in name_lower for k in ("bucket_list", "bucket_create", "bucket_delete",
                                      "bucket_head", "get_bucket",
                                      "put_bucket", "head_bucket", "delete_bucket",
                                      "list_buckets")):
        return "Bucket"

    if any(k in name_lower for k in ("multipart", "upload_part", "complete_upload",
                                      "abort_upload", "list_upload")):
        return "Multipart"

    if any(k in name_lower for k in ("list_object", "list_under", "list_marker",
                                      "list_delimiter", "list_prefix", "list_maxkeys",
                                      "list_continuation")):
        return "List"

    if any(k in name_lower for k in ("copy_object", "object_copy", "copy_source",
                                      "copy_dest", "copy_if", "copy_meta",
                                      "copy_enc", "copy_part")):
        return "Copy"

    if any(k in name_lower for k in ("get_object", "put_object", "head_object",
                                      "delete_object", "object_read", "object_write",
                                      "set_content", "get_content", "range",
                                      "ifmatch", "ifnone", "if_match",
                                      "content_type", "content_length",
                                      "content_md5", "etag", "metadata")):
        return "Object"

    if any(k in name_lower for k in ("auth", "sigv4", "sigv2", "credential",
                                      "signed")):
        return "Auth"

    if any(k in name_lower for k in ("header", "request_id", "date", "server")):
        return "Headers"

    return "Other"


# Known-unimplemented features (tests for these are expected to fail)
EXPECTED_FAIL_CATEGORIES = {
    "ACL/Policy", "CORS", "Lifecycle", "Versioning", "Encryption",
    "Tagging", "Object Lock", "Notifications", "S3 Select",
    "Presigned/POST", "STS/IAM", "Logging", "Checksums", "Anonymous",
    "RGW Extensions", "Not Implemented",
}


# Ceph/RGW-specific tests that are NOT part of the AWS S3 API (RGW extensions:
# x-rgw-* headers on HeadBucket, ?list-type=unordered, account usage). They can
# never pass against an AWS-compatible server, so they are excluded entirely from
# the compatibility denominator rather than counted as failures.
OUT_OF_SCOPE_CATEGORIES = {"RGW Extensions"}


# Tests where Ceph s3-tests expects RGW's answer and Arca deliberately gives
# AWS's. Such a test counts as passed ONLY when it fails on exactly the listed
# assertion, i.e. after every earlier check in it has passed; any other failure
# in it (or an error) stays a real failure. Keep each entry tied to the AWS
# behaviour it documents.
AWS_DIVERGENCES = {
    "test_object_checksum_sha256": {
        "assertion": "assert 'InvalidRequest' == 'BadDigest'",
        "reason": "A malformed x-amz-checksum-sha256 value ('bad' is not a base64 "
                  "SHA-256) is rejected with 400 InvalidRequest on AWS; RGW answers "
                  "BadDigest, which AWS reserves for a well-formed checksum that does "
                  "not match the body (TD-034).",
    },
    "test_object_checksum_crc64nvme": {
        "assertion": "assert 'InvalidRequest' == 'BadDigest'",
        "reason": "A malformed x-amz-checksum-crc64nvme value ('bad' is not a base64 "
                  "CRC64NVME) is rejected with 400 InvalidRequest on AWS; RGW answers "
                  "BadDigest, which AWS reserves for a well-formed checksum that does "
                  "not match the body (TD-034).",
    },
}


def aws_divergence(name: str, status: str, message: str) -> str | None:
    """The documented AWS divergence explaining this failure, if any."""
    divergence = AWS_DIVERGENCES.get(name)
    if status == "failed" and divergence and divergence["assertion"] in message:
        return divergence["reason"]
    return None


def parse_junit_xml(path: str):
    """Parse JUnit XML and return list of test results."""
    tree = ET.parse(path)
    root = tree.getroot()

    tests = []
    for suite in root.iter("testsuite"):
        for tc in suite.iter("testcase"):
            name = tc.get("name", "unknown")
            classname = tc.get("classname", "")
            time_val = float(tc.get("time", "0"))

            failure = tc.find("failure")
            error = tc.find("error")
            skipped = tc.find("skipped")

            if skipped is not None:
                status = "skipped"
                message = skipped.get("message", "")
            elif failure is not None:
                status = "failed"
                message = failure.get("message", "")
            elif error is not None:
                status = "error"
                message = error.get("message", "")
            else:
                status = "passed"
                message = ""

            category = categorize_test(name)
            expected_fail = category in EXPECTED_FAIL_CATEGORIES
            divergence = aws_divergence(name, status, message)
            if divergence:
                status = "passed"

            tests.append({
                "name": name,
                "classname": classname,
                "status": status,
                "message": message[:200],  # truncate long messages
                "time": time_val,
                "category": category,
                "expected_fail": expected_fail,
                "aws_divergence": divergence,
            })

    return tests


def terminal_summary(tests: list, out_of_scope_n: int = 0):
    """Print a colored terminal summary (over in-scope tests only)."""
    total = len(tests)
    passed = sum(1 for t in tests if t["status"] == "passed")
    failed = sum(1 for t in tests if t["status"] == "failed")
    errors = sum(1 for t in tests if t["status"] == "error")
    skipped = sum(1 for t in tests if t["status"] == "skipped")
    unexpected = sum(1 for t in tests
                     if t["status"] in ("failed", "error") and not t["expected_fail"])

    GREEN = "\033[32m"
    RED = "\033[31m"
    YELLOW = "\033[33m"
    CYAN = "\033[36m"
    BOLD = "\033[1m"
    RESET = "\033[0m"

    print(f"\n{BOLD}{'='*60}{RESET}")
    print(f"{BOLD}  Arca S3 Compatibility Report{RESET}")
    print(f"{'='*60}")
    print(f"  Total:    {total}")
    print(f"  {GREEN}Passed:   {passed}{RESET}")
    print(f"  {RED}Failed:   {failed}{RESET}")
    print(f"  {RED}Errors:   {errors}{RESET}")
    print(f"  {YELLOW}Skipped:  {skipped}{RESET}")
    print(f"  {CYAN}Pass rate: {passed/total*100:.1f}%{RESET}" if total > 0 else "")
    if out_of_scope_n:
        print(f"  {YELLOW}({out_of_scope_n} Ceph/RGW-specific tests excluded from scope){RESET}")
    divergent = [t["name"] for t in tests if t["aws_divergence"]]
    if divergent:
        print(f"  {CYAN}({len(divergent)} counted as passed: AWS behaviour where RGW differs){RESET}")
    print()

    if unexpected > 0:
        print(f"  {RED}{BOLD}Unexpected failures: {unexpected}{RESET}")
        for t in tests:
            if t["status"] in ("failed", "error") and not t["expected_fail"]:
                msg = t["message"][:80] if t["message"] else ""
                print(f"    {RED}- {t['name']}{RESET}")
                if msg:
                    print(f"      {msg}")
        print()

    # Category breakdown
    cats = defaultdict(lambda: {"passed": 0, "failed": 0, "error": 0, "skipped": 0})
    for t in tests:
        cats[t["category"]][t["status"]] += 1

    print(f"  {BOLD}Category breakdown:{RESET}")
    print(f"  {'Category':<16} {'Pass':>5} {'Fail':>5} {'Error':>5} {'Skip':>5}")
    print(f"  {'-'*42}")
    for cat in sorted(cats.keys()):
        c = cats[cat]
        p, f, e, s = c["passed"], c["failed"], c["error"], c["skipped"]
        print(f"  {cat:<16} {p:>5} {f:>5} {e:>5} {s:>5}")

    print(f"{'='*60}\n")


def _esc(text: str) -> str:
    """Escape text for HTML element and attribute content."""
    return (text.replace("&", "&amp;").replace("<", "&lt;")
            .replace(">", "&gt;").replace('"', "&quot;"))


REPORT_CSS = """
  :root {
    color-scheme: dark;
    --bg: #070a12; --panel: #0d121c; --panel-2: #121a28;
    --line: #1c2638; --line-2: #2a3850;
    --text: #e8ecf2; --text-2: #93a1b6; --text-3: #6a7a92;
    --cyan: #00d4ff; --violet: #a78bfa;
    --pass: #34d399; --fail: #f472b6; --skip: #ffb347;
    --pass-bg: rgba(52,211,153,.14); --fail-bg: rgba(244,114,182,.14);
    --skip-bg: rgba(255,179,71,.14); --cyan-bg: rgba(0,212,255,.12);
    --glow: rgba(0,212,255,.07);
    --serif: 'Iowan Old Style', 'Palatino Linotype', Palatino, Georgia, serif;
    --sans: system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif;
    --mono: ui-monospace, 'SF Mono', 'JetBrains Mono', Menlo, Consolas, monospace;
  }
  @media (prefers-color-scheme: light) {
    :root {
      color-scheme: light;
      --bg: #f4f6fb; --panel: #ffffff; --panel-2: #f0f3f9;
      --line: #e1e6f0; --line-2: #c8d1e1;
      --text: #0f172a; --text-2: #475569; --text-3: #64748b;
      --cyan: #0369a1; --violet: #6d4fd6;
      --pass: #047857; --fail: #be185d; --skip: #b45309;
      --pass-bg: rgba(4,120,87,.10); --fail-bg: rgba(190,24,93,.09);
      --skip-bg: rgba(180,83,9,.10); --cyan-bg: rgba(3,105,161,.09);
      --glow: rgba(3,105,161,.06);
    }
  }
  *, *::before, *::after { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    font-family: var(--sans); background: var(--bg); color: var(--text);
    line-height: 1.6; -webkit-font-smoothing: antialiased;
    background-image: radial-gradient(ellipse 80% 50% at 85% -10%, var(--glow), transparent 70%);
    background-repeat: no-repeat;
  }
  .wrap { max-width: 1080px; margin: 0 auto; padding: 0 20px 4rem; }
  a { color: var(--cyan); }
  code, .mono { font-family: var(--mono); }
  :focus-visible { outline: 2px solid var(--cyan); outline-offset: 2px; }

  header { padding: 4.5rem 0 2.5rem; }
  .brand { font-family: var(--mono); font-size: .8rem; color: var(--text-3); margin-bottom: 1.25rem; }
  .brand b { color: var(--cyan); font-weight: 600; }
  h1 { font-family: var(--serif); font-weight: 400; font-size: clamp(2rem, 5vw, 3.2rem);
       line-height: 1.12; max-width: 24ch; }
  .lede { color: var(--text-2); max-width: 62ch; margin-top: 1rem; font-size: 1.02rem; }
  .stamp { font-family: var(--mono); font-size: .75rem; color: var(--text-3); margin-top: 1.25rem; }

  .score { display: flex; align-items: baseline; gap: 1rem; flex-wrap: wrap; margin: 2.5rem 0 1rem; }
  .score .big { font-family: var(--serif); font-size: clamp(3.6rem, 12vw, 6.5rem); line-height: 1;
                letter-spacing: -.02em; }
  .score .big small { font-size: .45em; color: var(--text-3); margin-left: .05em; }
  .score .of { color: var(--text-2); font-size: 1.05rem; }
  .score .of b { color: var(--text); font-weight: 600; }

  .stack { display: flex; height: 18px; border-radius: 9px; overflow: hidden; gap: 2px;
           background: var(--line); }
  .stack > span { display: block; min-width: 3px; }
  .stack .p { background: var(--pass); } .stack .f { background: var(--fail); }
  .stack .s { background: var(--skip); }
  .legend { display: flex; flex-wrap: wrap; gap: .4rem 1.4rem; margin-top: .8rem;
            font-size: .82rem; color: var(--text-2); }
  .legend i { display: inline-block; width: 9px; height: 9px; border-radius: 3px;
              margin-right: .45rem; }

  .stats { display: grid; grid-template-columns: repeat(2, 1fr); gap: 1px; margin: 2.5rem 0 0;
           background: var(--line); border: 1px solid var(--line); border-radius: 12px; overflow: hidden; }
  .stat:last-child { grid-column: span 2; }
  @media (min-width: 720px) { .stats { grid-template-columns: repeat(5, 1fr); }
                              .stat:last-child { grid-column: auto; } }
  .stat { background: var(--panel); padding: 1.1rem 1.2rem; }
  .stat .v { font-family: var(--mono); font-size: 1.6rem; font-weight: 600; line-height: 1.2; }
  .stat .l { font-size: .78rem; color: var(--text-3); margin-top: .15rem; }
  .stat.pass .v { color: var(--pass); } .stat.fail .v { color: var(--fail); }
  .stat.skip .v { color: var(--skip); }

  section { margin-top: 3.5rem; }
  h2 { font-family: var(--serif); font-weight: 400; font-size: 1.7rem; line-height: 1.2; }
  h2 .count { font-family: var(--mono); font-size: .8rem; color: var(--text-3); margin-left: .6rem;
              vertical-align: middle; }
  .sub { color: var(--text-2); font-size: .92rem; max-width: 68ch; margin: .5rem 0 1.4rem; }

  .cats { background: var(--panel); border: 1px solid var(--line); border-radius: 12px; overflow: hidden; }
  .cat { display: grid; grid-template-columns: minmax(0, 1fr) 3.2rem; gap: .4rem 1rem;
         align-items: center; padding: .7rem 1.1rem; border-top: 1px solid var(--line); }
  .cat:first-child { border-top: 0; }
  .cat .name { font-weight: 500; font-size: .92rem; }
  .cat .rate { grid-row: 1; grid-column: 2; }
  .cat .stack { grid-column: 1 / -1; }
  .cat .counts { grid-column: 1 / -1; font-family: var(--mono); font-size: .72rem; color: var(--text-3); }
  .cat .counts .p { color: var(--pass); } .cat .counts .f { color: var(--fail); }
  .cat .counts .s { color: var(--skip); }
  .cat .stack { height: 8px; border-radius: 4px; gap: 1px; }
  .cat .rate { font-family: var(--mono); font-size: .85rem; text-align: right; font-weight: 600; }
  .rate.good { color: var(--pass); } .rate.warn { color: var(--skip); } .rate.bad { color: var(--fail); }
  @media (min-width: 720px) {
    .cat { grid-template-columns: 9rem 1fr 19rem 3.2rem; }
    .cat .counts { white-space: nowrap; }
    .cat .name, .cat .rate, .cat .stack, .cat .counts { grid-row: auto; grid-column: auto; }
    .cat .counts { text-align: right; }
  }

  .item { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; margin-bottom: .45rem;
          overflow: hidden; }
  .item[open] { border-color: var(--line-2); }
  .item summary { cursor: pointer; padding: .55rem .9rem; display: flex; align-items: center;
                  flex-wrap: wrap; gap: .3rem .7rem; font-size: .86rem; list-style: none; }
  .item summary::-webkit-details-marker { display: none; }
  .item summary::before { content: ''; width: 6px; height: 6px; flex: none; border-right: 1.5px solid var(--text-3);
                          border-bottom: 1.5px solid var(--text-3); transform: rotate(-45deg);
                          transition: transform .15s; }
  .item[open] > summary::before { transform: rotate(45deg); }
  .item summary:hover { background: var(--panel-2); }
  .item code { font-size: .8rem; overflow-wrap: anywhere; }
  .item pre { padding: .8rem 1rem; background: var(--bg); font-family: var(--mono); font-size: .76rem;
              color: var(--text-2); white-space: pre-wrap; overflow-wrap: anywhere;
              border-top: 1px solid var(--line); }
  .tag { font-size: .72rem; padding: .08rem .5rem; border-radius: 999px; background: var(--cyan-bg);
         color: var(--cyan); white-space: nowrap; }
  .tag.unexpected { background: var(--fail-bg); color: var(--fail); font-weight: 600; }
  .tag.expected { background: var(--panel-2); color: var(--text-3); }
  .group { margin-top: .9rem; }
  .group > summary { cursor: pointer; font-size: .9rem; color: var(--cyan); padding: .4rem 0; }
  .ok { background: var(--pass-bg); color: var(--pass); border-radius: 8px; padding: .8rem 1rem; }

  .diff { background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: .8rem 1rem;
          font-family: var(--mono); font-size: .8rem; overflow-wrap: anywhere; }
  .diff .new { color: var(--pass); } .diff .lost { color: var(--fail); }

  footer { margin-top: 4rem; padding-top: 1.2rem; border-top: 1px solid var(--line);
           font-size: .8rem; color: var(--text-3); }
"""


def _stack(c: dict, total: int) -> str:
    """Stacked passed/failed/skipped bar; widths are shares of `total`."""
    if total <= 0:
        return '<div class="stack"></div>'
    parts = ""
    for cls, label, n in (("p", "passed", c["passed"]),
                          ("f", "failed", c["failed"] + c["error"]),
                          ("s", "skipped", c["skipped"])):
        if n:
            parts += (f'<span class="{cls}" style="width:{n / total * 100:.2f}%" '
                      f'title="{n} {label}"></span>')
    return f'<div class="stack" role="img" aria-label="outcome distribution">{parts}</div>'


def generate_html(tests: list, output_path: str, old_passlist: set | None = None,
                  out_of_scope_n: int = 0):
    """Generate a self-contained HTML dashboard (over in-scope tests only)."""
    total = len(tests)
    passed = sum(1 for t in tests if t["status"] == "passed")
    failed = sum(1 for t in tests if t["status"] == "failed")
    errors = sum(1 for t in tests if t["status"] == "error")
    skipped = sum(1 for t in tests if t["status"] == "skipped")
    unexpected = sum(1 for t in tests
                     if t["status"] in ("failed", "error") and not t["expected_fail"])
    expected_fail = sum(1 for t in tests
                        if t["status"] in ("failed", "error") and t["expected_fail"])

    pass_rate = (passed / total * 100) if total > 0 else 0
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")
    scope_note = (f" {out_of_scope_n} Ceph/RGW-specific test(s) are excluded from scope."
                  if out_of_scope_n else "")

    # Category breakdown
    cats = defaultdict(lambda: {"passed": 0, "failed": 0, "error": 0, "skipped": 0, "total": 0})
    for t in tests:
        cats[t["category"]][t["status"]] += 1
        cats[t["category"]]["total"] += 1

    # Diff against old passlist
    current_passing = {t["name"] for t in tests if t["status"] == "passed"}
    newly_passing = current_passing - old_passlist if old_passlist else set()
    newly_failing = old_passlist - current_passing if old_passlist else set()

    # Failure rows: unexpected first, expected ones grouped and collapsed
    def fail_item(t):
        badge = ('<span class="tag expected">Expected</span>' if t["expected_fail"]
                 else '<span class="tag unexpected">Unexpected</span>')
        return f"""
        <details class="item">
          <summary>{badge}<span class="tag">{_esc(t["category"])}</span><code>{_esc(t["name"])}</code></summary>
          <pre>{_esc(t["message"])}</pre>
        </details>"""

    failing = sorted((t for t in tests if t["status"] in ("failed", "error")),
                     key=lambda x: (x["expected_fail"], x["name"]))
    unexpected_rows = "".join(fail_item(t) for t in failing if not t["expected_fail"])
    expected_rows = "".join(fail_item(t) for t in failing if t["expected_fail"])
    if not failing:
        fail_body = '<p class="ok">No failures.</p>'
    else:
        fail_body = unexpected_rows or '<p class="ok">No unexpected failures.</p>'
        if expected_rows:
            fail_body += f"""
        <details class="group">
          <summary>Show the {expected_fail} expected failures (features Arca does not implement yet)</summary>
          <div style="margin-top:.6rem">{expected_rows}</div>
        </details>"""

    # Category rows
    cat_rows = ""
    for cat in sorted(cats.keys()):
        c = cats[cat]
        p, f, e, s, tt = c["passed"], c["failed"], c["error"], c["skipped"], c["total"]
        rate = (p / tt * 100) if tt > 0 else 0
        rate_class = "good" if rate >= 80 else ("warn" if rate >= 40 else "bad")
        cat_rows += f"""
        <div class="cat">
          <div class="name">{_esc(cat)}</div>
          {_stack(c, tt)}
          <div class="counts"><span class="p">{p} passed</span> &nbsp;<span class="f">{f + e} failed</span> &nbsp;<span class="s">{s} skipped</span> &nbsp;of {tt}</div>
          <div class="rate {rate_class}">{rate:.0f}%</div>
        </div>"""

    # AWS divergences (counted as passed)
    divergence_section = ""
    divergent = sorted((t for t in tests if t["aws_divergence"]), key=lambda x: x["name"])
    if divergent:
        items = "".join(f"""
        <details class="item">
          <summary><span class="tag">{_esc(t["category"])}</span><code>{_esc(t["name"])}</code></summary>
          <pre>{_esc(t["aws_divergence"])}</pre>
        </details>""" for t in divergent)
        divergence_section = f"""
      <section>
        <h2>AWS divergences<span class="count">{len(divergent)} counted as passed</span></h2>
        <p class="sub">These tests fail only on their last assertion, where Ceph expects RGW's answer
        and Arca gives AWS's. Every earlier check in them passed.</p>
        {items}
      </section>"""

    # Diff rows
    diff_section = ""
    if newly_passing or newly_failing:
        diff_items = "".join(f'<div class="new">+ {_esc(n)}</div>\n' for n in sorted(newly_passing))
        diff_items += "".join(f'<div class="lost">- {_esc(n)}</div>\n' for n in sorted(newly_failing))
        diff_section = f"""
      <section>
        <h2>Passlist diff<span class="count">+{len(newly_passing)} / -{len(newly_failing)}</span></h2>
        <p class="sub">Changes against the previous passlist.</p>
        <div class="diff">{diff_items}</div>
      </section>"""

    summary_stack = _stack({"passed": passed, "failed": failed, "error": errors,
                            "skipped": skipped}, total)

    html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Arca S3 Compatibility Report</title>
<style>{REPORT_CSS}</style>
</head>
<body>
<div class="wrap">

<header>
  <div class="brand"><b>arca</b> / ceph s3-tests</div>
  <h1>S3 compatibility report</h1>
  <p class="lede">How Arca fares against the Ceph s3-tests suite, the reference conformance
  suite for S3-compatible servers.{scope_note}</p>
  <p class="stamp">{timestamp}</p>

  <div class="score">
    <div class="big">{pass_rate:.1f}<small>%</small></div>
    <div class="of"><b>{passed}</b> of {total} in-scope tests pass</div>
  </div>
  {summary_stack}
  <div class="legend">
    <span><i style="background:var(--pass)"></i>Passed</span>
    <span><i style="background:var(--fail)"></i>Failed</span>
    <span><i style="background:var(--skip)"></i>Skipped</span>
  </div>

  <div class="stats">
    <div class="stat"><div class="v">{total}</div><div class="l">Total</div></div>
    <div class="stat pass"><div class="v">{passed}</div><div class="l">Passed</div></div>
    <div class="stat fail"><div class="v">{failed + errors}</div><div class="l">Failed</div></div>
    <div class="stat skip"><div class="v">{skipped}</div><div class="l">Skipped</div></div>
    <div class="stat"><div class="v">{expected_fail}</div><div class="l">Expected failures</div></div>
  </div>
</header>

<main>
  <section>
    <h2>Category breakdown</h2>
    <p class="sub">Each bar splits a category into passed, failed and skipped tests.</p>
    <div class="cats">{cat_rows}
    </div>
  </section>
{divergence_section}
{diff_section}
  <section>
    <h2>Failures<span class="count">{failed + errors} total, {unexpected} unexpected</span></h2>
    <p class="sub">Click a test to see its assertion message.</p>
    {fail_body}
  </section>
</main>

<footer>
  Contains AI-generated code, see the <a href="https://github.com/dxc-technology/arca/blob/main/NOTICE">NOTICE</a> file.
</footer>
</div>
</body>
</html>"""

    Path(output_path).write_text(html, encoding="utf-8")


def generate_badge_svg(tests: list, output_path: str):
    """Generate an SVG progress badge for the README."""
    total = len(tests)
    passed = sum(1 for t in tests if t["status"] == "passed")
    rate = (passed / total * 100) if total > 0 else 0

    # Badge dimensions
    label_w = 130
    bar_w = 160
    stats_w = 100
    total_w = label_w + bar_w + stats_w
    h = 28
    r = 5  # corner radius

    # Progress bar geometry (with padding inside the bar area)
    bar_pad = 6
    bar_inner_w = bar_w - bar_pad * 2
    bar_inner_h = h - bar_pad * 2
    bar_fill_w = max(1, bar_inner_w * rate / 100)

    # Color: green if >= 50%, yellow if >= 25%, red otherwise
    if rate >= 50:
        bar_color = "#9ece6a"
    elif rate >= 25:
        bar_color = "#e0af68"
    else:
        bar_color = "#f7768e"

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{total_w}" height="{h}" role="img" aria-label="S3 Compatibility: {passed}/{total} ({rate:.0f}%)">
  <title>S3 Compatibility: {passed}/{total} ({rate:.0f}%)</title>
  <defs>
    <clipPath id="cr"><rect width="{total_w}" height="{h}" rx="{r}"/></clipPath>
  </defs>
  <g clip-path="url(#cr)">
    <!-- label background -->
    <rect width="{label_w}" height="{h}" fill="#24283b"/>
    <!-- bar background -->
    <rect x="{label_w}" width="{bar_w}" height="{h}" fill="#1a1b26"/>
    <!-- stats background -->
    <rect x="{label_w + bar_w}" width="{stats_w}" height="{h}" fill="#24283b"/>
    <!-- progress bar track -->
    <rect x="{label_w + bar_pad}" y="{bar_pad}" width="{bar_inner_w}" height="{bar_inner_h}" rx="3" fill="#414868"/>
    <!-- progress bar fill -->
    <rect x="{label_w + bar_pad}" y="{bar_pad}" width="{bar_fill_w:.1f}" height="{bar_inner_h}" rx="3" fill="{bar_color}"/>
  </g>
  <!-- label text -->
  <text x="{label_w / 2}" y="{h / 2 + 1}" fill="#c0caf5" font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif" font-size="11" font-weight="600" text-anchor="middle" dominant-baseline="middle">S3 Compatibility</text>
  <!-- stats text -->
  <text x="{label_w + bar_w + stats_w / 2}" y="{h / 2 + 1}" fill="{bar_color}" font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif" font-size="11" font-weight="700" text-anchor="middle" dominant-baseline="middle">{passed}/{total} · {rate:.0f}%</text>
</svg>
"""
    Path(output_path).write_text(svg, encoding="utf-8")


def generate_summary_json(tests: list, output_path: str, out_of_scope_n: int = 0):
    """Write a machine-readable JSON summary of in-scope test results."""
    import json

    total = len(tests)
    passed = sum(1 for t in tests if t["status"] == "passed")
    failed = sum(1 for t in tests if t["status"] == "failed")
    errors = sum(1 for t in tests if t["status"] == "error")
    skipped = sum(1 for t in tests if t["status"] == "skipped")
    unexpected = sum(1 for t in tests
                     if t["status"] in ("failed", "error") and not t["expected_fail"])
    pass_pct = round(passed / total * 100, 1) if total > 0 else 0

    # Category breakdown
    cats = defaultdict(lambda: {"passed": 0, "failed": 0, "error": 0, "skipped": 0, "total": 0})
    for t in tests:
        cats[t["category"]][t["status"]] += 1
        cats[t["category"]]["total"] += 1

    summary = {
        "timestamp": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "total": total,
        "passed": passed,
        "failed": failed + errors,
        "skipped": skipped,
        "unexpected_failures": unexpected,
        "pass_pct": pass_pct,
        "out_of_scope_excluded": out_of_scope_n,
        "aws_divergences": sorted(t["name"] for t in tests if t["aws_divergence"]),
        "categories": {
            cat: {
                "total": c["total"],
                "passed": c["passed"],
                "failed": c["failed"] + c["error"],
                "skipped": c["skipped"],
            }
            for cat, c in sorted(cats.items())
        },
    }

    Path(output_path).write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8"
    )


def generate_passlist(tests: list, output_path: str):
    """Write a passlist of all passing test names."""
    passing = sorted(t["name"] for t in tests if t["status"] == "passed")
    Path(output_path).write_text("\n".join(passing) + "\n" if passing else "", encoding="utf-8")


def load_passlist(path: str) -> set | None:
    """Load an existing passlist, returning None if not found."""
    p = Path(path)
    if not p.exists():
        return None
    lines = set()
    for line in p.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            lines.add(line)
    return lines if lines else None


def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <results.xml> <report.html> [passlist.txt]")
        sys.exit(1)

    results_xml = sys.argv[1]
    report_html = sys.argv[2]
    passlist_path = sys.argv[3] if len(sys.argv) > 3 else None

    all_tests = parse_junit_xml(results_xml)

    # Drop Ceph/RGW-specific tests entirely: they are not AWS S3 API and would
    # only drag the compatibility denominator down with failures we will never fix.
    tests = [t for t in all_tests if t["category"] not in OUT_OF_SCOPE_CATEGORIES]
    out_of_scope_n = len(all_tests) - len(tests)

    # Load old passlist for diff
    old_passlist = load_passlist(passlist_path) if passlist_path else None

    # Generate outputs
    terminal_summary(tests, out_of_scope_n)
    generate_html(tests, report_html, old_passlist, out_of_scope_n)
    print(f"HTML report written to: {report_html}")

    # Generate SVG badge alongside the HTML report
    badge_path = str(Path(report_html).parent / "s3-compatibility-badge.svg")
    generate_badge_svg(tests, badge_path)
    print(f"SVG badge written to: {badge_path}")

    # Generate JSON summary alongside the HTML report
    summary_path = str(Path(report_html).parent / "summary.json")
    generate_summary_json(tests, summary_path, out_of_scope_n)
    print(f"JSON summary written to: {summary_path}")

    if passlist_path:
        generate_passlist(tests, passlist_path)
        passing_count = sum(1 for t in tests if t["status"] == "passed")
        print(f"Passlist written to: {passlist_path} ({passing_count} tests)")


if __name__ == "__main__":
    main()
