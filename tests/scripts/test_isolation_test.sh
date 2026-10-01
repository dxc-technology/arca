#!/usr/bin/env bash
# Tests for the isolation of the test suites from the developer's stack:
# self-contained `bin/test` modes and `bin/perf-test` run in a compose project
# of their own, with their own generated config and no fixed host port, and
# `bin/test integration` (which deliberately targets the developer's running
# server) enables the suites the server's recorded features support. Run them
# with `bin/test scripts`.
#
# Library tests source bin/lib/compose.sh inside a scratch git repository.
# Script tests run a copy of bin/ in a scratch repository with `docker` and
# `sleep` replaced on PATH by recorders: nothing here talks to a Docker daemon.

set -uo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LIB="$REPO/bin/lib/compose.sh"

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

# A scratch repository with the library sourced from it.
setup() {
    cd "$(mktemp -d)" || exit 1
    git init -q
    mkdir -p docker config
    cp -R "$REPO/config/fragments" config/
    # shellcheck source=/dev/null
    source "$LIB"
}

# --- compose project -----------------------------------------------------------

test_the_developer_stack_uses_the_default_project() {
    # bin/arca, bin/console and `bin/test integration` address the stack the
    # developer started: no project name of their own.
    setup
    load_env
    assert_not_contains "$(compose_cmd)" " -p "
}

test_a_test_project_names_the_compose_project() {
    setup
    use_test_project tls
    local cmd
    cmd="$(compose_cmd)"
    assert_eq "docker compose -p arca-test-tls -f " "${cmd:0:35}"
}

test_the_project_name_survives_feature_registration() {
    setup
    use_test_project kms
    enable_kms
    build_config
    assert_contains "$(compose_cmd)" "docker compose -p arca-test-kms -f"
}

# --- generated config ----------------------------------------------------------

test_a_test_project_writes_its_own_config() {
    setup
    echo "developer config" > config/.generated.toml
    use_test_project encryption
    enable_encryption
    build_config
    assert_eq "developer config" "$(cat config/.generated.toml)" \
        "the developer's generated config was overwritten" &&
    assert_eq "$PWD/config/.generated-test-encryption.toml" "$GENERATED_CONFIG" &&
    assert_eq "$GENERATED_CONFIG" "$ARCA_GENERATED_CONFIG" &&
    assert_contains "$(cat "$GENERATED_CONFIG")" "master_key"
}

test_the_config_overlay_mounts_the_selected_config() {
    local overlay
    overlay="$(cat "$REPO/docker/docker-compose.config.yml")"
    assert_contains "$overlay" '${ARCA_GENERATED_CONFIG:-../config/.generated.toml}:/etc/arca/config.toml:ro'
}

test_the_generated_test_configs_are_ignored_by_git() {
    assert_contains "$(cat "$REPO/.gitignore")" "config/.generated-test-*.toml"
}

# --- host ports ----------------------------------------------------------------

# Every compose file a self-contained mode can include: the base, the
# feature overlays compose_cmd adds and the perf overlay.
overlay_files() {
    local f
    for f in "$REPO"/docker/docker-compose.yml "$REPO"/docker/docker-compose.*.yml; do
        case "$(basename "$f")" in
            docker-compose.cluster*.yml|docker-compose.docs.yml) ;;   # never in a test project
            *) echo "$f" ;;
        esac
    done
}

test_no_service_a_test_starts_publishes_a_fixed_host_port() {
    # A fixed host port would collide with the developer's running server
    # (9000) or its KMS/PostgreSQL. The console and the screenshot services
    # are never started by a test mode.
    local f bad=""
    while IFS= read -r f; do
        bad+="$(awk -v file="$(basename "$f")" '
            /^  [a-z][a-z0-9-]*:/ { service = $1; sub(":", "", service); ports = 0; next }
            /^    ports:/ { ports = 1; next }
            /^    [a-z]/ { ports = 0 }
            ports && /^      - / && service != "console" && $0 !~ /\$\{[A-Z_]*_HOST_PORT:-/ {
                print file ": " service ": " $0
            }' "$f")"
    done < <(overlay_files)
    assert_eq "" "$bad" "host ports that a test project cannot remap"
}

test_a_test_project_moves_every_host_port_to_an_ephemeral_loopback_one() {
    setup
    use_test_project tls
    local f var missing=""
    while IFS= read -r var; do
        [[ "$var" == CONSOLE_HOST_PORT ]] && continue
        [[ "${!var:-}" == "127.0.0.1:0" ]] || missing+="$var=${!var:-<unset>} "
    done < <(overlay_files | xargs grep -ho '\${[A-Z_]*_HOST_PORT:-' | tr -d '${:-' | sort -u)
    assert_eq "" "$missing" "host port variables not remapped by use_test_project"
}

test_the_developer_stack_keeps_its_host_ports() {
    setup
    load_env
    assert_eq "" "${ARCA_HOST_PORT:-}"
}

# --- features of the running server (TD-031) -----------------------------------

feature_env_for() {    # feature_env_for <FEATURES value>
    printf 'FEATURES="%s"\nBUILD_TARGET="production"\n' "$1" > .arca-env
    load_env
    integration_feature_env | paste -sd' ' -
}

test_a_plain_server_enables_no_feature_suite() {
    setup
    assert_eq "" "$(feature_env_for "")"
}

test_global_encryption_enables_the_encryption_suite() {
    setup
    assert_eq "-e ARCA_ENCRYPTION_ENABLED=1" "$(feature_env_for "tls encryption")"
}

test_per_bucket_encryption_enables_the_per_bucket_suites() {
    # test_per_bucket_encryption.py and test_recrypt.py: key present, global off.
    setup
    assert_eq "-e ARCA_PER_BUCKET_ENCRYPTION=1" "$(feature_env_for "encryption-per-bucket")"
}

test_kms_enables_the_kms_and_global_encryption_suites() {
    setup
    assert_eq "-e ARCA_ENCRYPTION_ENABLED=1 -e ARCA_KMS_ENABLED=1" "$(feature_env_for "kms")"
}

test_kms_per_bucket_enables_the_per_bucket_suites_only() {
    # test_kms.py expects global encryption: not for a per-bucket server.
    setup
    assert_eq "-e ARCA_PER_BUCKET_ENCRYPTION=1" "$(feature_env_for "kms-per-bucket")"
}

test_other_features_enable_no_encryption_suite() {
    setup
    assert_eq "" "$(feature_env_for "tls postgres notifications")"
}

# --- bin/test and bin/perf-test, end to end with docker mocked -----------------

# A scratch copy of the repository's scripts, a sentinel developer state and
# a `docker` recorder: one line per call, plus the environment that selects
# the project's config and ports.
script_setup() {
    cd "$(mktemp -d)" || exit 1
    git init -q
    cp -R "$REPO/bin" .
    mkdir -p config docker mockbin
    cp -R "$REPO/config/fragments" config/
    cp "$REPO"/docker/docker-compose*.yml docker/
    echo "developer config" > config/.generated.toml
    printf 'FEATURES="tls encryption"\nBUILD_TARGET="development"\nARCA_CERTS_DIR="../certs/local"\n' > .arca-env
    cp .arca-env .arca-env.orig
    export DOCKER_LOG="$PWD/docker.log"
    : > "$DOCKER_LOG"
    cat > mockbin/docker <<'EOF'
#!/usr/bin/env bash
echo "$* | config=${ARCA_GENERATED_CONFIG:-} port=${ARCA_HOST_PORT:-}" >> "$DOCKER_LOG"
case "$*" in
    *" run "*)              [[ -n "${MOCK_FAIL_RUN:-}" ]] && exit 1 ;;&
    *" ps -q "*)            echo 0123456789ab ;;
    *" ps arca")            echo "arca  running" ;;
    inspect*)               echo healthy ;;
    *"--to-cluster"*)
        printf '[cluster]\nenabled = true\ncluster_id = "arca-0a1b2c"\nsecret = "%064d"\n# key_file\n' 0
        echo "Reconciled object_seq write counter" ;;
    *"--to-single --force"*) echo "Purged cluster-only state"; echo "VACUUM done" ;;
    *"--to-single"*)        exit 1 ;;
esac
exit 0
EOF
    printf '#!/bin/sh\nexit 0\n' > mockbin/sleep
    chmod +x mockbin/docker mockbin/sleep
    export PATH="$PWD/mockbin:$PATH"
}

compose_calls() { grep '^compose ' "$DOCKER_LOG"; }

# The run left the developer's stack alone: every compose call addressed
# <project>, and neither the generated config nor .arca-env changed.
assert_isolated() {     # assert_isolated <project>
    local stray
    stray="$(compose_calls | grep -v "^compose -p $1 " || true)"
    [[ -n "$(compose_calls)" ]] || { echo "no compose call recorded"; return 1; }
    assert_eq "" "$stray" "compose calls outside project $1" &&
    assert_eq "developer config" "$(cat config/.generated.toml)" \
        "config/.generated.toml was overwritten" &&
    assert_eq "$(cat .arca-env.orig)" "$(cat .arca-env)" ".arca-env was rewritten" &&
    assert_eq "" "$(compose_calls | grep -v 'port=127.0.0.1:0$' || true)" \
        "compose calls with a fixed host port"
}

run_mode() {    # run_mode <bin/test args...>
    bin/test "$@" > run.log 2>&1 || { echo "bin/test $* failed:"; cat run.log; return 1; }
}

# shellcheck disable=SC2329  # invoked indirectly, by name
isolated_mode_test() {  # isolated_mode_test <project suffix> <bin/test args...>
    local project="arca-test-$1"; shift
    script_setup
    run_mode "$@" &&
    assert_isolated "$project" &&
    assert_contains "$(compose_calls)" "compose -p $project " &&
    assert_contains "$(compose_calls | grep ' down ')" "-v"
}

test_tls_mode_is_isolated()                  { isolated_mode_test tls tls; }
test_encryption_mode_is_isolated()           { isolated_mode_test encryption encryption; }
test_compression_mode_is_isolated()          { isolated_mode_test compression compression; }
test_per_bucket_encryption_mode_is_isolated() { isolated_mode_test per-bucket-encryption per-bucket-encryption; }
test_recrypt_mode_is_isolated()              { isolated_mode_test recrypt recrypt; }
test_kms_mode_is_isolated()                  { isolated_mode_test kms kms; }
test_postgres_mode_is_isolated()             { isolated_mode_test postgres postgres; }
test_postgres_mode_runs_conditional_write_suite() {
    script_setup
    run_mode postgres &&
    assert_contains "$(compose_calls | grep ' run ')" \
        "integration/test_postgres.py integration/test_conditional_writes.py"
}
test_migrate_db_mode_is_isolated()           { isolated_mode_test migrate-db migrate-db; }
test_migrate_topology_mode_is_isolated()     { isolated_mode_test migrate-topology migrate-topology; }
test_replication_mode_is_isolated()          { isolated_mode_test replication replication; }
test_notifications_mode_is_isolated()        { isolated_mode_test notifications notifications; }

test_every_connector_mode_is_isolated() {
    local connector
    for connector in redis nats mqtt postgresql mysql mongodb kafka amqp \
            elasticsearch syslog smtp grpc; do
        isolated_mode_test "connector-$connector" connectors "$connector" ||
            { echo "connector: $connector"; return 1; }
    done
}

test_migrate_topology_edits_only_its_own_config() {
    # The mode appends the emitted [cluster] stanza to the generated config.
    script_setup
    run_mode migrate-topology &&
    assert_eq "developer config" "$(cat config/.generated.toml)" &&
    [[ -f config/.generated-test-migrate-topology.toml ]]
}

test_perf_test_self_contained_mode_is_isolated() {
    script_setup
    bin/perf-test --encryption > run.log 2>&1 || { cat run.log; return 1; }
    assert_isolated arca-test-perf
}

test_perf_test_against_the_running_server_never_recreates_it() {
    script_setup
    bin/perf-test > run.log 2>&1 || { cat run.log; return 1; }
    assert_contains "$(compose_calls | grep ' run ')" "run --rm --no-deps perf-test" &&
    assert_eq "developer config" "$(cat config/.generated.toml)"
}

# --- bin/test integration and the modes that share it --------------------------

test_integration_targets_the_running_server_unchanged() {
    script_setup
    run_mode integration integration/ || return 1
    local run
    run="$(compose_calls | grep ' run ')"
    assert_not_contains "$(compose_calls)" " -p " &&
    assert_contains "$run" "run --rm --no-deps" &&
    assert_not_contains "$(compose_calls)" " up " &&
    assert_not_contains "$(compose_calls)" " down" &&
    assert_eq "developer config" "$(cat config/.generated.toml)" &&
    assert_eq "$(cat .arca-env.orig)" "$(cat .arca-env)"
}

test_integration_enables_the_suites_of_the_recorded_features() {
    script_setup
    run_mode integration integration/ || return 1
    local run
    run="$(compose_calls | grep ' run ')"
    assert_contains "$run" "-e ARCA_ENCRYPTION_ENABLED=1" &&
    assert_not_contains "$run" "ARCA_PER_BUCKET_ENCRYPTION" &&
    assert_not_contains "$run" "ARCA_KMS_ENABLED" &&
    assert_contains "$run" " test integration/"
}

test_presigned_and_ssec_run_against_the_running_server() {
    # They start nothing of their own: without --no-deps, `compose run` would
    # recreate the developer's server from the base compose file.
    script_setup
    local mode run
    for mode in presigned ssec; do
        : > "$DOCKER_LOG"
        run_mode "$mode" || return 1
        run="$(compose_calls | grep ' run ')"
        assert_contains "$run" "run --rm --no-deps" || { echo "mode: $mode"; return 1; }
        assert_contains "$run" "integration/test_$mode.py" || return 1
    done
}

# --- cleanup when a test step fails ----------------------------------------------

# `set -e` ends the script at the failing step and the EXIT trap runs after the
# mode function returned: the cleanup must not depend on the function's locals.
failing_mode_test() {   # failing_mode_test <project suffix> <bin/test args...>
    local project="arca-test-$1" rc=0; shift
    script_setup
    export MOCK_FAIL_RUN=1
    bin/test "$@" > run.log 2>&1 || rc=$?
    assert_eq "1" "$rc" "a failing step must make bin/test fail" || return 1
    assert_not_contains "$(cat run.log)" "unbound variable" &&
    assert_contains "$(compose_calls | grep ' down ')" "-p $project " &&
    assert_contains "$(compose_calls | grep ' down ')" "-v" &&
    assert_eq "" "$(compose_calls | grep ' down ' | grep -v "^compose -p $project " || true)" \
        "a cleanup addressed another project" &&
    assert_eq "developer config" "$(cat config/.generated.toml)"
}

test_tls_mode_cleans_up_when_a_step_fails()         { failing_mode_test tls tls; }
test_encryption_mode_cleans_up_when_a_step_fails()  { failing_mode_test encryption encryption; }
test_kms_mode_cleans_up_when_a_step_fails()         { failing_mode_test kms kms; }
test_postgres_mode_cleans_up_when_a_step_fails()    { failing_mode_test postgres postgres; }
test_notifications_mode_cleans_up_when_a_step_fails() { failing_mode_test notifications notifications; }
test_migrate_topology_mode_cleans_up_when_a_step_fails() { failing_mode_test migrate-topology migrate-topology; }

test_a_passing_mode_cleans_up_exactly_once() {
    local mode
    for mode in tls encryption; do
        script_setup
        run_mode "$mode" || return 1
        assert_eq "1" "$(compose_calls | grep -c ' down ')" "mode: $mode" || return 1
    done
}

# -----------------------------------------------------------------------------

echo "==> Test isolation (bin/test, bin/perf-test, bin/lib/compose.sh)"
for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
    run_test "$t"
done

echo ""
echo "$passed passed, $failed failed"
(( failed == 0 ))
