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


def categorize_test(name: str) -> str:
    """Infer category from test function name."""
    name_lower = name.lower()
    if any(k in name_lower for k in ("bucket_list", "bucket_create", "bucket_delete",
                                      "bucket_head", "bucket_acl", "get_bucket",
                                      "put_bucket", "head_bucket", "delete_bucket",
                                      "list_buckets")):
        return "Bucket"
    if any(k in name_lower for k in ("multipart", "upload_part", "complete_upload",
                                      "abort_upload", "list_upload")):
        return "Multipart"
    if any(k in name_lower for k in ("list_object", "list_under", "list_marker",
                                      "list_delimiter", "list_prefix", "list_maxkeys",
                                      "list_continuation", "versions")):
        return "List"
    if any(k in name_lower for k in ("copy_object", "copy_source", "copy_dest",
                                      "copy_if", "copy_meta")):
        return "Copy"
    if any(k in name_lower for k in ("object_acl", "bucket_policy", "acl", "policy",
                                      "grant", "permission", "public")):
        return "ACL/Policy"
    if any(k in name_lower for k in ("cors", "option")):
        return "CORS"
    if any(k in name_lower for k in ("lifecycle", "expir")):
        return "Lifecycle"
    if any(k in name_lower for k in ("versioning", "version_")):
        return "Versioning"
    if any(k in name_lower for k in ("encrypt", "sse", "kms")):
        return "Encryption"
    if any(k in name_lower for k in ("tagging", "tag_")):
        return "Tagging"
    if any(k in name_lower for k in ("lock", "retention", "legal_hold",
                                      "governance", "compliance")):
        return "Object Lock"
    if any(k in name_lower for k in ("notification", "event")):
        return "Notifications"
    if any(k in name_lower for k in ("select", "sql")):
        return "S3 Select"
    if any(k in name_lower for k in ("presign", "post_object", "post_")):
        return "Presigned/POST"
    if any(k in name_lower for k in ("get_object", "put_object", "head_object",
                                      "delete_object", "object_read", "object_write",
                                      "set_content", "get_content", "range",
                                      "ifmatch", "ifnone", "if_match",
                                      "content_type", "content_length",
                                      "content_md5", "etag", "metadata")):
        return "Object"
    if any(k in name_lower for k in ("auth", "sigv4", "sigv2", "credential",
                                      "anonymous", "signed")):
        return "Auth"
    if any(k in name_lower for k in ("header", "request_id", "date", "server")):
        return "Headers"
    if any(k in name_lower for k in ("sts", "iam", "assume_role", "session",
                                      "web_identity")):
        return "STS/IAM"
    return "Other"


# Known-unimplemented features (tests for these are expected to fail)
EXPECTED_FAIL_CATEGORIES = {
    "ACL/Policy", "CORS", "Lifecycle", "Versioning", "Encryption",
    "Tagging", "Object Lock", "Notifications", "S3 Select",
    "Presigned/POST", "STS/IAM",
}


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

            tests.append({
                "name": name,
                "classname": classname,
                "status": status,
                "message": message[:200],  # truncate long messages
                "time": time_val,
                "category": category,
                "expected_fail": expected_fail,
            })

    return tests


def terminal_summary(tests: list):
    """Print a colored terminal summary."""
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


def generate_html(tests: list, output_path: str, old_passlist: set | None = None):
    """Generate a self-contained HTML dashboard."""
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

    # Category breakdown
    cats = defaultdict(lambda: {"passed": 0, "failed": 0, "error": 0, "skipped": 0, "total": 0})
    for t in tests:
        cats[t["category"]][t["status"]] += 1
        cats[t["category"]]["total"] += 1

    # Diff against old passlist
    current_passing = {t["name"] for t in tests if t["status"] == "passed"}
    newly_passing = current_passing - old_passlist if old_passlist else set()
    newly_failing = old_passlist - current_passing if old_passlist else set()

    # Build failure rows
    fail_rows = ""
    for t in sorted(tests, key=lambda x: (x["expected_fail"], x["name"])):
        if t["status"] not in ("failed", "error"):
            continue
        exp_badge = ('<span class="badge expected">Expected</span>'
                     if t["expected_fail"]
                     else '<span class="badge unexpected">Unexpected</span>')
        msg_escaped = (t["message"]
                       .replace("&", "&amp;")
                       .replace("<", "&lt;")
                       .replace(">", "&gt;")
                       .replace('"', "&quot;"))
        fail_rows += f"""
        <details class="fail-item">
          <summary>
            {exp_badge}
            <span class="cat-tag">{t["category"]}</span>
            <code>{t["name"]}</code>
          </summary>
          <pre>{msg_escaped}</pre>
        </details>"""

    # Category rows
    cat_rows = ""
    for cat in sorted(cats.keys()):
        c = cats[cat]
        p, f, e, s, tt = c["passed"], c["failed"], c["error"], c["skipped"], c["total"]
        rate = (p / tt * 100) if tt > 0 else 0
        rate_class = "good" if rate >= 80 else ("warn" if rate >= 40 else "bad")
        cat_rows += f"""
        <tr>
          <td>{cat}</td>
          <td class="num">{tt}</td>
          <td class="num passed">{p}</td>
          <td class="num failed">{f + e}</td>
          <td class="num skipped">{s}</td>
          <td class="num {rate_class}">{rate:.0f}%</td>
        </tr>"""

    # Diff rows
    diff_section = ""
    if newly_passing or newly_failing:
        diff_items = ""
        for name in sorted(newly_passing):
            diff_items += f'<div class="diff-new">+ {name}</div>\n'
        for name in sorted(newly_failing):
            diff_items += f'<div class="diff-lost">- {name}</div>\n'
        diff_section = f"""
        <h2>Passlist Diff</h2>
        <div class="diff-box">{diff_items}</div>"""

    html = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Arca S3 Compatibility Report</title>
<style>
  :root {{
    --bg: #1a1b26; --surface: #24283b; --border: #414868;
    --text: #c0caf5; --text-dim: #565f89; --accent: #7aa2f7;
    --green: #9ece6a; --red: #f7768e; --yellow: #e0af68; --cyan: #7dcfff;
  }}
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, monospace;
    background: var(--bg); color: var(--text); padding: 2rem;
    max-width: 1200px; margin: 0 auto;
  }}
  h1 {{ color: var(--accent); margin-bottom: 0.25rem; }}
  h2 {{ color: var(--text); margin: 2rem 0 1rem; font-size: 1.2rem; }}
  .meta {{ color: var(--text-dim); margin-bottom: 2rem; font-size: 0.85rem; }}
  .cards {{
    display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
    gap: 1rem; margin-bottom: 2rem;
  }}
  .card {{
    background: var(--surface); border: 1px solid var(--border);
    border-radius: 8px; padding: 1rem; text-align: center;
  }}
  .card .value {{ font-size: 2rem; font-weight: 700; }}
  .card .label {{ color: var(--text-dim); font-size: 0.8rem; text-transform: uppercase; }}
  .card.passed .value {{ color: var(--green); }}
  .card.failed .value {{ color: var(--red); }}
  .card.skipped .value {{ color: var(--yellow); }}
  .card.expected .value {{ color: var(--text-dim); }}
  .card.rate .value {{ color: var(--cyan); }}

  .progress-bar {{
    background: var(--surface); border-radius: 8px; overflow: hidden;
    height: 24px; margin-bottom: 2rem; border: 1px solid var(--border);
  }}
  .progress-fill {{
    height: 100%; transition: width 0.3s;
    background: linear-gradient(90deg, var(--green), var(--cyan));
  }}

  table {{
    width: 100%; border-collapse: collapse;
    background: var(--surface); border-radius: 8px; overflow: hidden;
  }}
  th, td {{ padding: 0.6rem 1rem; text-align: left; border-bottom: 1px solid var(--border); }}
  th {{ color: var(--accent); font-weight: 600; font-size: 0.85rem; text-transform: uppercase; }}
  .num {{ text-align: right; font-variant-numeric: tabular-nums; }}
  .passed {{ color: var(--green); }}
  .failed {{ color: var(--red); }}
  .skipped {{ color: var(--yellow); }}
  .good {{ color: var(--green); }}
  .warn {{ color: var(--yellow); }}
  .bad {{ color: var(--red); }}

  .fail-item {{
    background: var(--surface); border: 1px solid var(--border);
    border-radius: 6px; margin-bottom: 0.5rem; overflow: hidden;
  }}
  .fail-item summary {{
    padding: 0.5rem 1rem; cursor: pointer; display: flex;
    align-items: center; gap: 0.5rem; font-size: 0.9rem;
  }}
  .fail-item summary:hover {{ background: rgba(255,255,255,0.03); }}
  .fail-item pre {{
    padding: 0.75rem 1rem; background: var(--bg);
    font-size: 0.8rem; color: var(--text-dim); white-space: pre-wrap;
    word-break: break-all; border-top: 1px solid var(--border);
  }}
  .badge {{
    display: inline-block; padding: 0.15rem 0.5rem; border-radius: 4px;
    font-size: 0.7rem; font-weight: 600; text-transform: uppercase;
  }}
  .badge.expected {{ background: rgba(86,95,137,0.3); color: var(--text-dim); }}
  .badge.unexpected {{ background: rgba(247,118,142,0.2); color: var(--red); }}
  .cat-tag {{
    display: inline-block; padding: 0.1rem 0.4rem; border-radius: 3px;
    font-size: 0.7rem; background: rgba(122,162,247,0.15); color: var(--accent);
  }}
  code {{ font-size: 0.85rem; }}

  .diff-box {{ background: var(--surface); border-radius: 8px; padding: 1rem;
               border: 1px solid var(--border); font-family: monospace; font-size: 0.85rem; }}
  .diff-new {{ color: var(--green); }}
  .diff-lost {{ color: var(--red); }}
</style>
</head>
<body>

<h1>Arca S3 Compatibility Report</h1>
<p class="meta">{timestamp}</p>

<div class="cards">
  <div class="card"><div class="value">{total}</div><div class="label">Total</div></div>
  <div class="card passed"><div class="value">{passed}</div><div class="label">Passed</div></div>
  <div class="card failed"><div class="value">{failed + errors}</div><div class="label">Failed</div></div>
  <div class="card skipped"><div class="value">{skipped}</div><div class="label">Skipped</div></div>
  <div class="card expected"><div class="value">{expected_fail}</div><div class="label">Expected Fail</div></div>
  <div class="card rate"><div class="value">{pass_rate:.1f}%</div><div class="label">Pass Rate</div></div>
</div>

<div class="progress-bar">
  <div class="progress-fill" style="width: {pass_rate}%"></div>
</div>

<h2>Category Breakdown</h2>
<table>
  <tr><th>Category</th><th class="num">Total</th><th class="num">Passed</th>
      <th class="num">Failed</th><th class="num">Skipped</th><th class="num">Rate</th></tr>
  {cat_rows}
</table>

{diff_section}

<h2>Failures ({failed + errors})</h2>
{fail_rows if fail_rows else '<p style="color: var(--green)">No failures!</p>'}

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

    tests = parse_junit_xml(results_xml)

    # Load old passlist for diff
    old_passlist = load_passlist(passlist_path) if passlist_path else None

    # Generate outputs
    terminal_summary(tests)
    generate_html(tests, report_html, old_passlist)
    print(f"HTML report written to: {report_html}")

    # Generate SVG badge alongside the HTML report
    badge_path = str(Path(report_html).parent / "s3-compatibility-badge.svg")
    generate_badge_svg(tests, badge_path)
    print(f"SVG badge written to: {badge_path}")

    if passlist_path:
        generate_passlist(tests, passlist_path)
        passing_count = sum(1 for t in tests if t["status"] == "passed")
        print(f"Passlist written to: {passlist_path} ({passing_count} tests)")


if __name__ == "__main__":
    main()
