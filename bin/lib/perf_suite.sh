# Performance suite for bin/perf-test (`bin/perf-test suite`).
#
# Runs the same set of warp scenarios against Arca and, as a reference,
# MinIO, each as a fresh server on a fresh volume in a compose project of its
# own (use_test_project perf-suite), with the plain, SSE-S3 and compression
# variants. Arca also gets the listing, boto3 and offline maintenance
# benchmarks. Everything lands in one run directory, raw outputs included,
# and tests/perf/report.py turns it into report.html and summary.json.
#
# Source after bin/lib/compose.sh, then call perf_suite_main "$@".

PERF_TARGETS_ALL="arca minio"
PERF_VARIANTS_ALL="plain sse compress"

# The warp scenarios. One row per scenario:
#   profiles<TAB>name<TAB>group<TAB>description<TAB>warp arguments
# A row runs in every profile its first column lists. bin/perf-test adds the
# host, the credentials, the bucket (one per scenario), the benchdata path and,
# unless the row sets one, the profile's --duration.
_PERF_SCENARIOS='full	put-1k-c1	Small PUT	PUT 1 KiB, 1 client	put --obj.size 1KiB --concurrent 1
quick full	put-1k-c16	Small PUT	PUT 1 KiB, 16 clients	put --obj.size 1KiB --concurrent 16
full	put-1k-c64	Small PUT	PUT 1 KiB, 64 clients	put --obj.size 1KiB --concurrent 64
quick full	put-64k-c16	Small PUT	PUT 64 KiB, 16 clients	put --obj.size 64KiB --concurrent 16
full	put-64k-c64	Small PUT	PUT 64 KiB, 64 clients	put --obj.size 64KiB --concurrent 64
quick full	get-1k-c16	Small GET	GET 1 KiB, 16 clients	get --obj.size 1KiB --objects 10000 --concurrent 16
full	get-1k-c64	Small GET	GET 1 KiB, 64 clients	get --obj.size 1KiB --objects 10000 --concurrent 64
quick full	stat-1k-c16	Small GET	HEAD 1 KiB, 16 clients	stat --obj.size 1KiB --objects 10000 --concurrent 16
quick	put-64m-c4	Large	PUT 64 MiB, 4 clients	put --obj.size 64MiB --concurrent 4
full	put-64m-c4	Large	PUT 64 MiB, 4 clients	put --obj.size 64MiB --concurrent 4 --duration 30s
full	put-64m-c16	Large	PUT 64 MiB, 16 clients	put --obj.size 64MiB --concurrent 16 --duration 30s
quick	get-64m-c4	Large	GET 64 MiB, 4 clients	get --obj.size 64MiB --objects 16 --concurrent 4
full	get-64m-c4	Large	GET 64 MiB, 4 clients	get --obj.size 64MiB --objects 32 --concurrent 4
full	get-64m-c16	Large	GET 64 MiB, 16 clients	get --obj.size 64MiB --objects 64 --concurrent 16
full	get-64m-range-c16	Large	Ranged GET (1 MiB of 64 MiB), 16 clients	get --obj.size 64MiB --objects 32 --range-size 1MiB --concurrent 16
quick	multipart-put-256m-c4	Large	Multipart PUT 256 MiB (32 x 8 MiB), 4 clients	multipart-put --parts 32 --part.size 8MiB --part.concurrent 8 --concurrent 4
full	multipart-put-1g-c4	Large	Multipart PUT 1 GiB (64 x 16 MiB), 4 clients	multipart-put --parts 64 --part.size 16MiB --part.concurrent 8 --concurrent 4 --duration 30s
quick full	mixed-64k-c32	Mixed	Mixed GET/HEAD/PUT/DELETE 64 KiB, 32 clients	mixed --obj.size 64KiB --objects 2000 --concurrent 32
quick	delete-1k-c16	Delete	DeleteObjects (100 keys per request), 16 clients	delete --obj.size 1KiB --objects 50000 --batch 100 --concurrent 16
full	delete-1k-c16	Delete	DeleteObjects (100 keys per request), 16 clients	delete --obj.size 1KiB --objects 200000 --batch 100 --concurrent 16
quick full	list-c16	List	ListObjectsV2 over 10,000 keys, 16 clients	list --obj.size 1KiB --objects 10000 --concurrent 16
quick full	versioned-4k-c16	Versioned	Versioned mixed 4 KiB, 16 clients	versioned --obj.size 4KiB --objects 1000 --concurrent 16'

# Per-profile settings: warp duration, objects of the listing tree and of the
# maintenance dataset, boto3 suite sizes.
_perf_profile_settings() {     # _perf_profile_settings <profile>
    case "$1" in
        quick) PERF_DURATION=10s; PERF_LISTING_OBJECTS=10000; PERF_SEED_OBJECTS=20000
               PERF_BOTO3_ARGS=(--threads 16 --objects 500 --mixed-ops 500 --list-objects 1000 --large-mb 50) ;;
        full)  PERF_DURATION=60s; PERF_LISTING_OBJECTS=100000; PERF_SEED_OBJECTS=200000
               PERF_BOTO3_ARGS=(--threads 16 --objects 5000 --mixed-ops 5000 --list-objects 5000 --large-mb 200) ;;
        *) echo "Error: unknown profile '$1' (quick or full)" >&2; return 1 ;;
    esac
}

# perf_scenarios <profile> [<name>,...]: the scenarios of a profile as
# name<TAB>group<TAB>description<TAB>warp arguments, optionally only the named ones.
perf_scenarios() {
    local profile="$1" only="${2:-}" profiles name rest
    while IFS=$'\t' read -r profiles name rest; do
        [[ " $profiles " == *" $profile "* ]] || continue
        [[ -z "$only" || ",$only," == *",$name,"* ]] || continue
        printf '%s\t%s\n' "$name" "$rest"
    done <<< "$_PERF_SCENARIOS"
}

# perf_warp_args <warp arguments> <duration> <bucket> <benchdata> <variant>:
# the full argument list of one warp run, one argument per line.
perf_warp_args() {
    local args="$1" duration="$2" bucket="$3" benchdata="$4" variant="$5"
    local -a out
    read -r -a out <<< "$args"
    [[ " $args " == *" --duration "* ]] || out+=(--duration "$duration")
    out+=(--bucket "$bucket" --benchdata "$benchdata" --lookup path --no-color)
    [[ "$variant" == sse ]] && out+=(--sse-s3-encrypt)
    printf '%s\n' "${out[@]}"
}

# The compose command of the suite: the variant's features, the generated
# config (always mounted, the maintenance commands read it too) and the
# suite overlays.
perf_compose_cmd() {
    local cmd
    cmd="$(compose_cmd)"
    [[ "$cmd" == *docker-compose.config.yml* ]] ||
        cmd="$cmd -f $REPO_ROOT/docker/docker-compose.config.yml"
    cmd="$cmd -f $REPO_ROOT/docker/docker-compose.perf-suite.yml"
    [[ "$PERF_STORAGE" == tmpfs ]] &&
        cmd="$cmd -f $REPO_ROOT/docker/docker-compose.perf-suite-tmpfs.yml"
    # Dozens of `compose run` per server: keep compose's own progress lines
    # out of the suite's output (errors still show).
    echo "$cmd --progress quiet"
}

# Runner steps. _perf_runner_as_user writes into the results directory as the
# host user; _perf_runner runs as the arca uid (the maintenance benchmark).
# `compose run` reads its stdin: without </dev/null it would swallow the
# scenario list the warp loop reads.
_perf_runner() {
    $COMPOSE --profile perf-suite run --rm --no-deps -T "$@" < /dev/null
}
_perf_runner_as_user() {
    _perf_runner -u "$PERF_UID:$PERF_GID" "$@"
}

_perf_usage() {
    cat <<EOF
Usage: bin/perf-test suite [options]

Benchmark Arca against MinIO with warp, plus listing, boto3 and offline
maintenance timings, and write an HTML report with every raw output.

Options:
  --profile quick|full     Scenario set and sizes (default: full; quick takes minutes)
  --targets LIST           Comma-separated: arca,minio (default: both)
  --variants LIST          Comma-separated: plain,sse,compress (default: all)
  --scenarios LIST         Only these warp scenarios (names from --list)
  --storage volume|tmpfs   Server data on a named volume (default) or in memory
  --release TAG            Benchmark a published Arca image instead of the local build
  --build                  Rebuild the Arca image first
  --no-maintenance         Skip the offline maintenance benchmark
  --no-boto3               Skip the boto3 suite
  --no-listing             Skip the listing walks
  --out DIR                Run directory (default: perf-results/<UTC time>-<commit>)
  --baseline DIR           Compare the report with an earlier run directory
  --list                   Print the scenarios of the profile and exit
EOF
}

perf_suite_main() {
    local profile=full targets="arca,minio" variants="plain,sse,compress" only=""
    local build=false maintenance=true boto3=true listing=true out="" list=false baseline=""
    PERF_STORAGE=volume
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --profile)   profile="${2:?--profile needs a value}"; shift 2 ;;
            --targets)   targets="${2:?--targets needs a value}"; shift 2 ;;
            --variants)  variants="${2:?--variants needs a value}"; shift 2 ;;
            --scenarios) only="${2:?--scenarios needs a value}"; shift 2 ;;
            --storage)   PERF_STORAGE="${2:?--storage needs a value}"; shift 2 ;;
            --release)   use_release "${2:?--release needs a tag}" || return 1; shift 2 ;;
            --build)     build=true; shift ;;
            --no-maintenance) maintenance=false; shift ;;
            --no-boto3)  boto3=false; shift ;;
            --no-listing) listing=false; shift ;;
            --out)       out="${2:?--out needs a directory}"; shift 2 ;;
            --baseline)  baseline="${2:?--baseline needs a directory}"; shift 2 ;;
            --list)      list=true; shift ;;
            -h|--help)   _perf_usage; return 0 ;;
            *) echo "Error: unknown option '$1'" >&2; _perf_usage >&2; return 1 ;;
        esac
    done
    _perf_profile_settings "$profile" || return 1
    if [[ -n "$baseline" ]]; then
        baseline="$(_perf_run_dir "$baseline")" || return 1
    fi
    case "$PERF_STORAGE" in volume|tmpfs) ;; *)
        echo "Error: --storage must be volume or tmpfs" >&2; return 1 ;; esac
    local t v
    for t in ${targets//,/ }; do
        [[ " $PERF_TARGETS_ALL " == *" $t "* ]] || { echo "Error: unknown target '$t'" >&2; return 1; }
    done
    for v in ${variants//,/ }; do
        [[ " $PERF_VARIANTS_ALL " == *" $v "* ]] || { echo "Error: unknown variant '$v'" >&2; return 1; }
    done
    local scenarios
    scenarios="$(perf_scenarios "$profile" "$only")"
    if [[ -z "$scenarios" ]]; then
        echo "Error: no scenario selected" >&2
        return 1
    fi
    if $list; then
        printf '%s\n' "$scenarios" | cut -f1,3
        return 0
    fi

    use_test_project perf-suite
    export BUILD_TARGET=production
    local sha run_id
    sha="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    run_id="$(date -u +%Y%m%dT%H%M%SZ)-$sha"
    PERF_OUT="${out:-$REPO_ROOT/perf-results/$run_id}"
    mkdir -p "$PERF_OUT"
    PERF_OUT="$(cd "$PERF_OUT" && pwd)"
    export PERF_RESULTS_DIR="$PERF_OUT"
    PERF_UID="$(id -u)"; PERF_GID="$(id -g)"
    export PERF_UID PERF_GID

    { printf 'name\tgroup\tdescription\twarp\n'; printf '%s\n' "$scenarios"; } \
        > "$PERF_OUT/scenarios.tsv"

    echo "==> Performance suite: profile $profile, storage $PERF_STORAGE"
    echo "    targets: $targets  variants: $variants"
    echo "    results: $PERF_OUT"

    build_config
    COMPOSE="$(perf_compose_cmd)"
    if $build && [[ -z "${ARCA_RELEASE:-}" ]]; then
        echo "==> Building the Arca image..."
        $COMPOSE build arca || return 1
    fi
    echo "==> Building the perf-runner image..."
    $COMPOSE --profile perf-suite build perf-runner || return 1

    _perf_write_run_json "$run_id" "$sha" "$profile" "$targets" "$variants"

    local failed=() rc
    for v in ${variants//,/ }; do
        for t in ${targets//,/ }; do
            ( _perf_run_server "$t" "$v" "$scenarios" "$maintenance" "$boto3" "$listing" )
            rc=$?
            (( rc == 0 )) || failed+=("$t-$v")
        done
    done

    _perf_finish_run_json
    echo "==> Generating the report..."
    COMPOSE="$(perf_compose_cmd)"
    if [[ -n "$baseline" ]]; then
        _perf_runner_as_user -v "$baseline:$(_perf_baseline_mount "$baseline"):ro" \
            perf-runner report /results --baseline "$(_perf_baseline_mount "$baseline")"
    else
        _perf_runner_as_user perf-runner report /results
    fi || failed+=(report)
    # The runner mounts the suite's data volume: remove it again.
    $COMPOSE --profile perf-suite down -v >/dev/null 2>&1 || true

    echo ""
    echo "Report: $PERF_OUT/report.html"
    if [[ ${#failed[@]} -gt 0 ]]; then
        echo "Steps with failures: ${failed[*]} (see the logs in the run directory)" >&2
        return 1
    fi
}

# _perf_run_dir <dir>: the absolute path of a finished run directory, or an
# error when <dir> holds no run.json.
_perf_run_dir() {
    if [[ ! -f "$1/run.json" ]]; then
        echo "Error: $1 is not a run directory (no run.json)" >&2
        return 1
    fi
    (cd "$1" && pwd)
}

# Where a baseline run is mounted: under its own name, which the report shows
# and links (../<name>/report.html, the runs being siblings in perf-results/).
_perf_baseline_mount() {
    echo "/baselines/${1##*/}"
}

# bin/perf-test report <run-dir> [--baseline DIR]: regenerate the report of a
# finished run. A plain Python container (the report needs only the standard
# library): no compose project, no server and no volume, so it can run while a
# suite is in progress.
perf_report_main() {
    local run="" baseline=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --baseline) baseline="${2:?--baseline needs a directory}"; shift 2 ;;
            -h|--help)  run=""; break ;;
            -*) echo "Error: unknown option '$1'" >&2; run=""; break ;;
            *)  run="$1"; shift ;;
        esac
    done
    if [[ -z "$run" ]]; then
        echo "Usage: bin/perf-test report <run-dir> [--baseline <run-dir>]" >&2
        return 1
    fi
    run="$(_perf_run_dir "$run")" || return 1
    local -a cmd=(docker run --rm -u "$(id -u):$(id -g)" -e PYTHONDONTWRITEBYTECODE=1
                  -v "$REPO_ROOT/tests/perf:/perf:ro" -v "$run:/results")
    if [[ -n "$baseline" ]]; then
        baseline="$(_perf_run_dir "$baseline")" || return 1
        local mount
        mount="$(_perf_baseline_mount "$baseline")"
        cmd+=(-v "$baseline:$mount:ro" python:3.14-slim python /perf/report.py /results
              --baseline "$mount")
    else
        cmd+=(python:3.14-slim python /perf/report.py /results)
    fi
    "${cmd[@]}" || return 1
    echo "Report: $run/report.html"
}

# Run metadata: what was measured, where and with what.
_perf_write_run_json() {   # <run id> <sha> <profile> <targets> <variants>
    local dirty=false docker_info
    [[ -n "$(git status --porcelain 2>/dev/null)" ]] && dirty=true
    docker_info="$(docker info --format '{{json .}}' 2>/dev/null)"
    [[ -n "$docker_info" ]] || docker_info='{}'
    jq -n --arg run_id "$1" --arg sha "$2" --arg profile "$3" \
        --arg targets "$4" --arg variants "$5" --arg storage "$PERF_STORAGE" \
        --arg duration "$PERF_DURATION" --argjson dirty "$dirty" \
        --arg describe "$(git describe --tags --always --dirty 2>/dev/null || echo unknown)" \
        --arg release "${ARCA_RELEASE:-}" --arg host "$(uname -srm)" \
        --arg minio_image "${PERF_MINIO_IMAGE:-cgr.dev/chainguard/minio:latest}" \
        --argjson docker "$docker_info" \
        --arg started "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --argjson listing "$PERF_LISTING_OBJECTS" --argjson seed "$PERF_SEED_OBJECTS" \
        '{run_id: $run_id, git_sha: $sha, git_describe: $describe, dirty: $dirty,
          profile: $profile, targets: ($targets | split(",")), variants: ($variants | split(",")),
          storage: $storage, duration: $duration, arca_release: $release,
          minio_image: $minio_image, listing_objects: $listing, maintenance_objects: $seed,
          host: $host,
          docker: ($docker | {ServerVersion, OperatingSystem, KernelVersion, Architecture,
                              NCPU, MemTotal, Driver}),
          started: $started}' > "$PERF_OUT/run.json"
}

_perf_finish_run_json() {
    local tmp="$PERF_OUT/run.json.tmp"
    jq --arg finished "$(date -u +%Y-%m-%dT%H:%M:%SZ)" '. + {finished: $finished}' \
        "$PERF_OUT/run.json" > "$tmp" && mv "$tmp" "$PERF_OUT/run.json"
}

# One server under test (target + variant), in a subshell: its features,
# config and compose project are its own, and it is torn down on exit.
_perf_run_server() {    # <target> <variant> <scenarios> <maintenance> <boto3> <listing>
    local target="$1" variant="$2" scenarios="$3" maintenance="$4" boto3="$5" listing="$6"
    local combo="$target-$variant" endpoint=http://arca:9000 compression="" failed=0
    # Not local: the EXIT trap runs after this function has returned (the
    # subshell keeps them from leaking into the next server's run).
    dir="$PERF_OUT/$target-$variant" service=arca
    mkdir -p "$dir/warp"

    if [[ "$target" == arca ]]; then
        [[ "$variant" == sse ]] && enable_encryption
        [[ "$variant" == compress ]] && compression=zstd
    else
        service=minio endpoint=http://minio:9000
        [[ "$variant" == compress ]] && export PERF_MINIO_COMPRESSION=on
    fi
    build_config
    COMPOSE="$(perf_compose_cmd)"

    # shellcheck disable=SC2329  # invoked by the trap
    _perf_cleanup_server() {
        $COMPOSE logs --no-color "$service" > "$dir/server.log" 2>&1 || true
        $COMPOSE --profile perf-suite down -v >/dev/null 2>&1 || true
    }
    trap _perf_cleanup_server EXIT

    echo ""
    echo "==> [$combo] starting $service..."
    $COMPOSE --profile perf-suite down -v >/dev/null 2>&1 || true
    $COMPOSE --profile perf-suite up -d "$service" || return 1
    _perf_runner -e "ARCA_ENDPOINT=$endpoint" perf-runner wait --timeout 90 || {
        echo "==> [$combo] the server did not become ready" >&2
        return 1
    }

    local name group desc args bucket
    local -a warp_args
    while IFS=$'\t' read -r name group desc args; do
        bucket="perf-$name"
        _perf_runner -e "ARCA_ENDPOINT=$endpoint" perf-runner prepare-bucket \
            --bucket "$bucket" ${compression:+--compression "$compression"} || {
            echo "==> [$combo] could not prepare bucket $bucket" >&2; failed=1; continue; }
        mapfile -t warp_args < <(perf_warp_args "$args" "$PERF_DURATION" "$bucket" \
            "/results/$combo/warp/$name" "$variant")
        echo "==> [$combo] $name: $desc"
        if ! $COMPOSE --profile perf-suite run --rm --no-deps -T \
                -e "WARP_HOST=${endpoint#http://}" \
                -e "WARP_ACCESS_KEY=${ARCA_ROOT_ACCESS_KEY:-AKIA5B6BSHJA8CIHZSVG}" \
                -e "WARP_SECRET_KEY=${ARCA_ROOT_SECRET_KEY:-hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv}" \
                warp "${warp_args[@]}" < /dev/null > "$dir/warp/$name.log" 2>&1; then
            echo "==> [$combo] $name failed, see $dir/warp/$name.log" >&2
            failed=1
        fi
        grep -E '^ \* Average' "$dir/warp/$name.log" | head -1 || true
    done <<< "$scenarios"

    if $listing; then
        echo "==> [$combo] listing walks ($PERF_LISTING_OBJECTS keys)..."
        _perf_runner_as_user -e "ARCA_ENDPOINT=$endpoint" perf-runner listing \
            --objects "$PERF_LISTING_OBJECTS" --out "/results/$combo/listing.json" ||
            { echo "==> [$combo] listing failed" >&2; failed=1; }
    fi

    if $boto3; then
        echo "==> [$combo] boto3 suite..."
        _perf_runner_as_user -e "ARCA_ENDPOINT=$endpoint" --entrypoint python perf-runner \
            /perf/perf_test.py --endpoint "$endpoint" "${PERF_BOTO3_ARGS[@]}" -q \
            --json-out "/results/$combo/boto3.json" > "$dir/boto3.log" 2>&1 ||
            { echo "==> [$combo] boto3 suite failed, see $dir/boto3.log" >&2; failed=1; }
    fi

    if [[ "$target" == arca ]]; then
        _perf_runner_as_user perf-runner metrics --out "/results/$combo/metrics.txt" ||
            echo "==> [$combo] could not fetch the metrics" >&2
        if $maintenance; then
            _perf_maintenance "$combo" "$dir" "$variant" || failed=1
        fi
    fi
    return $failed
}

# Seed the maintenance dataset on the running Arca, stop it, and time the
# offline commands against its volume.
_perf_maintenance() {   # <combo> <dir> <variant>
    local combo="$1" dir="$2" variant="$3"
    echo "==> [$combo] seeding the maintenance dataset ($PERF_SEED_OBJECTS objects)..."
    _perf_runner_as_user perf-runner seed --objects "$PERF_SEED_OBJECTS" \
        --out "/results/$combo/seed.json" || { echo "==> [$combo] seeding failed" >&2; return 1; }
    $COMPOSE stop arca || return 1
    echo "==> [$combo] maintenance commands (server stopped)..."
    _perf_runner perf-runner maintenance --variant "$variant" > "$dir/maintenance.json" ||
        { echo "==> [$combo] a maintenance step failed, see $dir/maintenance.json" >&2; return 1; }
}
