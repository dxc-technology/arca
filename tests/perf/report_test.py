"""Tests for report.py, the performance suite report generator.

Run with: bin/test perf-report
"""

import json
from compression import zstd

import pytest

import report


def requests(p50: float, p90: float, p99: float, avg: float, count: int, size: int) -> dict:
    return {"single_sized_requests": {
        "dur_median_millis": p50, "dur_90_millis": p90, "dur_99_millis": p99,
        "dur_avg_millis": avg, "fastest_millis": p50 / 2, "slowest_millis": p99 * 2,
        "std_dev_millis": 0.5, "requests": count, "obj_size": size, "merged_entries": 1}}


def op_block(objects: float, nbytes: float, millis: int, errors: int = 0,
             concurrency: int = 16, latency: dict | None = None) -> dict:
    """One `by_op_type` entry the way warp 1.x writes it in its aggregated benchdata."""
    return {
        "total_requests": int(objects), "total_objects": int(objects),
        "total_errors": errors, "total_bytes": int(nbytes), "concurrency": concurrency,
        "throughput": {"measure_duration_millis": millis, "bytes": nbytes,
                       "objects": objects, "ops": int(objects), "errors": errors},
        "requests_by_client": {"C1": [latency or requests(1.0, 2.0, 4.0, 1.2, int(objects), 1024)]},
    }


def warp_doc(by_op: dict, total: dict | None = None) -> dict:
    first = next(iter(by_op.values()))
    return {"v": 2, "commandline": "/warp put --access-key=*REDACTED*", "final": True,
            "total": total or first, "by_op_type": by_op}


def write_zst(path, doc: dict):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(zstd.compress(json.dumps(doc).encode()))


SCENARIOS_TSV = (
    "name\tgroup\tdescription\twarp\n"
    "put-1k-c16\tSmall PUT\tPUT 1 KiB, 16 clients\tput --obj.size 1KiB --concurrent 16\n"
    "mixed-64k-c32\tMixed\tMixed 64 KiB, 32 clients\tmixed --obj.size 64KiB --concurrent 32\n"
)


@pytest.fixture
def run_dir(tmp_path):
    """A run with Arca and MinIO, plain and sse, two scenarios each."""
    (tmp_path / "run.json").write_text(json.dumps({
        "run_id": "20261018T080000Z-08c53c4", "git_sha": "08c53c4", "profile": "quick",
        "targets": ["arca", "minio"], "variants": ["plain", "sse"], "storage": "volume",
        "duration": "10s"}))
    (tmp_path / "scenarios.tsv").write_text(SCENARIOS_TSV)
    speeds = {"arca-plain": 1000, "minio-plain": 2000, "arca-sse": 500, "minio-sse": 1600}
    for combo, ops in speeds.items():
        d = tmp_path / combo / "warp"
        # put: `ops` objects/s over 2 s of measurement.
        write_zst(d / "put-1k-c16.json.zst",
                  warp_doc({"PUT": op_block(ops * 2, ops * 2 * 1024, 2000)}))
        get = op_block(ops * 3, ops * 3 * 65536, 2000,
                       latency=requests(0.5, 1.0, 2.0, 0.6, ops * 3, 65536))
        put = op_block(ops, ops * 65536, 2000)
        total = op_block(ops * 4, ops * 4 * 65536, 2000)
        write_zst(d / "mixed-64k-c32.json.zst", warp_doc({"GET": get, "PUT": put}, total))
    return tmp_path


# --- warp parsing --------------------------------------------------------------

def test_throughput_is_objects_over_the_measured_window(tmp_path):
    path = tmp_path / "x.json.zst"
    write_zst(path, warp_doc({"PUT": op_block(5292.87, 5419898.7, 2000)}))
    m = report.load_warp(path)
    assert m["obj_per_s"] == pytest.approx(2646.4, rel=1e-3)
    assert m["mib_per_s"] == pytest.approx(2.585, rel=1e-3)


def test_latency_percentiles_come_from_the_request_stats(tmp_path):
    path = tmp_path / "x.json.zst"
    write_zst(path, warp_doc({"PUT": op_block(100, 102400, 1000,
                                              latency=requests(1.3, 2.1, 4.5, 1.5, 100, 1024))}))
    m = report.load_warp(path)
    assert (m["p50_ms"], m["p90_ms"], m["p99_ms"], m["avg_ms"]) == (1.3, 2.1, 4.5, 1.5)


def test_a_mixed_run_keeps_each_operation(tmp_path, run_dir):
    m = report.load_warp(run_dir / "arca-plain" / "warp" / "mixed-64k-c32.json.zst")
    assert set(m["ops"]) == {"GET", "PUT"}
    assert m["ops"]["GET"]["p50_ms"] == 0.5
    # The headline figures are warp's Total, not one of the operations.
    assert m["obj_per_s"] == pytest.approx(2000)  # 4000 objects over 2 s


def test_errors_and_concurrency_are_reported(tmp_path):
    path = tmp_path / "x.json.zst"
    write_zst(path, warp_doc({"PUT": op_block(100, 102400, 1000, errors=3, concurrency=64)}))
    m = report.load_warp(path)
    assert m["errors"] == 3
    assert m["concurrency"] == 64


def test_uncompressed_benchdata_is_read_too(tmp_path):
    path = tmp_path / "x.json"
    path.write_text(json.dumps(warp_doc({"PUT": op_block(100, 102400, 1000)})))
    assert report.load_warp(path)["obj_per_s"] == pytest.approx(100)


def test_a_run_too_short_for_warp_falls_back_to_the_whole_run_average(tmp_path):
    # warp skips its analysis when the run ends in a few seconds (a fast server
    # deleting a fixed pool of objects): the totals and the run's start and end
    # are still there.
    block = op_block(0, 0, 0)
    block.update({"total_objects": 50000, "total_requests": 500,
                  "start_time": "2026-10-18T08:36:32.5Z", "end_time": "2026-10-18T08:36:35.0Z"})
    path = tmp_path / "x.json.zst"
    write_zst(path, warp_doc({"DELETE": block}))
    m = report.load_warp(path)
    assert m["obj_per_s"] == pytest.approx(20000)
    assert m["whole_run"] is True
    assert m["ops"]["DELETE"]["whole_run"] is True


def test_warp_analysed_runs_are_not_flagged(tmp_path):
    path = tmp_path / "x.json.zst"
    write_zst(path, warp_doc({"PUT": op_block(100, 102400, 1000)}))
    assert report.load_warp(path)["whole_run"] is False


def test_a_run_without_samples_has_no_figures(tmp_path):
    # warp writes the file but skips the analysis when a run is too short.
    path = tmp_path / "x.json.zst"
    write_zst(path, {"v": 2, "final": True, "total": {}, "by_op_type": {}})
    assert report.load_warp(path) is None


# --- summary -------------------------------------------------------------------

def test_summary_lists_every_scenario_for_every_combination(run_dir):
    s = report.build_summary(run_dir)
    assert [sc["name"] for sc in s["scenarios"]] == ["put-1k-c16", "mixed-64k-c32"]
    assert set(s["scenarios"][0]["results"]) == {"arca-plain", "minio-plain", "arca-sse", "minio-sse"}


def test_arca_is_compared_with_minio_per_variant(run_dir):
    s = report.build_summary(run_dir)
    cmp = {(c["scenario"], c["variant"]): c["ratio"] for c in s["comparisons"]}
    assert cmp[("put-1k-c16", "plain")] == pytest.approx(0.5)
    assert cmp[("put-1k-c16", "sse")] == pytest.approx(500 / 1600)


def test_headline_is_the_geometric_mean_of_the_ratios(run_dir):
    s = report.build_summary(run_dir)
    # Both scenarios of the plain variant are at 0.5: geomean 0.5.
    assert s["headline"]["plain"] == pytest.approx(0.5)


def test_variant_overhead_is_relative_to_plain_of_the_same_target(run_dir):
    s = report.build_summary(run_dir)
    over = {(o["target"], o["variant"], o["scenario"]): o["ratio"] for o in s["overheads"]}
    assert over[("arca", "sse", "put-1k-c16")] == pytest.approx(0.5)
    assert over[("minio", "sse", "put-1k-c16")] == pytest.approx(0.8)


def test_a_missing_scenario_file_is_an_explicit_gap(run_dir):
    (run_dir / "minio-sse" / "warp" / "put-1k-c16.json.zst").unlink()
    (run_dir / "minio-sse" / "warp" / "put-1k-c16.log").write_text("warp: connection refused\n")
    s = report.build_summary(run_dir)
    res = s["scenarios"][0]["results"]["minio-sse"]
    assert res["missing"] is True
    assert "connection refused" in res["log_tail"]
    # No ratio is made up for a gap.
    assert ("put-1k-c16", "sse") not in {(c["scenario"], c["variant"]) for c in s["comparisons"]}


def test_arca_only_run_has_no_comparison(run_dir):
    import shutil
    for combo in ("minio-plain", "minio-sse"):
        shutil.rmtree(run_dir / combo)
    s = report.build_summary(run_dir)
    assert s["comparisons"] == []
    assert s["headline"] == {}


def test_every_raw_file_is_listed(run_dir):
    (run_dir / "arca-plain" / "maintenance.json").write_text("{}")
    s = report.build_summary(run_dir)
    assert "arca-plain/warp/put-1k-c16.json.zst" in s["raw_files"]
    assert "arca-plain/maintenance.json" in s["raw_files"]
    assert "run.json" in s["raw_files"]


def test_maintenance_boto3_and_listing_results_are_carried_over(run_dir):
    maint = {"objects": 100, "steps": [{"name": "fsck", "seconds": 1.5, "rc": 0}]}
    (run_dir / "arca-plain" / "maintenance.json").write_text(json.dumps(maint))
    listing = {"objects": 1000, "walks": [{"name": "flat", "seconds": 0.2}]}
    (run_dir / "minio-plain" / "listing.json").write_text(json.dumps(listing))
    boto = {"results": [{"name": "Small PUT", "ops_per_sec": 10.0}], "score": 3.0}
    (run_dir / "arca-plain" / "boto3.json").write_text(json.dumps(boto))
    s = report.build_summary(run_dir)
    assert s["maintenance"]["arca-plain"] == maint
    assert s["listing"]["minio-plain"] == listing
    assert s["boto3"]["arca-plain"] == boto


def test_a_corrupt_side_file_does_not_break_the_report(run_dir):
    (run_dir / "arca-plain" / "maintenance.json").write_text("{not json")
    s = report.build_summary(run_dir)
    assert "error" in s["maintenance"]["arca-plain"]


# --- HTML ----------------------------------------------------------------------

def test_html_names_every_scenario_and_links_the_raw_files(run_dir):
    html = report.render_html(report.build_summary(run_dir))
    assert "put-1k-c16" in html and "mixed-64k-c32" in html
    assert 'href="arca-plain/warp/put-1k-c16.json.zst"' in html
    assert 'href="summary.json"' in html


def test_html_escapes_untrusted_text(run_dir):
    (run_dir / "minio-sse" / "warp" / "put-1k-c16.json.zst").unlink()
    (run_dir / "minio-sse" / "warp" / "put-1k-c16.log").write_text("<script>alert(1)</script>")
    html = report.render_html(report.build_summary(run_dir))
    assert "<script>alert(1)</script>" not in html
    assert "&lt;script&gt;" in html


def test_html_has_light_and_dark_themes(run_dir):
    html = report.render_html(report.build_summary(run_dir))
    assert "prefers-color-scheme" in html


def test_main_writes_the_report_and_the_summary(run_dir):
    assert report.main([str(run_dir)]) == 0
    assert (run_dir / "report.html").read_text().startswith("<!DOCTYPE html>")
    summary = json.loads((run_dir / "summary.json").read_text())
    assert summary["run"]["run_id"] == "20261018T080000Z-08c53c4"


def test_whole_run_figures_are_marked_in_the_html(run_dir):
    block = op_block(0, 0, 0)
    block.update({"total_objects": 50000, "start_time": "2026-10-18T08:36:32Z",
                  "end_time": "2026-10-18T08:36:35Z"})
    write_zst(run_dir / "minio-plain" / "warp" / "put-1k-c16.json.zst", warp_doc({"PUT": block}))
    html = report.render_html(report.build_summary(run_dir))
    assert "MinIO: whole-run average" in html
    assert "†</span>" in html
