#!/usr/bin/env bash
# Unit tests for the TLS certificate selection in bin/lib/compose.sh: which
# certificates `bin/arca start --tls` serves, and how the local ones are kept
# valid. Run them with `bin/test scripts`.
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

# A scratch repository with the library sourced from it.
setup() {
    cd "$(mktemp -d)" || exit 1
    git init -q
    mkdir -p docker
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

touch_cert() { mkdir -p "$(dirname "$1")"; echo "pem" > "$1"; }

# --- has_user_certs ----------------------------------------------------------

test_no_certs_directory_means_no_user_certs() {
    setup
    ! has_user_certs
}

test_empty_certs_directory_means_no_user_certs() {
    setup
    mkdir -p certs
    ! has_user_certs
}

test_generated_subdirectories_are_not_user_certs() {
    setup
    touch_cert certs/local/arca-server.crt
    touch_cert certs/local/arca-server.key
    touch_cert certs/local-ca/arca-ca.key
    ! has_user_certs
}

test_pem_files_are_user_certs() {
    setup
    touch_cert certs/fullchain.pem
    touch_cert certs/privkey.pem
    has_user_certs
}

test_each_certificate_extension_counts() {
    setup
    local ext
    for ext in pem crt cert key; do
        rm -rf certs
        touch_cert "certs/server.$ext"
        has_user_certs || { echo "a .$ext file was not recognized"; return 1; }
    done
}

test_symlinked_certificates_count() {
    # certbot users often link to /etc/letsencrypt/live/...
    setup
    touch_cert elsewhere/fullchain.pem
    mkdir -p certs
    ln -s "$PWD/elsewhere/fullchain.pem" certs/fullchain.pem
    has_user_certs
}

test_unrelated_files_are_not_user_certs() {
    setup
    mkdir -p certs
    echo "notes" > certs/README.txt
    ! has_user_certs
}

# --- enable_tls_auto ---------------------------------------------------------

test_user_certs_are_served_with_auto_detect() {
    setup
    touch_cert certs/fullchain.pem
    touch_cert certs/privkey.pem
    enable_tls_auto
    assert_eq "tls" "${_FEATURES[*]}" &&
    assert_eq "../certs" "$ARCA_CERTS_DIR"
}

test_without_user_certs_the_local_ones_are_served() {
    setup
    enable_tls_auto
    # Known file names: the local directory also holds the CA certificate.
    assert_eq "tls-explicit" "${_FEATURES[*]}" &&
    assert_eq "../certs/local" "$ARCA_CERTS_DIR"
}

test_user_certs_win_over_existing_local_ones() {
    setup
    touch_cert certs/local/arca-server.crt
    touch_cert certs/local/arca-server.key
    touch_cert certs/fullchain.pem
    enable_tls_auto
    assert_eq "../certs" "$ARCA_CERTS_DIR"
}

test_both_modes_use_the_tls_compose_overlay() {
    setup
    enable_tls_auto
    assert_contains "$(compose_cmd)" "docker-compose.tls.yml"
}

# --- prepare_local_certs -----------------------------------------------------

test_prepare_does_nothing_for_user_certs() {
    setup
    mock_docker
    touch_cert certs/fullchain.pem
    enable_tls_auto
    prepare_local_certs
    assert_eq "" "$(cat "$DOCKER_LOG")"
}

test_prepare_runs_ensure_as_the_host_user_with_the_service_group() {
    setup
    mock_docker
    enable_tls_auto
    prepare_local_certs
    local log
    log="$(cat "$DOCKER_LOG")"
    assert_contains "$log" $'run\n--rm\n--user\n'"$(id -u):65532" &&
    # Mounted at the same relative path it has in the repository, so the paths
    # `arca tls ensure` prints are the ones the user sees on the host.
    assert_contains "$log" $'-v\n'"$PWD/certs:/repo/certs"$'\n-w\n/repo' &&
    assert_contains "$log" $'ghcr.io/dxc-technology/arca:production\ntls\nensure\n--output-dir\ncerts/local\n--ca-dir\ncerts/local-ca' &&
    assert_contains "$log" $'--sans\nlocalhost,127.0.0.1,::1,arca\n--'
}

test_prepare_uses_the_image_of_the_build_target() {
    setup
    mock_docker
    export BUILD_TARGET=development
    enable_tls_auto
    prepare_local_certs
    assert_contains "$(cat "$DOCKER_LOG")" "ghcr.io/dxc-technology/arca:development"
}

test_prepare_builds_the_image_when_missing() {
    setup
    mock_docker
    docker_reply() { [[ "$1 $2" != "image inspect" ]]; }
    enable_tls_auto
    prepare_local_certs
    assert_contains "$(cat "$DOCKER_LOG")" $'build\narca\n--'
}

test_prepare_adds_extra_sans_from_docker_env_file() {
    setup
    mock_docker
    echo 'ARCA_TLS_SANS="s3.home.lan, 192.168.1.10"' > docker/.env
    enable_tls_auto
    prepare_local_certs
    assert_contains "$(cat "$DOCKER_LOG")" $'--sans\nlocalhost,127.0.0.1,::1,arca,s3.home.lan, 192.168.1.10\n--'
}

test_prepare_prefers_extra_sans_from_the_environment() {
    setup
    mock_docker
    echo 'ARCA_TLS_SANS=from-file' > docker/.env
    export ARCA_TLS_SANS=from-env
    enable_tls_auto
    prepare_local_certs
    assert_contains "$(cat "$DOCKER_LOG")" $'--sans\nlocalhost,127.0.0.1,::1,arca,from-env\n--'
}

test_prepare_fails_when_ensure_fails() {
    setup
    mock_docker
    docker_reply() { [[ "$1" != "run" ]]; }
    enable_tls_auto
    ! prepare_local_certs
}

# --- make_test_certs / remove_test_certs -----------------------------------

test_make_test_certs_generates_into_a_fresh_named_volume() {
    setup
    mock_docker
    make_test_certs "arca,localhost"
    local log
    log="$(cat "$DOCKER_LOG")"
    assert_contains "$log" $'volume\nrm\n-f\narca-tls-test-certs\n--\nvolume\ncreate\narca-tls-test-certs\n--' &&
    # A fresh volume is root-owned; `arca tls generate` runs as 65532.
    assert_contains "$log" $'-v\narca-tls-test-certs:/certs\nalpine\nchown\n65532:65532\n/certs' &&
    assert_contains "$log" $'-v\narca-tls-test-certs:/certs\nghcr.io/dxc-technology/arca:production\ntls\ngenerate\n--output-dir\n/certs\n--sans\narca,localhost\n--' &&
    assert_eq "arca-tls-test-certs" "$ARCA_CERTS_DIR"
}

test_make_test_certs_never_touches_the_certs_directory() {
    setup
    mock_docker
    make_test_certs "arca"
    if grep -q "$PWD/certs" "$DOCKER_LOG" || [[ -e certs ]]; then
        echo "the self-contained suites must not use ./certs"
        return 1
    fi
}

test_make_test_certs_fails_when_generation_fails() {
    setup
    mock_docker
    docker_reply() { [[ "$*" != *"tls generate"* ]]; }
    ! make_test_certs "arca"
}

test_remove_test_certs_removes_the_volume() {
    setup
    mock_docker
    remove_test_certs
    assert_contains "$(cat "$DOCKER_LOG")" $'volume\nrm\n-f\narca-tls-test-certs\n--'
}

# --- pick_tls_hostname -------------------------------------------------------

test_pick_prefers_the_first_plain_dns_name() {
    setup
    assert_eq "s3.example.org" "$(printf '*.example.org\ns3.example.org\nexample.org\n' | pick_tls_hostname)"
}

test_pick_turns_a_wildcard_into_a_concrete_name() {
    # *.example.org covers any single label, so arca.example.org is valid.
    setup
    assert_eq "arca.example.org" "$(printf '*.example.org\n' | pick_tls_hostname)"
}

test_pick_fails_without_dns_names() {
    setup
    ! printf '' | pick_tls_hostname
}

test_pick_honours_the_override_from_docker_env_file() {
    setup
    echo 'ARCA_TLS_HOSTNAME=s3.home.lan' > docker/.env
    assert_eq "s3.home.lan" "$(printf 'other.example.org\n' | pick_tls_hostname)"
}

# --- integration_endpoint_args ----------------------------------------------

test_plain_server_keeps_the_default_endpoint() {
    setup
    assert_eq "" "$(integration_endpoint_args "")"
}

test_local_certificates_are_verified_against_the_local_ca() {
    setup
    enable_tls_auto
    assert_eq "-e
ARCA_ENDPOINT=https://arca:9000
-e
AWS_CA_BUNDLE=/etc/arca/certs/arca-ca.crt" "$(integration_endpoint_args "")"
}

test_user_certificates_use_the_certificate_hostname() {
    setup
    touch_cert certs/fullchain.pem
    enable_tls_auto
    # System CAs verify a publicly issued certificate: no bundle.
    assert_eq "-e
ARCA_ENDPOINT=https://s3.example.org:9000" "$(integration_endpoint_args "s3.example.org")"
}

# --- extra_hosts_override ----------------------------------------------------

test_extra_hosts_override_maps_a_name_for_one_service() {
    setup
    local file content
    file="$(extra_hosts_override test s3.example.org 172.18.0.5)" || return 1
    content="$(cat "$file")"
    rm -f "$file"
    if [[ "$file" == "$PWD"/* ]]; then
        echo "the override must not land in the repository"
        return 1
    fi
    assert_eq "services:
  test:
    extra_hosts:
      - \"s3.example.org:172.18.0.5\"" "$content"
}

# --- state persistence -------------------------------------------------------

test_the_certificate_choice_survives_save_and_load() {
    setup
    enable_tls_auto
    save_env
    unset ARCA_CERTS_DIR
    _FEATURES=()
    _HAS_TLS=false
    load_env
    assert_eq "../certs/local" "$ARCA_CERTS_DIR" &&
    assert_eq "tls-explicit" "${_FEATURES[*]}"
}

test_an_old_state_file_falls_back_to_the_certs_directory() {
    # .arca-env written before ARCA_CERTS_DIR existed.
    setup
    printf 'FEATURES="tls"\nBUILD_TARGET="production"\n' > .arca-env
    load_env
    assert_eq "../certs" "${ARCA_CERTS_DIR:-../certs}"
}

# -----------------------------------------------------------------------------

echo "==> bin/lib/compose.sh (TLS certificates)"
for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
    run_test "$t"
done

echo ""
echo "$passed passed, $failed failed"
(( failed == 0 ))
