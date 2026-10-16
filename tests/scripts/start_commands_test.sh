#!/usr/bin/env bash
# Tests for how bin/arca start, bin/console start and bin/cluster up get their
# images: the one in the working tree, the published one (--release) or, when
# nothing was built, the published latest. Run them with `bin/test scripts`.
#
# Every test runs a copy of bin/ in a scratch git repository with `docker`
# replaced on PATH by a recorder: nothing here talks to a Docker daemon or a
# registry. IMAGES_PRESENT=1 makes `docker image inspect` succeed.

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

# A scratch repository with a copy of bin/ and the config fragments, and a
# recording `docker` first on PATH. Sets WORK and DOCKER_LOG.
setup() {
    WORK="$(mktemp -d)"
    cd "$WORK" || exit 1
    git init -q
    mkdir -p docker config stub
    cp -R "$REPO/bin" .
    cp -R "$REPO/config/fragments" config/
    DOCKER_LOG="$WORK/docker.log"
    : > "$DOCKER_LOG"
    : > "$DOCKER_LOG.env"
    cat > stub/docker <<'STUB'
#!/bin/sh
echo "$*" >> "$DOCKER_LOG"
echo "$1 [${ARCA_RELEASE-UNSET}]" >> "$DOCKER_LOG.env"
case "$*" in
    "image inspect"*) [ -n "${IMAGES_PRESENT:-}" ] ;;
esac
STUB
    chmod +x stub/docker
    export PATH="$WORK/stub:$PATH" DOCKER_LOG
    unset IMAGES_PRESENT ARCA_RELEASE BUILD_TARGET
}

log() { cat "$DOCKER_LOG"; }

# The distinct ARCA_RELEASE values the compose calls ran with, as
# "[value]" ("[UNSET]" when the variable was not exported at all).
compose_releases() { awk '$1 == "compose" {print $2}' "$DOCKER_LOG.env" | sort -u | tr '\n' ' '; }

compose_calls() { grep -E '^compose ' "$DOCKER_LOG" || true; }

# --- bin/arca start ------------------------------------------------------------

test_start_runs_the_existing_image_without_building_or_pulling() {
    setup
    IMAGES_PRESENT=1 bin/arca start -d >/dev/null 2>&1 || return 1
    local calls
    calls="$(compose_calls)"
    assert_contains "$calls" " up --no-build -d arca" &&
    assert_not_contains "$calls" " pull" &&
    assert_not_contains "$calls" " build" &&
    assert_contains "$(cat .arca-env)" 'ARCA_RELEASE=""'
}

test_start_pulls_the_published_latest_when_no_image_was_built() {
    setup
    bin/arca start -d >/dev/null 2>&1 || return 1
    local calls
    calls="$(compose_calls)"
    assert_contains "$calls" " pull arca" &&
    assert_contains "$calls" " up --no-build -d arca" &&
    assert_not_contains "$calls" " build" &&
    assert_contains "$(cat .arca-env)" 'ARCA_RELEASE="latest"'
}

test_start_builds_the_development_image_when_missing() {
    setup
    bin/arca start -d --dev >/dev/null 2>&1 || return 1
    local calls
    calls="$(compose_calls)"
    assert_contains "$calls" " build --build-arg" &&
    assert_not_contains "$calls" " pull" &&
    assert_contains "$(cat .arca-env)" 'ARCA_RELEASE=""'
}

test_start_with_a_release_pulls_that_tag_and_remembers_it() {
    setup
    IMAGES_PRESENT=1 bin/arca start -d --release 0.31 >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " pull arca" &&
    assert_contains "$(cat .arca-env)" 'ARCA_RELEASE="0.31"'
}

test_start_with_build_builds_and_forgets_a_previous_release() {
    setup
    echo 'ARCA_RELEASE="0.31"' > .arca-env
    bin/arca start -d --build >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " build --build-arg" &&
    assert_not_contains "$(compose_calls)" " pull" &&
    assert_contains "$(cat .arca-env)" 'ARCA_RELEASE=""'
}

test_start_with_a_release_applies_it_to_every_compose_call() {
    setup
    IMAGES_PRESENT=1 bin/arca start -d --release 0.31 >/dev/null 2>&1 || return 1
    assert_eq "[0.31] " "$(compose_releases)"
}

test_start_ignores_a_release_left_in_docker_env() {
    setup
    echo 'ARCA_RELEASE=9.9' > docker/.env
    IMAGES_PRESENT=1 bin/arca start -d >/dev/null 2>&1 || return 1
    # Empty, not unset: compose would otherwise read 9.9 from docker/.env.
    assert_eq "[] " "$(compose_releases)"
}

test_start_honours_build_target_from_docker_env() {
    setup
    echo 'BUILD_TARGET=development' > docker/.env
    bin/arca start -d >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " build --build-arg" &&
    assert_not_contains "$(compose_calls)" " pull"
}

test_start_refuses_build_with_release() {
    setup
    if bin/arca start -d --build --release latest >/dev/null 2>&1; then
        echo "expected failure"; return 1
    fi
    assert_eq "" "$(compose_calls)" "nothing may run"
}

test_start_refuses_dev_with_release() {
    setup
    if bin/arca start -d --release latest --dev >/dev/null 2>&1; then
        echo "expected failure"; return 1
    fi
    assert_eq "" "$(compose_calls)" "nothing may run"
}

test_start_refuses_a_release_without_a_valid_tag() {
    setup
    local args
    for args in "--release" "--release production" "--release v0.31.2" "--release 0"; do
        # shellcheck disable=SC2086
        if bin/arca start -d $args >/dev/null 2>&1; then
            echo "'$args' accepted"; return 1
        fi
    done
    assert_eq "" "$(compose_calls)" "nothing may run"
}

# --- bin/console start ---------------------------------------------------------

test_console_follows_the_release_of_the_server() {
    setup
    echo 'ARCA_RELEASE="0.31"' > .arca-env
    IMAGES_PRESENT=1 bin/console start -d >/dev/null 2>&1 || return 1
    local calls
    calls="$(compose_calls)"
    assert_contains "$calls" " pull console" &&
    assert_contains "$calls" " up --no-build -d console" &&
    assert_not_contains "$calls" "--no-deps"
}

test_console_runs_the_existing_image_without_pulling() {
    setup
    IMAGES_PRESENT=1 bin/console start -d >/dev/null 2>&1 || return 1
    assert_not_contains "$(compose_calls)" " pull" &&
    assert_contains "$(compose_calls)" " up --no-build -d console"
}

test_console_pulls_the_published_latest_when_none_was_built() {
    setup
    bin/console start -d >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " pull console" &&
    assert_contains "$(compose_calls)" "--no-deps"
}

test_console_release_leaves_a_local_server_alone_and_warns() {
    setup
    local err
    err="$(IMAGES_PRESENT=1 bin/console start -d --release 0.31 2>&1 >/dev/null)" || return 1
    assert_contains "$(compose_calls)" " pull console" &&
    assert_contains "$(compose_calls)" " up --no-build --no-deps -d console" &&
    assert_contains "$err" "keep them at the same version"
}

test_console_build_leaves_a_server_running_a_release_alone() {
    setup
    echo 'ARCA_RELEASE="0.31"' > .arca-env
    bin/console start -d --build >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " build console" &&
    assert_contains "$(compose_calls)" " up --no-build --no-deps -d console"
}

test_console_build_ignores_a_release_left_in_docker_env() {
    setup
    echo 'ARCA_RELEASE=9.9' > docker/.env
    bin/console start -d --build >/dev/null 2>&1 || return 1
    assert_eq "[] " "$(compose_releases)"
}

test_console_build_warns_when_the_server_runs_a_release() {
    setup
    echo 'ARCA_RELEASE="0.31"' > .arca-env
    local err
    err="$(bin/console start -d --build 2>&1 >/dev/null)" || return 1
    assert_contains "$err" "keep them at the same version"
}

test_console_refuses_build_with_release() {
    setup
    if bin/console start -d --build --release latest >/dev/null 2>&1; then
        echo "expected failure"; return 1
    fi
    assert_eq "" "$(compose_calls)" "nothing may run"
}

# --- bin/cluster up ------------------------------------------------------------

test_cluster_runs_the_existing_images_without_building_or_pulling() {
    setup
    IMAGES_PRESENT=1 bin/cluster up -d >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " up --no-build -d" &&
    assert_not_contains "$(compose_calls)" " pull" &&
    assert_not_contains "$(compose_calls)" " build"
}

test_cluster_pulls_the_published_latest_when_none_was_built() {
    setup
    bin/cluster up -d >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " pull arca-1 arca-2 arca-3 console" &&
    assert_contains "$(compose_calls)" " up --no-build -d" &&
    assert_not_contains "$(compose_calls)" " build"
}

test_cluster_builds_the_development_images_when_missing() {
    setup
    bin/cluster up -d --dev >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " build" &&
    assert_not_contains "$(compose_calls)" " pull"
}

test_cluster_release_pulls_that_tag() {
    setup
    IMAGES_PRESENT=1 bin/cluster up -d --release 0.31.2 >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " pull arca-1 arca-2 arca-3 console"
}

test_cluster_release_applies_it_to_every_compose_call() {
    setup
    IMAGES_PRESENT=1 bin/cluster up -d --release 0.31.2 >/dev/null 2>&1 || return 1
    assert_eq "[0.31.2] " "$(compose_releases)"
}

test_cluster_honours_build_target_from_docker_env() {
    setup
    echo 'BUILD_TARGET=development' > docker/.env
    bin/cluster up -d >/dev/null 2>&1 || return 1
    assert_contains "$(compose_calls)" " build" &&
    assert_not_contains "$(compose_calls)" " pull"
}

test_cluster_refuses_build_with_release() {
    setup
    if bin/cluster up -d --build --release latest >/dev/null 2>&1; then
        echo "expected failure"; return 1
    fi
    assert_eq "" "$(compose_calls)" "nothing may run"
}

# -----------------------------------------------------------------------------

echo "==> bin/arca, bin/console, bin/cluster (image selection)"
for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
    run_test "$t"
done

echo ""
echo "$passed passed, $failed failed"
(( failed == 0 ))
