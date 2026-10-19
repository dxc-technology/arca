# Performance Testing

Arca ships two performance tools, both run from the repository in containers:

- **`bin/perf-test suite`**, the benchmark suite: Arca against MinIO with
  [warp](https://github.com/minio/warp), plus listing walks, an SDK-side suite
  and offline maintenance timings, written to one run directory with an HTML
  report. Use it to measure a change, compare releases or size hardware.
- **`bin/perf-test`** (no subcommand), the quick boto3 check against the server
  you are running: see [Quick boto3 check](#quick-boto3-check).

## The benchmark suite

```bash
bin/perf-test suite --profile quick           # about 30 minutes, every variant
bin/perf-test suite                           # full profile, a few hours
bin/perf-test suite --targets arca --variants plain --scenarios put-1k-c16,get-1k-c16
bin/perf-test suite --list                    # the scenarios of a profile
```

Each server under test is started fresh, on a fresh volume, in a compose project of its own
(`arca-test-perf-suite`), with no fixed host port: the suite never touches the stack
`bin/arca start` runs, and can run next to it, though the two then share the CPU and the
disk. For a baseline, stop everything else on the machine first.

| Option | Default | Meaning |
|---|---|---|
| `--profile quick\|full` | `full` | Scenario set and sizes: `quick` runs each scenario 10 s, `full` 60 s (30 s for the large-object ones) and adds the concurrency sweep |
| `--targets` | `arca,minio` | Servers to benchmark |
| `--variants` | `plain,sse,compress` | Server configurations, see below |
| `--scenarios` | all | Only these warp scenarios (names from `--list`) |
| `--storage volume\|tmpfs` | `volume` | Server data on a named Docker volume (a real disk, fsync included) or in memory (`PERF_TMPFS_SIZE`, default `8g`: the large-object scenarios need it) |
| `--release TAG` | | Benchmark a published Arca image (`latest`, `X.Y.Z`) instead of the local build |
| `--build` | | Rebuild the local Arca image first |
| `--no-maintenance`, `--no-boto3`, `--no-listing` | | Skip a part |
| `--out DIR` | `perf-results/<UTC time>-<commit>` | Run directory (git-ignored) |
| `--baseline DIR` | | Compare the report with an earlier run (see [Comparing with a baseline](#comparing-with-a-baseline)) |

Arca runs its production image with its default configuration, so `[storage] fsync = true`.
The suite speaks plain HTTP: TLS cost is measured by `bin/perf-test --tls`.

### Variants

| Variant | Arca | MinIO |
|---|---|---|
| `plain` | default configuration | default configuration |
| `sse` | SSE-S3 with a local master key (`--encryption`) | SSE-S3 with a static KMS key; warp sends the SSE-S3 header to both |
| `compress` | zstd compression on every benchmark bucket | compression on for every object (`MINIO_COMPRESSION_*`) |

warp uploads random data, which does not compress: the `compress` variant measures what
compression costs, not what it saves.

### What it measures

- **warp scenarios** (both servers): small PUT, GET and HEAD at several concurrencies, 64 MiB
  PUT and GET, ranged GET, multipart upload, a mixed workload, DeleteObjects, ListObjectsV2
  and a versioned mixed workload. Every scenario gets a bucket of its own.
- **Listing walks** (both servers): a tree of keys (10 prefixes of 10 sub-prefixes each) is
  seeded, then full ListObjectsV2 walks are timed with and without a delimiter, on a leaf
  prefix, on a prefix that matches nothing, and ListObjectVersions.
- **boto3 suite** (both servers): `tests/perf/perf_test.py`, latencies from the SDK side.
- **Maintenance** (Arca only): a dataset is seeded (objects of mixed sizes, a versioned
  bucket, multipart objects), the server is stopped, and `arca fsck`,
  `arca fsck --verify-checksums`, `arca gc` (dry run), `arca compress-existing` (plain
  variant), `arca recover --dry-run` and `arca recover --skip-verify` are timed against its
  volume, with each command's peak memory. The `perf-runner` image carries the `arca` binary
  of the image under test, so these are the same build as the S3 figures.

### The run directory

```
perf-results/20261018T090000Z-08c53c4/
├── report.html          the report
├── summary.json         every figure of the report, machine-readable
├── run.json             commit, profile, host, Docker resources, images
├── scenarios.tsv        the scenarios and their exact warp arguments
└── arca-plain/          one directory per server and variant
    ├── warp/<scenario>.json.zst   warp benchdata (warp analyze <file> re-reads it)
    ├── warp/<scenario>.log        warp's own output
    ├── listing.json, boto3.json, boto3.log
    ├── seed.json, maintenance.json        Arca only
    ├── metrics.txt                        Arca's /admin/metrics after the run
    └── server.log
```

Nothing is discarded: the report links every raw file, and `bin/test perf-report` covers
the report generator (`tests/perf/report.py`). Regenerate the report of a finished run,
for instance after editing the generator, with:

```bash
bin/perf-test report perf-results/<run>
```

It runs the generator in a plain Python container: no server, no compose project and no
volume, so it is safe to run while a suite is in progress.

### Comparing with a baseline

A targeted run (`--targets arca --variants plain --scenarios list-c16`, say, to measure
one change) has no MinIO of its own and nothing to compare its figures with. Give it an
earlier run as a baseline, either when the run is made or afterwards:

```bash
bin/perf-test suite --targets arca --variants plain --scenarios list-c16 \
    --baseline perf-results/<earlier-run>
bin/perf-test report perf-results/<run> --baseline perf-results/<earlier-run>
```

The report still covers only the run's own scenarios and servers. For each of them it adds:

- **a third bar**, Arca in the baseline, between Arca and MinIO, and under the ratio to
  MinIO the change since the baseline. A change within ±3% is shown as noise, not as a
  gain or a loss;
- **MinIO from the baseline** when the run did not measure it, labelled *MinIO
  (baseline)*. The headline share of MinIO then uses those figures too, and says so;
- **the baseline's rows** in the scenario details, and its figures and the change in the
  listing, maintenance and boto3 tables (shorter is better for seconds, higher for
  throughput);
- **a warning** when the two runs were not made the same way: host, Docker resources,
  profile, duration, storage, MinIO image or dataset sizes. The changes are then not like
  for like.

The baseline is only read. It is mounted under its own name, and the report links its
`report.html` as `../<earlier-run>/report.html`, which resolves when both runs are in
`perf-results/`. `summary.json` records the comparison in `baseline`,
`baseline_comparisons`, `baseline_headline` and `baseline_mismatches`.

### Reading the numbers

The headline is Arca's throughput as a share of MinIO's, the geometric mean over the
scenarios of a variant. The figures depend on the host, the container runtime and the disk:
compare runs made on the same machine only. On macOS the servers run in a Linux VM whose
virtual disk has its own fsync cost, so absolute numbers there say little about a Linux
server; the Arca/MinIO ratio and the before/after difference of a change are what to read.

### Third-party tools

warp and MinIO are AGPL-3.0 programs. The suite runs them as separate containers next to
Arca; they are never linked into, built into or distributed with Arca. MinIO no longer
publishes server images on Docker Hub: the default is Chainguard's build of upstream MinIO
(`cgr.dev/chainguard/minio`), and `PERF_MINIO_IMAGE` selects another one.

## Quick boto3 check

```bash
bin/perf-test                        # against the Arca you are running
bin/perf-test --encryption           # self-contained, data in memory
bin/perf-test --tls                  # self-contained, TLS with a throwaway CA
```

It reports per-operation latencies and two rates: `ops_per_sec`, the rate of one client
(operations over their summed latencies, kept for comparison with older results), and
`wall_ops_per_sec`, the rate the server sustained (operations over elapsed time).
