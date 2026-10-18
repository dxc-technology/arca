"""Tests for suite.py, the performance suite runner (the parts that need no server).

Run with: bin/test perf-report
"""

import json
import sys
import zlib
from collections import Counter

import pytest

import suite


# --- listing tree --------------------------------------------------------------

def test_tree_keys_spread_evenly_over_every_leaf():
    keys = suite.tree_keys(suite.DIRS * suite.SUBDIRS * 5)
    leaves = Counter(k.rsplit("/", 1)[0] for k in keys)
    assert len(leaves) == suite.DIRS * suite.SUBDIRS
    assert set(leaves.values()) == {5}


def test_tree_keys_are_unique_and_sorted_within_a_leaf():
    keys = suite.tree_keys(1000)
    assert len(set(keys)) == 1000
    leaf = [k for k in keys if k.startswith("d03/s04/")]
    assert leaf == sorted(leaf)


def test_the_listing_walks_hit_existing_prefixes():
    keys = suite.tree_keys(suite.DIRS * suite.SUBDIRS)
    for name, _desc, _fn, params in suite.LISTING_WALKS:
        prefix = params.get("Prefix")
        if prefix and name != "prefix-missing":
            assert any(k.startswith(prefix) for k in keys), name
        if name == "prefix-missing":
            assert not any(k.startswith(prefix) for k in keys)


# --- seed ----------------------------------------------------------------------

@pytest.mark.parametrize("size", suite.SEED_SIZES)
def test_seed_bodies_have_the_requested_size(size):
    assert len(suite.seed_body("seed/00/obj-00000001", size)) == size


def test_seed_bodies_are_compressible():
    # compress-existing must have real work to do.
    body = suite.seed_body("seed/00/obj-00000001", 65536)
    assert len(zlib.compress(body)) < len(body) / 10


# --- maintenance ---------------------------------------------------------------

def test_recover_runs_last():
    for variant in ("plain", "sse", "compress"):
        names = [n for n, _ in suite.maintenance_steps(variant, "b")]
        assert names[-1].startswith("recover --skip-verify")


def test_compress_existing_only_runs_on_the_plain_variant():
    def has(variant):
        return any(n == "compress-existing" for n, _ in suite.maintenance_steps(variant, "b"))
    assert has("plain")
    assert not has("sse") and not has("compress")


def test_offline_commands_skip_the_grace_period():
    # The server is stopped: nothing is in flight, everything is in scope.
    for name, cmd in suite.maintenance_steps("plain", "b"):
        if cmd[0] in ("fsck", "gc"):
            assert cmd[-2:] == ["--grace-seconds", "0"], name


def test_run_timed_reports_exit_code_time_rss_and_output():
    r = suite.run_timed([sys.executable, "-c",
                         "import sys; print('\\n'.join(map(str, range(40)))); sys.exit(3)"])
    assert r["rc"] == 3
    assert r["seconds"] >= 0
    assert r["max_rss_mib"] > 0
    assert r["output_tail"][-1] == "39" and len(r["output_tail"]) == 15


def fake_arca(tmp_path, failing: str | None = None):
    """An `arca` stand-in that records its arguments and fails on one subcommand."""
    log = tmp_path / "calls.log"
    script = tmp_path / "arca"
    script.write_text(f"""#!{sys.executable}
import sys
open({str(log)!r}, "a").write(" ".join(sys.argv[1:]) + "\\n")
sys.exit(1 if sys.argv[1] == {failing!r} else 0)
""")
    script.chmod(0o755)
    return script, log


def test_maintenance_runs_every_step_with_the_config(tmp_path):
    arca, log = fake_arca(tmp_path)
    out = tmp_path / "m.json"
    rc = suite.main(["maintenance", "--variant", "plain", "--objects", "100",
                     "--arca", str(arca), "--config", "/etc/x.toml", "--out", str(out)])
    assert rc == 0
    calls = log.read_text().splitlines()
    assert len(calls) == len(suite.maintenance_steps("plain", "perf-maint"))
    assert all(c.endswith("--config-path /etc/x.toml") for c in calls)
    doc = json.loads(out.read_text())
    assert doc["objects"] == 100
    assert all("objects_per_s" in s for s in doc["steps"])


def test_a_failing_step_is_recorded_and_fails_the_run(tmp_path):
    arca, _ = fake_arca(tmp_path, failing="gc")
    out = tmp_path / "m.json"
    rc = suite.main(["maintenance", "--variant", "sse", "--arca", str(arca),
                     "--out", str(out)])
    assert rc == 1
    steps = {s["name"]: s["rc"] for s in json.loads(out.read_text())["steps"]}
    assert steps["gc (dry run)"] == 1
    # The later steps still ran.
    assert steps["recover --skip-verify"] == 0


# --- compression subresource ---------------------------------------------------

def test_compression_xml_matches_the_documented_body():
    assert suite.compression_xml("zstd") == (
        b"<CompressionConfiguration><Algorithm>zstd</Algorithm></CompressionConfiguration>")


def test_count_sidecars_counts_only_sidecar_files(tmp_path):
    shard = tmp_path / "blobs" / "ab" / "cd"
    shard.mkdir(parents=True)
    for name in ("x", "x.meta", "y", "y.meta", "z.tmp", "y.meta.123.tmp"):
        (shard / name).write_text("")
    assert suite.count_sidecars(str(tmp_path)) == 2


def test_maintenance_without_a_store_has_no_rates(tmp_path):
    arca, _ = fake_arca(tmp_path)
    out = tmp_path / "m.json"
    suite.main(["maintenance", "--variant", "sse", "--arca", str(arca),
                "--data", str(tmp_path / "none"), "--out", str(out)])
    doc = json.loads(out.read_text())
    assert doc["objects"] == 0
    assert not any("objects_per_s" in s for s in doc["steps"])
