#!/usr/bin/env bash
# Tests for the performance suite (`bin/perf-test suite`, bin/lib/perf_suite.sh):
# the scenario catalogue, the warp arguments, the compose command, and a whole
# run with `docker` replaced on PATH by a recorder (nothing here talks to a
# Docker daemon). Run them with `bin/test scripts`.

set -uo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"

passed=0
failed=0

run_test() {
    local output
    if output="$( ("$1") 2>&1 )"; then
        echo "  PASS  $1"
        passed=$((passed + 1))
    else
        echo "  FAIL  $1"
        echo "$output" | sed 's/^/        /'
        failed=$((failed + 1))
    fi
}

assert_eq() {
    if [[ "$1" != "$2" ]]; then
        echo "${3:-assertion failed}"
        echo "  expected: $(printf '%q' "$1")"
        echo "  actual:   $(printf '%q' "$2")"
        return 1
    fi
}

assert_contains() {
    if [[ "$1" != *"$2"* ]]; then
        echo "expected to find: $2"
        echo "in: $1"
        return 1
    fi
}

assert_not_contains() {
    if [[ "$1" == *"$2"* ]]; then
        echo "expected NOT to find: $2"
        echo "in: $1"
        return 1
    fi
}

# The library sourced from a scratch repository.
lib_setup() {
    cd "$(mktemp -d)" || exit 1
    git init -q
    mkdir -p docker config
    cp -R "$REPO/config/fragments" config/
    # shellcheck source=/dev/null
    source "$REPO/bin/lib/compose.sh"
    # shellcheck source=/dev/null
    source "$REPO/bin/lib/perf_suite.sh"
}

# --- scenario catalogue --------------------------------------------------------

test_every_scenario_row_has_five_fields() {
    lib_setup
    local bad
    bad="$(awk -F'\t' 'NF != 5' <<< "$_PERF_SCENARIOS")"
    assert_eq "" "$bad" "malformed scenario rows"
}

test_scenario_names_are_unique_within_a_profile() {
    lib_setup
    local p dup
    for p in quick full; do
        dup="$(perf_scenarios "$p" | cut -f1 | sort | uniq -d)"
        assert_eq "" "$dup" "duplicate names in $p" || return 1
    done
}

test_the_quick_profile_covers_every_group_of_the_full_one() {
    lib_setup
    assert_eq "$(perf_scenarios full | cut -f2 | sort -u)" \
        "$(perf_scenarios quick | cut -f2 | sort -u)"
}

test_the_full_profile_has_the_concurrency_sweep() {
    lib_setup
    local names
    names="$(perf_scenarios full | cut -f1 | tr '\n' ' ')"
    assert_contains "$names" "put-1k-c1 " &&
    assert_contains "$names" "put-1k-c64 " &&
    assert_not_contains "$(perf_scenarios quick | cut -f1 | tr '\n' ' ')" "put-1k-c64 "
}

test_scenarios_can_be_selected_by_name() {
    lib_setup
    assert_eq "put-1k-c16
list-c16" "$(perf_scenarios quick put-1k-c16,list-c16 | cut -f1)"
}

test_every_scenario_names_a_warp_benchmark() {
    lib_setup
    local op bad=""
    while read -r op; do
        case "$op" in put|get|stat|mixed|delete|list|versioned|multipart-put) ;;
            *) bad+="$op " ;; esac
    done < <(perf_scenarios full | cut -f4 | awk '{print $1}')
    assert_eq "" "$bad" "unknown warp benchmarks"
}

# --- warp arguments ------------------------------------------------------------

test_warp_args_add_the_profile_duration() {
    lib_setup
    local args
    args="$(perf_warp_args "put --obj.size 1KiB" 60s perf-x /results/a/warp/x plain | tr '\n' ' ')"
    assert_contains "$args" "put --obj.size 1KiB --duration 60s --bucket perf-x" &&
    assert_contains "$args" "--benchdata /results/a/warp/x --lookup path --no-color" &&
    assert_not_contains "$args" "--sse-s3-encrypt"
}

test_a_scenario_duration_wins_over_the_profile_one() {
    lib_setup
    local args
    args="$(perf_warp_args "put --duration 30s" 60s b d plain | tr '\n' ' ')"
    assert_contains "$args" "--duration 30s" &&
    assert_not_contains "$args" "60s"
}

test_the_sse_variant_sends_the_sse_header() {
    lib_setup
    assert_contains "$(perf_warp_args "put" 10s b d sse)" "--sse-s3-encrypt"
}

# --- compose command -----------------------------------------------------------

test_the_compose_command_always_mounts_the_generated_config() {
    lib_setup
    use_test_project perf-suite
    PERF_STORAGE=volume
    local cmd
    cmd="$(perf_compose_cmd)"
    assert_contains "$cmd" "-p arca-test-perf-suite " &&
    assert_contains "$cmd" "docker-compose.config.yml" &&
    assert_contains "$cmd" "docker-compose.perf-suite.yml" &&
    assert_not_contains "$cmd" "perf-suite-tmpfs"
}

test_the_config_overlay_is_not_added_twice() {
    lib_setup
    use_test_project perf-suite
    PERF_STORAGE=volume
    enable_encryption
    assert_eq 1 "$(perf_compose_cmd | grep -o 'docker-compose.config.yml' | wc -l | tr -d ' ')"
}

test_tmpfs_storage_adds_the_tmpfs_overlay() {
    lib_setup
    PERF_STORAGE=tmpfs
    assert_contains "$(perf_compose_cmd)" "docker-compose.perf-suite-tmpfs.yml"
}

# --- a whole run, docker mocked ------------------------------------------------

# A scratch copy of the repository's scripts with a `docker` recorder: one
# line per call. MOCK_FAIL_WARP=<scenario> makes that warp run fail.
script_setup() {
    cd "$(mktemp -d)" || exit 1
    git init -q
    cp -R "$REPO/bin" .
    mkdir -p config docker mockbin
    cp -R "$REPO/config/fragments" config/
    cp "$REPO"/docker/docker-compose*.yml docker/
    echo "developer config" > config/.generated.toml
    printf 'FEATURES="tls"\nBUILD_TARGET="development"\n' > .arca-env
    cp .arca-env .arca-env.orig
    export DOCKER_LOG="$PWD/docker.log"
    : > "$DOCKER_LOG"
    cat > mockbin/docker <<'EOF'
#!/usr/bin/env bash
echo "$* | config=${ARCA_GENERATED_CONFIG:-} port=${ARCA_HOST_PORT:-} out=${PERF_RESULTS_DIR:-} minio_compression=${PERF_MINIO_COMPRESSION:-}" >> "$DOCKER_LOG"
case "$*" in
    # `compose run` reads its stdin, like the real one does.
    *" run "*) [[ -t 0 ]] || cat > /dev/null ;;&
    *" warp "*) [[ -n "${MOCK_FAIL_WARP:-}" && "$*" == *"--bucket perf-$MOCK_FAIL_WARP "* ]] && exit 1 ;;
esac
exit 0
EOF
    chmod +x mockbin/docker
    export PATH="$PWD/mockbin:$PATH"
}

compose_calls() { grep '^compose ' "$DOCKER_LOG"; }

run_suite() {   # run_suite <bin/perf-test suite args...>
    bin/perf-test suite "$@" < /dev/null > run.log 2>&1
}

test_a_run_stays_in_its_own_project_and_leaves_the_developer_stack_alone() {
    script_setup
    run_suite --profile quick --out "$PWD/out" || { cat run.log; return 1; }
    local stray
    stray="$(compose_calls | grep -v '^compose -p arca-test-perf-suite ' || true)"
    assert_eq "" "$stray" "compose calls outside the suite's project" &&
    assert_eq "developer config" "$(cat config/.generated.toml)" &&
    assert_eq "$(cat .arca-env.orig)" "$(cat .arca-env)" &&
    assert_eq "" "$(compose_calls | grep -v 'port=127.0.0.1:0 ' || true)" \
        "compose calls with a fixed host port"
}

test_every_server_runs_every_scenario_in_its_own_bucket() {
    script_setup
    run_suite --profile quick --targets arca,minio --variants plain --out "$PWD/out" ||
        { cat run.log; return 1; }
    local n
    n="$(perf_scenario_count quick)"
    assert_eq "$n" "$(compose_calls | grep -c ' warp put\| warp get\| warp stat\| warp mixed\| warp delete\| warp list\| warp versioned\| warp multipart-put' | tr -d ' ' | awk -v n="$n" '{print $1/2}')" &&
    assert_contains "$(compose_calls | grep ' warp ' | head -1)" "--bucket perf-put-1k-c16 " &&
    assert_contains "$(compose_calls | grep ' warp ' | head -1)" "--benchdata /results/arca-plain/warp/put-1k-c16 "
}

perf_scenario_count() {
    (
        # shellcheck source=/dev/null
        source bin/lib/compose.sh; source bin/lib/perf_suite.sh
        perf_scenarios "$1" | wc -l | tr -d ' '
    )
}

test_each_server_is_torn_down_with_its_volumes() {
    script_setup
    run_suite --profile quick --variants plain,sse --out "$PWD/out" || { cat run.log; return 1; }
    # Before and after each of the 4 servers a clean start and a teardown,
    # and one more after the report.
    assert_eq 9 "$(compose_calls | grep -c -- '--profile perf-suite down -v')"
}

test_the_sse_variant_enables_arca_encryption_in_the_suite_config() {
    script_setup
    run_suite --profile quick --targets arca --variants sse --scenarios put-1k-c16 \
        --out "$PWD/out" || { cat run.log; return 1; }
    assert_contains "$(cat config/.generated-test-perf-suite.toml)" "[encryption]" &&
    assert_contains "$(compose_calls | grep ' warp ')" "--sse-s3-encrypt"
}

test_the_compress_variant_compresses_on_both_servers() {
    script_setup
    run_suite --profile quick --variants compress --scenarios put-1k-c16 \
        --out "$PWD/out" || { cat run.log; return 1; }
    assert_contains "$(compose_calls | grep 'prepare-bucket' | head -1)" "--compression zstd" &&
    assert_contains "$(compose_calls | grep ' up -d minio')" "minio_compression=on" &&
    assert_not_contains "$(compose_calls | grep ' up -d arca')" "minio_compression=on"
}

test_nothing_is_left_behind_after_the_report() {
    # The report step mounts the suite's volumes again: the last call tears
    # them down.
    script_setup
    run_suite --profile quick --variants plain --scenarios put-1k-c16 \
        --out "$PWD/out" || { cat run.log; return 1; }
    assert_contains "$(compose_calls | tail -1)" "--profile perf-suite down -v"
}

test_maintenance_runs_on_arca_only_with_the_server_stopped() {
    script_setup
    run_suite --profile quick --variants plain --scenarios put-1k-c16 \
        --out "$PWD/out" || { cat run.log; return 1; }
    local calls
    calls="$(compose_calls)"
    assert_eq 1 "$(grep -c 'perf-runner maintenance' <<< "$calls")" &&
    # stop comes before the maintenance step.
    assert_contains "$(grep -E ' stop arca|perf-runner maintenance' <<< "$calls" | head -1)" " stop arca"
}

test_no_maintenance_skips_it() {
    script_setup
    run_suite --profile quick --targets arca --variants plain --scenarios put-1k-c16 \
        --no-maintenance --out "$PWD/out" || { cat run.log; return 1; }
    assert_not_contains "$(compose_calls)" "perf-runner maintenance" &&
    assert_not_contains "$(compose_calls)" "perf-runner seed"
}

test_a_failing_scenario_does_not_stop_the_run_but_fails_it() {
    script_setup
    MOCK_FAIL_WARP=put-1k-c16 run_suite --profile quick --targets arca --variants plain \
        --scenarios put-1k-c16,list-c16 --out "$PWD/out"
    local rc=$?
    assert_eq 1 "$rc" "exit code" &&
    assert_contains "$(compose_calls)" "--bucket perf-list-c16 " &&
    # The report is still generated.
    assert_contains "$(compose_calls)" "perf-runner report /results"
}

test_the_run_directory_records_the_run_and_the_scenarios() {
    script_setup
    run_suite --profile quick --targets arca --variants plain --scenarios put-1k-c16 \
        --out "$PWD/out" || { cat run.log; return 1; }
    assert_eq "quick" "$(jq -r .profile out/run.json)" &&
    assert_eq '["arca"]' "$(jq -c .targets out/run.json)" &&
    [[ "$(jq -r .finished out/run.json)" != null ]] &&
    assert_eq "name	group	description	warp" "$(head -1 out/scenarios.tsv)" &&
    assert_contains "$(compose_calls | grep ' warp ')" "out=$PWD/out "
}

test_results_are_written_as_the_host_user() {
    script_setup
    run_suite --profile quick --targets arca --variants plain --scenarios put-1k-c16 \
        --out "$PWD/out" || { cat run.log; return 1; }
    assert_contains "$(compose_calls | grep 'perf-runner report')" "-u $(id -u):$(id -g)"
}

test_unknown_options_are_rejected() {
    script_setup
    run_suite --targets arca,ceph && return 1
    assert_contains "$(cat run.log)" "unknown target 'ceph'" &&
    assert_eq "" "$(compose_calls || true)" "nothing may start on bad input"
}

# --- baseline and report ------------------------------------------------------

# A finished run directory, as `bin/perf-test report` and --baseline read it.
make_run() {   # make_run <dir>
    mkdir -p "$1/arca-plain/warp"
    echo '{"run_id": "x"}' > "$1/run.json"
}

test_a_baseline_is_mounted_read_only_under_its_own_name() {
    script_setup
    make_run "$PWD/runs/base-run"
    run_suite --profile quick --targets arca --variants plain --scenarios put-1k-c16 \
        --baseline runs/base-run --out "$PWD/out" || { cat run.log; return 1; }
    # bin/perf-test runs from the git toplevel, a resolved path (/private/var on macOS).
    local call
    call="$(compose_calls | grep 'perf-runner report')"
    assert_contains "$call" "-v $(pwd -P)/runs/base-run:/baselines/base-run:ro" &&
    assert_contains "$call" "perf-runner report /results --baseline /baselines/base-run"
}

test_a_baseline_that_is_not_a_run_is_rejected_before_anything_starts() {
    script_setup
    mkdir -p not-a-run
    run_suite --profile quick --baseline not-a-run --out "$PWD/out" && return 1
    assert_contains "$(cat run.log)" "not a run directory" &&
    assert_eq "" "$(compose_calls || true)" "nothing may start on bad input"
}

# The report of an existing run: a plain container, no compose project,
# no server, no volume, so it can run next to a suite in progress.
test_report_regenerates_a_run_in_a_plain_container() {
    script_setup
    make_run "$PWD/runs/now"
    make_run "$PWD/runs/then"
    bin/perf-test report runs/now --baseline runs/then > run.log 2>&1 || { cat run.log; return 1; }
    local call
    call="$(grep '^run ' "$DOCKER_LOG")"
    assert_eq "" "$(compose_calls || true)" "the report must not use compose" &&
    assert_contains "$call" "-v $(pwd -P)/runs/now:/results " &&
    assert_contains "$call" "-v $(pwd -P)/runs/then:/baselines/then:ro " &&
    assert_contains "$call" "-u $(id -u):$(id -g)" &&
    assert_contains "$call" "/perf/report.py /results --baseline /baselines/then"
}

test_report_without_a_baseline() {
    script_setup
    make_run "$PWD/runs/now"
    bin/perf-test report runs/now > run.log 2>&1 || { cat run.log; return 1; }
    assert_contains "$(grep '^run ' "$DOCKER_LOG")" "/perf/report.py /results" &&
    assert_not_contains "$(cat "$DOCKER_LOG")" "--baseline"
}

test_report_rejects_a_directory_that_is_not_a_run() {
    script_setup
    mkdir -p runs/empty
    bin/perf-test report runs/empty > run.log 2>&1 && return 1
    assert_contains "$(cat run.log)" "not a run directory" &&
    assert_eq "" "$(cat "$DOCKER_LOG")" "nothing may run on bad input"
}

test_report_needs_a_run_directory() {
    script_setup
    bin/perf-test report > run.log 2>&1 && return 1
    assert_contains "$(cat run.log)" "Usage: bin/perf-test report"
}

test_the_results_directory_is_ignored_by_git() {
    assert_contains "$(cat "$REPO/.gitignore")" "perf-results/"
}

# -----------------------------------------------------------------------------

echo "==> Performance suite (bin/perf-test suite, bin/lib/perf_suite.sh)"
for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
    run_test "$t"
done

echo ""
echo "$passed passed, $failed failed"
(( failed == 0 ))
