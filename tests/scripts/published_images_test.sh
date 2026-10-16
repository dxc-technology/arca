#!/usr/bin/env bash
# Unit tests for running the published images instead of building them
# (`--release` of bin/arca, bin/console and bin/cluster): the state kept in
# bin/lib/compose.sh. Run them with `bin/test scripts`.
#
# Every test runs in its own subshell, inside a scratch git repository (the
# library resolves REPO_ROOT from git), with `docker` replaced by a recorder.

set -uo pipefail

LIB="$(cd "$(dirname "$0")/../.." && pwd)/bin/lib/compose.sh"

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

assert_fails() {
    if "$@" >/dev/null 2>&1; then
        echo "expected failure: $*"
        return 1
    fi
}

setup() {
    cd "$(mktemp -d)" || exit 1
    git init -q
    mkdir -p docker
    unset ARCA_RELEASE BUILD_TARGET ARCA_CERTS_DIR
    # shellcheck source=/dev/null
    source "$LIB"
}

mock_docker() {
    DOCKER_LOG="$(mktemp)"
    docker() {
        printf '%s\n' "$@" -- >> "$DOCKER_LOG"
        docker_reply "$@"
    }
    docker_reply() { :; }
}

# --- use_release -------------------------------------------------------------

test_use_release_exports_the_tag() {
    setup
    use_release 0.31.2 || return 1
    assert_eq "0.31.2" "$ARCA_RELEASE"
}

test_use_release_accepts_latest_and_published_versions() {
    setup
    local tag
    for tag in latest 1 0.31 0.31.2; do
        use_release "$tag" || { echo "'$tag' rejected"; return 1; }
    done
}

test_use_release_rejects_tags_that_are_not_published() {
    setup
    local tag
    for tag in "" production development v0.31.2 "a b" 0; do
        if use_release "$tag" 2>/dev/null; then echo "'$tag' accepted"; return 1; fi
    done
}

test_use_release_refuses_the_development_image() {
    setup
    export BUILD_TARGET=development
    local out
    out="$(use_release latest 2>&1)" && { echo "expected failure"; return 1; }
    assert_contains "$out" "--dev"
}

# --- the tree's own image -----------------------------------------------------

test_build_flag_if_missing_asks_for_a_build_when_the_image_is_absent() {
    setup
    mock_docker
    docker_reply() { return 1; }        # `docker image inspect` fails
    assert_eq "--build" "$(build_flag_if_missing)"
}

test_build_flag_if_missing_is_empty_when_the_image_exists() {
    setup
    mock_docker
    assert_eq "" "$(build_flag_if_missing)"
}

test_build_flag_if_missing_looks_for_the_local_image_not_a_release() {
    setup
    mock_docker
    ARCA_RELEASE=
    build_flag_if_missing >/dev/null
    assert_contains "$(cat "$DOCKER_LOG")" "ghcr.io/dxc-technology/arca:production"
}

test_build_flag_if_missing_follows_build_target_from_docker_env() {
    setup
    mock_docker
    echo 'BUILD_TARGET=development' > docker/.env
    build_flag_if_missing >/dev/null
    assert_contains "$(cat "$DOCKER_LOG")" "ghcr.io/dxc-technology/arca:development"
}

# --- state file --------------------------------------------------------------

test_the_release_survives_save_and_load() {
    setup
    use_release 0.31 || return 1
    save_env
    unset ARCA_RELEASE
    load_env
    assert_eq "0.31" "$ARCA_RELEASE"
}

test_a_local_build_clears_the_saved_release() {
    setup
    use_release 0.31 || return 1
    save_env
    unset ARCA_RELEASE
    save_env                    # the next start builds locally
    load_env
    assert_eq "" "${ARCA_RELEASE:-}"
}

test_an_old_state_file_has_no_release() {
    setup
    printf 'FEATURES="tls"\nBUILD_TARGET="production"\n' > .arca-env
    load_env
    assert_eq "" "${ARCA_RELEASE:-}"
}

# --- certificates ------------------------------------------------------------

test_prepare_uses_the_release_image() {
    setup
    mock_docker
    use_release 0.31.2 || return 1
    enable_tls_auto
    prepare_local_certs
    assert_contains "$(cat "$DOCKER_LOG")" $'ghcr.io/dxc-technology/arca:0.31.2\ntls\nensure'
}

test_prepare_pulls_a_missing_release_image_instead_of_building() {
    setup
    mock_docker
    docker_reply() { [[ "$1 $2" != "image inspect" ]]; }
    use_release 0.31.2 || return 1
    enable_tls_auto
    prepare_local_certs
    local log
    log="$(cat "$DOCKER_LOG")"
    assert_contains "$log" $'pull\narca\n--' &&
    if [[ "$log" == *$'build\narca'* ]]; then echo "a release must not be built"; return 1; fi
}

# -----------------------------------------------------------------------------

echo "==> bin/lib/compose.sh (published images)"
for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
    run_test "$t"
done

echo ""
echo "$passed passed, $failed failed"
(( failed == 0 ))
