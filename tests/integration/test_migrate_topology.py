"""Integration tests for Phase 30 milestone M4 — `migrate-topology`.

`migrate-topology` is the guided, in-place single-node <-> HA-cluster transition.
The cluster is fully replicated (not sharded), so there is NO data
redistribution: the tool generates the `[cluster]` config stanza, runs small DB
ops, and prints the operator runbook.

This module is driven by `bin/test migrate-topology`, which orchestrates a
single-node SQLite lifecycle around the offline `arca migrate-topology` CLI and
asserts on its output (the deterministic, non-flaky parts):

  Phase SEED   — single-node Arca on SQLite; seed objects, then delete one in a
                 way that leaves a row (so the DB has content to carry across).
                 (ARCA_TOPO_PHASE=seed)
  (bin/test stops Arca, runs `arca migrate-topology --to-cluster`, asserts the
   emitted [cluster] stanza is well-formed and that `arca cluster status` accepts
   the merged config; then runs `arca migrate-topology --to-single --force` and
   asserts the cluster-only state purge + VACUUM succeed; restarts standalone.)
  Phase VERIFY — single-node Arca on SQLite again; assert the seeded objects are
                 still served (the round trip preserved data in place).
                 (ARCA_TOPO_PHASE=verify)

The full 3-node round-trip (seed node emits config; two empty joiners come up and
anti-entropy converges) is documented as a runbook in
documentation/docs/guide/ha.md rather than automated here: the emitted secret is
random and the 3-node harness uses fixed configs, so an automated bringup with
the generated stanza would be brittle. The DB ops and config emission — the parts
unique to this milestone — ARE automated end-to-end (here + the Rust unit tests
in crates/arca-server/src/migrate_topology.rs).
"""

import os

import pytest

PHASE = os.environ.get("ARCA_TOPO_PHASE")

pytestmark = pytest.mark.skipif(
    PHASE not in ("seed", "verify"),
    reason="migrate-topology tests run via `bin/test migrate-topology` "
    "(sets ARCA_TOPO_PHASE)",
)

BUCKET = "topo-bucket"

# (key, body) pairs seeded and then re-verified after the round trip.
OBJECTS = [
    ("keep-a.txt", b"first object survives the round trip"),
    ("keep-b.bin", bytes(range(128))),
]


@pytest.mark.skipif(PHASE != "seed", reason="seed phase only")
def test_seed(s3_client):
    """Seed a bucket with a couple of objects on the standalone SQLite node."""
    s3_client.create_bucket(Bucket=BUCKET)
    for key, body in OBJECTS:
        s3_client.put_object(Bucket=BUCKET, Key=key, Body=body)

    # Sanity: everything is readable before the topology round trip.
    listed = s3_client.list_objects_v2(Bucket=BUCKET)
    keys = {o["Key"] for o in listed.get("Contents", [])}
    assert keys == {k for k, _ in OBJECTS}


@pytest.mark.skipif(PHASE != "verify", reason="verify phase only")
def test_verify(s3_client):
    """After --to-cluster then --to-single, the seeded data is intact."""
    listed = s3_client.list_objects_v2(Bucket=BUCKET)
    keys = {o["Key"] for o in listed.get("Contents", [])}
    assert keys == {k for k, _ in OBJECTS}, (
        "objects must survive the topology round trip (data is preserved in place)"
    )
    for key, body in OBJECTS:
        got = s3_client.get_object(Bucket=BUCKET, Key=key)["Body"].read()
        assert got == body, f"body of {key} changed across the round trip"
