#!/usr/bin/env python3
"""
Performance report generator for the Arca performance suite.

Reads a run directory written by `bin/perf-test suite` and generates:
  - summary.json  machine-readable results (every figure the HTML shows)
  - report.html   a self-contained dashboard

The run directory is never trimmed: the raw outputs (warp benchdata, logs,
the boto3, listing and maintenance JSON, the server metrics) stay next to the
report, and the report links every one of them.

Layout of a run directory:
    run.json                      run metadata (bin/perf-test)
    scenarios.tsv                 name, group, description, warp arguments
    <target>-<variant>/           one per server under test, e.g. arca-plain
        warp/<scenario>.json.zst  warp aggregated benchdata
        warp/<scenario>.log       warp console output
        boto3.json                tests/perf/perf_test.py --json-out
        listing.json              suite.py listing
        maintenance.json          suite.py maintenance (Arca only)
        metrics.txt, server.log   server-side evidence

Usage:
    python report.py <run-dir>

Needs Python 3.14 or later (compression.zstd) and nothing outside the
standard library.
"""

import json
import math
import sys
from compression import zstd
from datetime import datetime, timezone
from pathlib import Path

MIB = 1024 * 1024

# The reference server the suite compares Arca with.
REFERENCE = "minio"
TARGET_LABELS = {"arca": "Arca", "minio": "MinIO"}
VARIANT_LABELS = {"plain": "Plain", "sse": "SSE-S3", "compress": "Compression"}


# --- warp benchdata ------------------------------------------------------------

def _read_json(path: Path):
    data = path.read_bytes()
    if path.suffix == ".zst":
        data = zstd.decompress(data)
    return json.loads(data)


def _latency(block: dict) -> dict:
    """Latency figures of one warp block, from its first client's request stats."""
    for segments in (block.get("requests_by_client") or {}).values():
        for seg in segments:
            req = seg.get("single_sized_requests")
            if req:
                return {"p50_ms": req.get("dur_median_millis"),
                        "p90_ms": req.get("dur_90_millis"),
                        "p99_ms": req.get("dur_99_millis"),
                        "avg_ms": req.get("dur_avg_millis")}
    return {"p50_ms": None, "p90_ms": None, "p99_ms": None, "avg_ms": None}


def _timestamp(text: str) -> datetime:
    """warp's RFC 3339 times carry nanoseconds; datetime keeps microseconds."""
    head, _, frac = text.rstrip("Z").partition(".")
    return datetime.fromisoformat(f"{head}.{(frac or '0')[:6]}+00:00")


def _metrics(block: dict) -> dict | None:
    """Figures of one warp block.

    warp's own analysis (`throughput`) leaves out the ramp-up and ramp-down
    of the run; when the run was too short for it (a fast server working
    through a fixed pool of objects), the figures fall back to the whole run,
    totals over its start and end, flagged `whole_run`.
    """
    tp = block.get("throughput") or {}
    millis = tp.get("measure_duration_millis") or 0
    whole_run = millis <= 0
    if whole_run:
        objects, nbytes = block.get("total_objects", 0), block.get("total_bytes", 0)
        try:
            seconds = (_timestamp(block["end_time"]) - _timestamp(block["start_time"])).total_seconds()
        except (KeyError, ValueError):
            return None
        if objects <= 0 or seconds <= 0:
            return None
    else:
        objects, nbytes, seconds = tp.get("objects", 0), tp.get("bytes", 0), millis / 1000
    return {
        "obj_per_s": objects / seconds,
        "mib_per_s": nbytes / seconds / MIB,
        "whole_run": whole_run,
        "requests": block.get("total_requests", 0),
        "errors": block.get("total_errors", 0),
        "concurrency": block.get("concurrency"),
        **_latency(block),
    }


def load_warp(path: Path) -> dict | None:
    """Throughput and latency of one warp run; None when warp kept no sample.

    The headline figures are warp's Total (all operations together); `ops`
    holds each operation of a mixed run. Throughput is objects (or bytes)
    over warp's measured window, the same figure warp prints as "Average".
    """
    doc = _read_json(Path(path))
    ops = {}
    for name, block in (doc.get("by_op_type") or {}).items():
        m = _metrics(block)
        if m:
            ops[name] = m
    if not ops:
        return None
    total = _metrics(doc.get("total") or {}) or next(iter(ops.values()))
    # A single-operation run's Total has no request stats of its own.
    if total["p50_ms"] is None and len(ops) == 1:
        total.update({k: v for k, v in next(iter(ops.values())).items() if k.endswith("_ms")})
    return {**total, "ops": ops}


# --- summary -------------------------------------------------------------------

def _read_side_file(path: Path):
    try:
        return _read_json(path)
    except (OSError, ValueError) as e:
        return {"error": f"{path.name}: {e}"}


def _load_scenarios(run_dir: Path) -> list[dict]:
    path = run_dir / "scenarios.tsv"
    if not path.exists():
        return []
    lines = path.read_text().splitlines()
    header = lines[0].split("\t")
    return [dict(zip(header, line.split("\t"))) for line in lines[1:] if line.strip()]


def _combos(run_dir: Path, run: dict) -> list[str]:
    """`<target>-<variant>` directories, in the order the run lists them."""
    present = {p.name for p in run_dir.iterdir() if p.is_dir()}
    ordered = [f"{t}-{v}" for v in run.get("variants", []) for t in run.get("targets", [])]
    combos = [c for c in ordered if c in present]
    combos += sorted(c for c in present - set(combos) if "-" in c)
    return combos


def _log_tail(path: Path, lines: int = 12) -> str:
    try:
        return "\n".join(path.read_text(errors="replace").splitlines()[-lines:])
    except OSError:
        return ""


def _geomean(values: list[float]) -> float:
    return math.exp(sum(math.log(v) for v in values) / len(values))


def build_summary(run_dir) -> dict:
    run_dir = Path(run_dir)
    run = _read_side_file(run_dir / "run.json") if (run_dir / "run.json").exists() else {}
    scenarios = _load_scenarios(run_dir)
    combos = _combos(run_dir, run)

    rows = []
    for sc in scenarios:
        results = {}
        for combo in combos:
            warp_dir = run_dir / combo / "warp"
            data = next((p for p in (warp_dir / f"{sc['name']}.json.zst",
                                     warp_dir / f"{sc['name']}.json") if p.exists()), None)
            m = load_warp(data) if data else None
            if m is None:
                results[combo] = {"missing": True,
                                  "log_tail": _log_tail(warp_dir / f"{sc['name']}.log")}
            else:
                results[combo] = {"missing": False, **m}
        rows.append({**sc, "results": results})

    def obj_s(row, combo):
        r = row["results"].get(combo)
        return r["obj_per_s"] if r and not r["missing"] and r["obj_per_s"] > 0 else None

    comparisons, overheads = [], []
    variants = list(dict.fromkeys(c.split("-", 1)[1] for c in combos))
    targets = list(dict.fromkeys(c.split("-", 1)[0] for c in combos))
    for row in rows:
        for v in variants:
            a, ref = obj_s(row, f"arca-{v}"), obj_s(row, f"{REFERENCE}-{v}")
            if a and ref:
                comparisons.append({"scenario": row["name"], "variant": v,
                                    "arca_obj_per_s": a, "reference_obj_per_s": ref,
                                    "ratio": a / ref})
        for t in targets:
            base = obj_s(row, f"{t}-plain")
            for v in variants:
                cur = obj_s(row, f"{t}-{v}")
                if v != "plain" and base and cur:
                    overheads.append({"target": t, "variant": v, "scenario": row["name"],
                                      "ratio": cur / base})

    headline = {}
    for v in variants:
        ratios = [c["ratio"] for c in comparisons if c["variant"] == v]
        if ratios:
            headline[v] = _geomean(ratios)

    def side(name):
        return {c: _read_side_file(run_dir / c / name) for c in combos
                if (run_dir / c / name).exists()}

    raw = sorted(str(p.relative_to(run_dir)) for p in run_dir.rglob("*")
                 if p.is_file() and p.name not in ("report.html", "summary.json"))

    return {
        "generated": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "run": run,
        "combos": combos,
        "reference": REFERENCE,
        "scenarios": rows,
        "comparisons": comparisons,
        "overheads": overheads,
        "headline": headline,
        "boto3": side("boto3.json"),
        "listing": side("listing.json"),
        "maintenance": side("maintenance.json"),
        "raw_files": raw,
    }


# --- HTML ----------------------------------------------------------------------

def _esc(text) -> str:
    """Escape text for HTML element and attribute content."""
    return (str(text).replace("&", "&amp;").replace("<", "&lt;")
            .replace(">", "&gt;").replace('"', "&quot;"))


def _num(v, digits: int = 1) -> str:
    if v is None:
        return "–"
    if abs(v) >= 1000:
        return f"{v:,.0f}"
    return f"{v:.{digits}f}"


def _combo_label(combo: str) -> str:
    t, v = combo.split("-", 1)
    return f"{TARGET_LABELS.get(t, t)} · {VARIANT_LABELS.get(v, v)}"


def _ratio_class(r: float) -> str:
    return "good" if r >= 0.9 else ("warn" if r >= 0.6 else "bad")


REPORT_CSS = """
  :root {
    color-scheme: dark;
    --bg: #070a12; --panel: #0d121c; --panel-2: #121a28;
    --line: #1c2638; --line-2: #2a3850;
    --text: #e8ecf2; --text-2: #93a1b6; --text-3: #6a7a92;
    --cyan: #00d4ff; --cyan-bg: rgba(0,212,255,.12);
    --arca: #3987e5; --ref: #d95926;
    --good: #34d399; --bad: #f472b6; --warn: #ffb347;
    --good-bg: rgba(52,211,153,.14); --bad-bg: rgba(244,114,182,.14); --warn-bg: rgba(255,179,71,.14);
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
      --cyan: #0369a1; --cyan-bg: rgba(3,105,161,.09);
      --arca: #2a78d6; --ref: #eb6834;
      --good: #047857; --bad: #be185d; --warn: #b45309;
      --good-bg: rgba(4,120,87,.10); --bad-bg: rgba(190,24,93,.09); --warn-bg: rgba(180,83,9,.10);
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
  .wrap { max-width: 1080px; margin: 0 auto; padding: 0 16px 4rem; }
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
  .score .of { color: var(--text-2); font-size: 1.05rem; max-width: 40ch; }
  .score .of b { color: var(--text); font-weight: 600; }

  .stats { display: grid; grid-template-columns: repeat(2, 1fr); gap: 1px; margin: 2.5rem 0 0;
           background: var(--line); border: 1px solid var(--line); border-radius: 12px; overflow: hidden; }
  @media (min-width: 720px) { .stats { grid-template-columns: repeat(4, 1fr); } }
  .stat { background: var(--panel); padding: 1.1rem 1.2rem; }
  .stat .v { font-family: var(--mono); font-size: 1.6rem; font-weight: 600; line-height: 1.2; }
  .stat .l { font-size: .78rem; color: var(--text-3); margin-top: .15rem; }
  .stat.good .v { color: var(--good); } .stat.bad .v { color: var(--bad); }
  .stat.warn .v { color: var(--warn); }

  section { margin-top: 3.5rem; }
  h2 { font-family: var(--serif); font-weight: 400; font-size: 1.7rem; line-height: 1.2; }
  h2 .count { font-family: var(--mono); font-size: .8rem; color: var(--text-3); margin-left: .6rem;
              vertical-align: middle; }
  h3 { font-size: 1rem; font-weight: 600; margin: 1.6rem 0 .6rem; }
  .sub { color: var(--text-2); font-size: .92rem; max-width: 68ch; margin: .5rem 0 1.4rem; }

  .legend { display: flex; flex-wrap: wrap; gap: .4rem 1.4rem; margin: 0 0 .8rem;
            font-size: .82rem; color: var(--text-2); }
  .legend i { display: inline-block; width: 10px; height: 10px; border-radius: 3px;
              margin-right: .45rem; vertical-align: -1px; }

  .chart { background: var(--panel); border: 1px solid var(--line); border-radius: 12px; overflow: hidden; }
  .crow { display: grid; grid-template-columns: minmax(0, 1fr) 4.2rem; gap: .25rem 1rem;
          align-items: center; padding: .7rem 1.1rem; border-top: 1px solid var(--line); }
  .crow:first-child { border-top: 0; }
  .crow:hover { background: var(--panel-2); }
  .crow .name { font-size: .88rem; font-weight: 500; overflow-wrap: anywhere; }
  .crow .name small { display: block; color: var(--text-3); font-weight: 400; font-size: .74rem; }
  .crow .bars { grid-column: 1 / -1; display: flex; flex-direction: column; gap: 2px; }
  .crow .rate { grid-row: 1; grid-column: 2; font-family: var(--mono); font-size: .85rem;
                text-align: right; font-weight: 600; }
  .bar { display: grid; grid-template-columns: minmax(0, 1fr) minmax(7.5rem, auto); align-items: center; gap: .6rem; }
  .bar .track { height: 10px; }
  .bar .fill { display: block; height: 10px; border-radius: 0 4px 4px 0; min-width: 2px; }
  .bar .fill.arca { background: var(--arca); } .bar .fill.ref { background: var(--ref); }
  .bar .val { font-family: var(--mono); font-size: .74rem; color: var(--text-2); text-align: right;
              white-space: nowrap; }
  @media (min-width: 720px) {
    .crow { grid-template-columns: 13rem minmax(0, 1fr) 4.2rem; }
    .crow .bars, .crow .rate { grid-row: auto; grid-column: auto; }
  }
  .rate.good { color: var(--good); } .rate.warn { color: var(--warn); } .rate.bad { color: var(--bad); }

  .tbl { overflow-x: auto; background: var(--panel); border: 1px solid var(--line); border-radius: 10px; }
  table { border-collapse: collapse; width: 100%; font-size: .82rem; }
  th, td { padding: .45rem .75rem; text-align: right; white-space: nowrap; border-top: 1px solid var(--line); }
  th:first-child, td:first-child { text-align: left; }
  thead th { border-top: 0; color: var(--text-3); font-weight: 500; font-size: .74rem;
             text-transform: uppercase; letter-spacing: .04em; }
  td { font-family: var(--mono); }
  td:first-child { font-family: var(--sans); }
  tr.gap td { color: var(--text-3); }

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
  .item .body { padding: .2rem .9rem .9rem; }
  .item pre, pre.box { padding: .8rem 1rem; background: var(--bg); font-family: var(--mono); font-size: .76rem;
              color: var(--text-2); white-space: pre-wrap; overflow-wrap: anywhere;
              border: 1px solid var(--line); border-radius: 6px; margin-top: .6rem; }
  .tag { font-size: .72rem; padding: .08rem .5rem; border-radius: 999px; background: var(--cyan-bg);
         color: var(--cyan); white-space: nowrap; }
  .tag.bad { background: var(--bad-bg); color: var(--bad); font-weight: 600; }
  .ok { background: var(--good-bg); color: var(--good); border-radius: 8px; padding: .8rem 1rem; }
  .warnbox { background: var(--warn-bg); color: var(--text); border-radius: 8px; padding: .8rem 1rem;
             font-size: .88rem; }
  .files { columns: 2 18rem; font-family: var(--mono); font-size: .76rem; }
  .files a { display: block; overflow-wrap: anywhere; padding: .1rem 0; }
  ul.notes { padding-left: 1.2rem; color: var(--text-2); font-size: .9rem; max-width: 72ch; }
  ul.notes li { margin: .3rem 0; }

  footer { margin-top: 4rem; padding-top: 1.2rem; border-top: 1px solid var(--line);
           font-size: .8rem; color: var(--text-3); }
"""


def _bar(cls: str, value: float, scale: float, who: str, unit: str) -> str:
    width = value / scale * 100 if scale > 0 else 0
    return (f'<div class="bar"><div class="track"><span class="fill {cls}" '
            f'style="width:{width:.2f}%"></span></div>'
            f'<span class="val">{who} {_num(value)} {unit}</span></div>')


def _comparison_chart(summary: dict, variant: str) -> str:
    rows = ""
    by_name = {s["name"]: s for s in summary["scenarios"]}
    for c in (c for c in summary["comparisons"] if c["variant"] == variant):
        sc = by_name[c["scenario"]]
        a, ref = c["arca_obj_per_s"], c["reference_obj_per_s"]
        scale = max(a, ref)
        title = (f'{sc["name"]}: Arca {_num(a)} obj/s, MinIO {_num(ref)} obj/s, '
                 f'ratio {c["ratio"]:.2f}')
        flagged = [TARGET_LABELS.get(c.split("-", 1)[0], c) for c in (f"arca-{variant}",
                   f"{summary['reference']}-{variant}") if sc["results"][c].get("whole_run")]
        note = f" · † {', '.join(flagged)}: whole-run average" if flagged else ""
        rows += f"""
        <div class="crow" title="{_esc(title)}">
          <div class="name">{_esc(sc["name"])}<small>{_esc(sc.get("description", ""))}{_esc(note)}</small></div>
          <div class="bars">{_bar("arca", a, scale, "Arca", "obj/s")}{_bar("ref", ref, scale, "MinIO", "obj/s")}</div>
          <div class="rate {_ratio_class(c["ratio"])}">{c["ratio"]:.2f}×</div>
        </div>"""
    return f'<div class="chart" role="list">{rows}</div>' if rows else ""


WHOLE_RUN_NOTE = ("whole-run average: the run was too short for warp's own analysis, "
                  "so the figure includes its ramp-up and ramp-down")


def _dagger(r: dict) -> str:
    return f' <span title="{_esc(WHOLE_RUN_NOTE)}">†</span>' if r.get("whole_run") else ""


def _scenario_table(sc: dict, combos: list[str]) -> str:
    body = ""
    for combo in combos:
        r = sc["results"].get(combo)
        if r is None:
            continue
        link = f'<a href="{_esc(combo)}/warp/{_esc(sc["name"])}.json.zst">raw</a>'
        if r["missing"]:
            body += (f'<tr class="gap"><td>{_esc(_combo_label(combo))}</td>'
                     f'<td colspan="7">no result, see <a href="{_esc(combo)}/warp/{_esc(sc["name"])}.log">log</a></td></tr>')
            continue
        err = f'<span class="tag bad">{r["errors"]}</span>' if r["errors"] else "0"
        body += (f'<tr><td>{_esc(_combo_label(combo))}</td><td>{_num(r["obj_per_s"])}{_dagger(r)}</td>'
                 f'<td>{_num(r["mib_per_s"])}</td><td>{_num(r["p50_ms"], 2)}</td>'
                 f'<td>{_num(r["p90_ms"], 2)}</td><td>{_num(r["p99_ms"], 2)}</td>'
                 f'<td>{err}</td><td>{link}</td></tr>')
        if len(r["ops"]) > 1:
            for op, m in r["ops"].items():
                body += (f'<tr><td>&nbsp;&nbsp;↳ {_esc(op)}</td><td>{_num(m["obj_per_s"])}</td>'
                         f'<td>{_num(m["mib_per_s"])}</td><td>{_num(m["p50_ms"], 2)}</td>'
                         f'<td>{_num(m["p90_ms"], 2)}</td><td>{_num(m["p99_ms"], 2)}</td>'
                         f'<td>{m["errors"]}</td><td></td></tr>')
    logs = "".join(
        f'<pre>{_esc(_combo_label(c))}: {_esc(r["log_tail"])}</pre>'
        for c, r in sc["results"].items() if r["missing"] and r.get("log_tail"))
    return f"""
        <details class="item">
          <summary><span class="tag">{_esc(sc.get("group", ""))}</span><code>{_esc(sc["name"])}</code>
            <span style="color:var(--text-3)">{_esc(sc.get("description", ""))}</span></summary>
          <div class="body">
            <div class="tbl"><table>
              <thead><tr><th>Server</th><th>obj/s</th><th>MiB/s</th><th>p50 ms</th><th>p90 ms</th>
              <th>p99 ms</th><th>errors</th><th></th></tr></thead>
              <tbody>{body}</tbody></table></div>
            <pre>warp {_esc(sc.get("warp", ""))}</pre>{logs}
          </div>
        </details>"""


def _overhead_table(summary: dict) -> str:
    if not summary["overheads"]:
        return ""
    cols = sorted({(o["target"], o["variant"]) for o in summary["overheads"]},
                  key=lambda tv: (tv[1], tv[0]))
    by = {(o["scenario"], o["target"], o["variant"]): o["ratio"] for o in summary["overheads"]}
    head = "".join(f"<th>{_esc(TARGET_LABELS.get(t, t))} {_esc(VARIANT_LABELS.get(v, v))}</th>"
                   for t, v in cols)
    body = ""
    for sc in summary["scenarios"]:
        cells = ""
        for t, v in cols:
            r = by.get((sc["name"], t, v))
            cells += (f'<td class="rate {_ratio_class(r)}">{r:.2f}×</td>' if r is not None
                      else "<td>–</td>")
        body += f'<tr><td>{_esc(sc["name"])}</td>{cells}</tr>'
    return f"""
  <section>
    <h2>Feature overhead</h2>
    <p class="sub">Throughput of each feature variant relative to the plain run of the same server
    (1.00× = no cost).</p>
    <div class="tbl"><table><thead><tr><th>Scenario</th>{head}</tr></thead>
    <tbody>{body}</tbody></table></div>
  </section>"""


def _kv_section(title: str, sub: str, data: dict, render) -> str:
    if not data:
        return ""
    parts = ""
    for combo, doc in data.items():
        inner = (f'<p class="warnbox">{_esc(doc["error"])}</p>' if "error" in doc
                 else render(doc))
        parts += f"<h3>{_esc(_combo_label(combo))}</h3>{inner}"
    return f"""
  <section>
    <h2>{_esc(title)}</h2>
    <p class="sub">{sub}</p>
    {parts}
  </section>"""


def _meta_line(doc: dict, rows_key: str) -> str:
    """The scalar fields of a side file, as one line."""
    return ", ".join(f"{_esc(k)}: {_esc(_num(v, 2) if isinstance(v, float) else v)}"
                     for k, v in doc.items() if k != rows_key)


def _steps_table(doc: dict) -> str:
    rows = ""
    for s in doc.get("steps", []):
        rate = s.get("objects_per_s")
        rc = s.get("rc", 0)
        status = "ok" if rc == 0 else f'<span class="tag bad">rc {rc}</span>'
        rss = s.get("max_rss_mib")
        rows += (f'<tr><td>{_esc(s.get("name", ""))}</td><td>{_num(s.get("seconds"), 2)}</td>'
                 f'<td>{_num(rate)}</td><td>{_num(rss)}</td><td>{status}</td></tr>')
    meta = _meta_line(doc, "steps")
    return (f'<p class="sub mono">{meta}</p><div class="tbl"><table><thead><tr><th>Step</th>'
            f'<th>seconds</th><th>obj/s</th><th>peak RSS MiB</th><th>status</th></tr></thead>'
            f'<tbody>{rows}</tbody></table></div>')


def _walks_table(doc: dict) -> str:
    rows = ""
    for w in doc.get("walks", []):
        rows += (f'<tr><td>{_esc(w.get("name", ""))}</td><td>{_num(w.get("seconds"), 3)}</td>'
                 f'<td>{_num(w.get("requests"), 0)}</td><td>{_num(w.get("entries"), 0)}</td>'
                 f'<td style="text-align:left;font-family:var(--sans)">{_esc(w.get("description", ""))}</td></tr>')
    meta = _meta_line(doc, "walks")
    return (f'<p class="sub mono">{meta}</p><div class="tbl"><table><thead><tr><th>Walk</th>'
            f'<th>seconds</th><th>requests</th><th>entries</th><th style="text-align:left">what</th>'
            f'</tr></thead><tbody>{rows}</tbody></table></div>')


def _boto3_table(doc: dict) -> str:
    rows = ""
    for r in doc.get("results", []):
        tput = r.get("wall_ops_per_sec", r.get("throughput"))
        unit = "ops/s" if "wall_ops_per_sec" in r else r.get("throughput_unit", "")
        rows += (f'<tr><td>{_esc(r.get("name", ""))}</td><td>{_num(tput)} {_esc(unit)}</td>'
                 f'<td>{_num(r.get("p50_ms"))}</td><td>{_num(r.get("p95_ms"))}</td>'
                 f'<td>{_num(r.get("p99_ms"))}</td><td>{r.get("errors", 0)}</td></tr>')
    return (f'<div class="tbl"><table><thead><tr><th>Test</th><th>throughput</th><th>p50 ms</th>'
            f'<th>p95 ms</th><th>p99 ms</th><th>errors</th></tr></thead><tbody>{rows}</tbody>'
            f'</table></div><p class="sub mono">score: {_num(doc.get("score"))}</p>')


def render_html(summary: dict) -> str:
    run = summary["run"] if "error" not in summary["run"] else {}
    combos = summary["combos"]
    ref = TARGET_LABELS.get(summary["reference"], summary["reference"])
    headline = summary["headline"]
    total_errors = sum(r.get("errors", 0) for sc in summary["scenarios"]
                       for r in sc["results"].values() if not r["missing"])
    gaps = sum(1 for sc in summary["scenarios"] for r in sc["results"].values() if r["missing"])

    if headline:
        variant = "plain" if "plain" in headline else next(iter(headline))
        pct = headline[variant] * 100
        score = f"""
  <div class="score">
    <div class="big">{pct:.0f}<small>%</small></div>
    <div class="of">Arca's throughput as a share of <b>{_esc(ref)}</b>'s, geometric mean over the
    {_esc(VARIANT_LABELS.get(variant, variant).lower())} scenarios</div>
  </div>"""
        others = " · ".join(f"{_esc(VARIANT_LABELS.get(v, v))} {r * 100:.0f}%"
                            for v, r in headline.items() if v != variant)
        if others:
            score += f'<p class="stamp">{others}</p>'
    else:
        score = '<p class="lede">Arca only: no reference server in this run.</p>'

    compare = ""
    for v in dict.fromkeys(c["variant"] for c in summary["comparisons"]):
        compare += f"<h3>{_esc(VARIANT_LABELS.get(v, v))}</h3>{_comparison_chart(summary, v)}"
    if compare:
        compare = f"""
  <section>
    <h2>Arca vs {_esc(ref)}<span class="count">objects per second, higher is better</span></h2>
    <p class="sub">Each row is one warp scenario; the bars share a scale within the row and the
    figure on the right is Arca ÷ {_esc(ref)}. Exact numbers and latencies are in the
    scenario details below. † marks a {_esc(WHOLE_RUN_NOTE)}.</p>
    <div class="legend"><span><i style="background:var(--arca)"></i>Arca</span>
    <span><i style="background:var(--ref)"></i>{_esc(ref)}</span></div>
    {compare}
  </section>"""

    details = "".join(_scenario_table(sc, combos) for sc in summary["scenarios"])

    meta_rows = "".join(
        f'<tr><td>{_esc(k)}</td><td style="text-align:left;white-space:normal">{_esc(v if not isinstance(v, (dict, list)) else json.dumps(v))}</td></tr>'
        for k, v in run.items())
    files = "".join(f'<a href="{_esc(f)}">{_esc(f)}</a>' for f in summary["raw_files"])

    errors_cls = "bad" if total_errors else "good"
    gaps_cls = "warn" if gaps else "good"
    stamp = _esc(run.get("run_id", summary["generated"]))

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Arca Performance Report</title>
<style>{REPORT_CSS}</style>
</head>
<body>
<div class="wrap">

<header>
  <div class="brand"><b>arca</b> / performance suite</div>
  <h1>Performance report</h1>
  <p class="lede">Throughput and latency of Arca's S3 data path, measured with warp on the
  same host and storage as {_esc(ref)}, plus SDK-side, listing and offline maintenance timings.</p>
  <p class="stamp">run {stamp} · profile {_esc(run.get("profile", "?"))} ·
  storage {_esc(run.get("storage", "?"))} · generated {_esc(summary["generated"])}</p>
  {score}
  <div class="stats">
    <div class="stat"><div class="v">{len(summary["scenarios"])}</div><div class="l">Scenarios</div></div>
    <div class="stat"><div class="v">{len(combos)}</div><div class="l">Servers × variants</div></div>
    <div class="stat {errors_cls}"><div class="v">{total_errors}</div><div class="l">Request errors</div></div>
    <div class="stat {gaps_cls}"><div class="v">{gaps}</div><div class="l">Runs without result</div></div>
  </div>
</header>

<main>
{compare}
  <section>
    <h2>Scenario details<span class="count">{len(summary["scenarios"])} scenarios</span></h2>
    <p class="sub">Click a scenario for every server's throughput, latency percentiles and errors,
    the exact warp arguments and the raw benchdata.</p>
    {details}
  </section>
{_overhead_table(summary)}
{_kv_section("Listing", "Full ListObjectsV2 walks over a seeded key tree, timed from the client, "
             "with and without a delimiter.", summary["listing"], _walks_table)}
{_kv_section("Maintenance", "Offline maintenance commands run against the data of the seeded "
             "server, stopped. Peak RSS is the command's own.", summary["maintenance"], _steps_table)}
{_kv_section("boto3 suite", "tests/perf/perf_test.py, the SDK-side suite. Throughput is wall "
             "clock (operations over elapsed time).", summary["boto3"], _boto3_table)}
  <section>
    <h2>Run</h2>
    <p class="sub">Numbers depend on the host, the container runtime and the disk: compare runs
    made on the same machine only.</p>
    <div class="tbl"><table><tbody>{meta_rows}</tbody></table></div>
  </section>

  <section>
    <h2>Raw data<span class="count">{len(summary["raw_files"])} files</span></h2>
    <p class="sub">Everything the run produced, next to this report. <a href="summary.json">summary.json</a>
    holds every figure shown here. warp benchdata can be re-analysed with
    <code>warp analyze &lt;file&gt;</code>.</p>
    <div class="files">{files}</div>
  </section>
</main>

<footer>
  Contains AI-generated code, see the <a href="https://github.com/dxc-technology/arca/blob/main/NOTICE">NOTICE</a> file.
</footer>
</div>
</body>
</html>"""


def main(argv: list[str]) -> int:
    if len(argv) != 1:
        print("Usage: python report.py <run-dir>", file=sys.stderr)
        return 2
    run_dir = Path(argv[0])
    summary = build_summary(run_dir)
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (run_dir / "report.html").write_text(render_html(summary), encoding="utf-8")
    print(f"==> Report: {run_dir / 'report.html'}")
    print(f"==> Summary: {run_dir / 'summary.json'}")
    for v, r in summary["headline"].items():
        print(f"    Arca / {summary['reference']} ({v}): {r * 100:.0f}%")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
