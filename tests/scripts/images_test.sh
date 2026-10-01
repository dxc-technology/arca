#!/usr/bin/env bash
# Unit tests for bin/lib/images.sh (published image names, release versioning
# and multi-arch publishing). Run them with `bin/test scripts`, which executes
# this file in a throwaway container.
#
# Every test runs in its own subshell, inside a scratch git repository, with
# `docker` replaced by a shell function that records its arguments: nothing
# here talks to a Docker daemon or a registry.

set -uo pipefail

LIB="$(cd "$(dirname "$0")/../.." && pwd)/bin/lib/images.sh"

passed=0
failed=0

run_test() {    # run_test <function>
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

assert_eq() {   # assert_eq <expected> <actual> [message]
    if [[ "$1" != "$2" ]]; then
        echo "${3:-assertion failed}"
        echo "  expected: $(printf '%q' "$1")"
        echo "  actual:   $(printf '%q' "$2")"
        return 1
    fi
}

assert_contains() {     # assert_contains <haystack> <needle>
    if [[ "$1" != *"$2"* ]]; then
        echo "expected to find: $2"
        echo "in: $1"
        return 1
    fi
}

assert_fails() {    # assert_fails <command...>
    if "$@" >/dev/null 2>&1; then
        echo "expected failure: $*"
        return 1
    fi
}

# A scratch repository shaped like Arca: the workspace version in Cargo.toml
# (plus a dependency pin, which must not be mistaken for it) and the console
# version in console/index.html.
new_repo() {    # new_repo <cargo version> <console version>
    cd "$(mktemp -d)" || exit 1
    git init -q
    git config user.email test@example.com
    git config user.name test
    cat > Cargo.toml <<EOF
[workspace]
members = []

[workspace.package]
version = "$1"
license = "AGPL-3.0-or-later"

[workspace.dependencies]
time = { version = "=0.3.47" }
EOF
    mkdir -p console docker
    cat > console/index.html <<EOF
<script>
    window.ARCA_CONSOLE_VERSION = "$2";
</script>
EOF
    git add -A
    git commit -qm init
}

# Replaces `docker` with a recorder: each call appends its arguments, one per
# line, followed by a `--` separator, to $DOCKER_LOG.
mock_docker() {
    DOCKER_LOG="$(mktemp)"
    docker() {
        printf '%s\n' "$@" -- >> "$DOCKER_LOG"
        docker_reply "$@"
    }
    docker_reply() { :; }
}

source "$LIB"

# --- image_repo --------------------------------------------------------------

test_image_repo_names_both_published_images() {
    assert_eq "ghcr.io/dxc-technology/arca" "$(image_repo arca)" &&
    assert_eq "ghcr.io/dxc-technology/arca-console" "$(image_repo arca-console)"
}

test_image_repo_rejects_unpublished_images() {
    assert_fails image_repo arca-test
}

# --- release_version ---------------------------------------------------------

test_release_version_accepts_matching_tag() {
    new_repo 1.2.3 1.2.3
    git tag v1.2.3
    assert_eq "1.2.3" "$(release_version)"
}

test_release_version_accepts_prerelease() {
    new_repo 1.3.0-rc.1 1.3.0-rc.1
    git tag v1.3.0-rc.1
    assert_eq "1.3.0-rc.1" "$(release_version)"
}

test_release_version_rejects_untagged_head() {
    new_repo 1.2.3 1.2.3
    git tag v1.2.3
    git commit -q --allow-empty -m next
    assert_fails release_version
}

test_release_version_rejects_two_release_tags_on_head() {
    new_repo 1.2.3 1.2.3
    git tag v1.2.3
    git tag v1.2.4
    assert_fails release_version
}

test_release_version_ignores_non_release_tags_on_head() {
    new_repo 1.2.3 1.2.3
    git tag v1.2.3
    git tag some-marker
    assert_eq "1.2.3" "$(release_version)"
}

test_release_version_rejects_non_semver_tag() {
    new_repo 1.2 1.2
    git tag v1.2
    assert_fails release_version
}

test_release_version_rejects_cargo_mismatch() {
    new_repo 1.2.2 1.2.3
    git tag v1.2.3
    assert_fails release_version
}

test_release_version_rejects_console_mismatch() {
    new_repo 1.2.3 1.2.2
    git tag v1.2.3
    assert_fails release_version
}

test_release_version_rejects_modified_tree() {
    new_repo 1.2.3 1.2.3
    git tag v1.2.3
    echo "# edit" >> Cargo.toml
    assert_fails release_version
}

test_release_version_rejects_untracked_files() {
    new_repo 1.2.3 1.2.3
    git tag v1.2.3
    touch stray-file
    assert_fails release_version
}

test_release_version_reports_the_reason() {
    new_repo 1.2.2 1.2.3
    git tag v1.2.3
    assert_contains "$(release_version 2>&1)" "Cargo.toml"
}

# --- release_tags ------------------------------------------------------------

tags_of() { release_tags "$1" | tr '\n' ' ' | sed 's/ $//'; }

test_release_tags_first_release_gets_all_tags() {
    new_repo 1.2.3 1.2.3
    git tag v1.2.3
    assert_eq "1.2.3 1.2 1 latest" "$(tags_of 1.2.3)"
}

test_release_tags_fix_on_older_minor_keeps_latest() {
    new_repo 1.3.0 1.3.0
    git tag v1.2.3
    git tag v1.2.4
    git tag v1.3.0
    assert_eq "1.2.4 1.2" "$(tags_of 1.2.4)"
}

test_release_tags_republishing_older_patch_moves_nothing() {
    new_repo 1.3.1 1.3.1
    git tag v1.3.0
    git tag v1.3.1
    assert_eq "1.3.0" "$(tags_of 1.3.0)"
}

test_release_tags_sort_numerically_not_lexically() {
    new_repo 1.10.0 1.10.0
    git tag v1.9.0
    git tag v1.10.0
    assert_eq "1.10.0 1.10 1 latest" "$(tags_of 1.10.0)" &&
    assert_eq "1.9.0 1.9" "$(tags_of 1.9.0)"
}

test_release_tags_minor_glob_does_not_leak_into_other_minors() {
    # v0.3.* must not match v0.30.x
    new_repo 0.30.0 0.30.0
    git tag v0.3.9
    git tag v0.30.0
    assert_eq "0.3.9 0.3" "$(tags_of 0.3.9)"
}

test_release_tags_major_tag_skipped_in_0x() {
    # in 0.x a minor bump may break compatibility, so `:0` would promise too much
    new_repo 0.30.2 0.30.2
    git tag v0.30.2
    assert_eq "0.30.2 0.30 latest" "$(tags_of 0.30.2)"
}

test_release_tags_major_tag_starts_at_1_0_0() {
    new_repo 1.0.0 1.0.0
    git tag v0.30.2
    git tag v1.0.0
    assert_eq "1.0.0 1.0 1 latest" "$(tags_of 1.0.0)"
}

test_release_tags_fix_on_older_major_keeps_its_major_tag() {
    new_repo 2.0.0 2.0.0
    git tag v1.4.0
    git tag v1.4.1
    git tag v2.0.0
    assert_eq "1.4.1 1.4 1" "$(tags_of 1.4.1)"
}

test_release_tags_older_minor_does_not_move_major_tag() {
    new_repo 1.3.0 1.3.0
    git tag v1.2.4
    git tag v1.3.0
    assert_eq "1.2.4 1.2" "$(tags_of 1.2.4)"
}

test_release_tags_major_glob_does_not_leak_into_other_majors() {
    # v1.* must not match v10.x
    new_repo 10.0.0 10.0.0
    git tag v1.9.0
    git tag v10.0.0
    assert_eq "1.9.0 1.9 1" "$(tags_of 1.9.0)"
}

test_release_tags_prerelease_gets_exact_tag_only() {
    new_repo 1.4.0-rc.1 1.4.0-rc.1
    git tag v1.3.0
    git tag v1.4.0-rc.1
    assert_eq "1.4.0-rc.1" "$(tags_of 1.4.0-rc.1)"
}

test_release_tags_prerelease_does_not_steal_latest() {
    new_repo 1.3.0 1.3.0
    git tag v1.3.0
    git tag v1.4.0-rc.1
    assert_eq "1.3.0 1.3 1 latest" "$(tags_of 1.3.0)"
}

# --- annotation_args ---------------------------------------------------------

test_annotation_args_promotes_only_oci_labels() {
    local out
    out="$(echo '{
        "org.opencontainers.image.source": "https://github.com/dxc-technology/arca",
        "org.opencontainers.image.description": "Object storage, S3 compatible",
        "com.example.other": "ignored"
    }' | annotation_args)"
    assert_eq "index:org.opencontainers.image.source=https://github.com/dxc-technology/arca
index:org.opencontainers.image.description=Object storage, S3 compatible" "$out"
}

test_annotation_args_handles_no_labels() {
    assert_eq "" "$(echo 'null' | annotation_args)"
}

# --- image_labels ------------------------------------------------------------

test_image_labels_reads_first_real_platform_of_an_index() {
    new_repo 1.2.3 1.2.3
    mock_docker
    docker_reply() {
        if [[ "$*" == *"--raw"* ]]; then
            echo '{"manifests": [
                {"digest": "sha256:attest", "platform": {"os": "unknown", "architecture": "unknown"}},
                {"digest": "sha256:amd64",  "platform": {"os": "linux", "architecture": "amd64"}}
            ]}'
        else
            echo '{"org.opencontainers.image.title": "Arca"}'
        fi
    }
    local out
    out="$(image_labels ghcr.io/dxc-technology/arca@sha256:index)"
    assert_eq '{"org.opencontainers.image.title": "Arca"}' "$out" &&
    assert_contains "$(cat "$DOCKER_LOG")" $'inspect\nghcr.io/dxc-technology/arca@sha256:amd64\n--format'
}

test_image_labels_reads_a_plain_manifest_directly() {
    new_repo 1.2.3 1.2.3
    mock_docker
    docker_reply() {
        if [[ "$*" == *"--raw"* ]]; then
            echo '{"config": {"digest": "sha256:cfg"}, "layers": []}'
        else
            echo '{}'
        fi
    }
    image_labels ghcr.io/dxc-technology/arca@sha256:plain >/dev/null
    assert_contains "$(cat "$DOCKER_LOG")" $'inspect\nghcr.io/dxc-technology/arca@sha256:plain\n--format'
}

# --- publish_index -----------------------------------------------------------

test_publish_index_tags_annotates_and_merges_sources() {
    new_repo 1.2.3 1.2.3
    git tag v1.2.3
    mock_docker
    docker_reply() {
        if [[ "$*" == *"--raw"* ]]; then
            echo '{"config": {}}'
        elif [[ "$*" == *"--format"* ]]; then
            echo '{"org.opencontainers.image.description": "Object storage, S3 compatible"}'
        fi
    }
    publish_index arca 1.2.3 \
        ghcr.io/dxc-technology/arca@sha256:aaa ghcr.io/dxc-technology/arca@sha256:bbb
    local create
    create="$(awk '/^create$/{on=1} on' "$DOCKER_LOG")"
    assert_eq "create
-t
ghcr.io/dxc-technology/arca:1.2.3
-t
ghcr.io/dxc-technology/arca:1.2
-t
ghcr.io/dxc-technology/arca:1
-t
ghcr.io/dxc-technology/arca:latest
--annotation
index:org.opencontainers.image.description=Object storage, S3 compatible
ghcr.io/dxc-technology/arca@sha256:aaa
ghcr.io/dxc-technology/arca@sha256:bbb
--" "$create"
}

test_publish_index_requires_a_source() {
    new_repo 1.2.3 1.2.3
    mock_docker
    assert_fails publish_index arca 1.2.3
}

# --- image_build -------------------------------------------------------------

test_image_build_arca_uses_production_target_and_repo_root() {
    new_repo 1.2.3 1.2.3
    mock_docker
    local root sha
    root="$(git rev-parse --show-toplevel)"
    sha="$(git rev-parse HEAD)"
    image_build arca 1.2.3 --platform linux/arm64 --load
    assert_eq "buildx
build
--label
org.opencontainers.image.version=1.2.3
--label
org.opencontainers.image.revision=$sha
-f
$root/docker/Dockerfile
--target
production
--build-arg
ARCA_GIT_COMMIT=${sha:0:7}
--platform
linux/arm64
--load
$root
--" "$(cat "$DOCKER_LOG")"
}

test_image_build_console_uses_console_context() {
    new_repo 1.2.3 1.2.3
    mock_docker
    local root
    root="$(git rev-parse --show-toplevel)"
    image_build arca-console 1.2.3 --load
    local log
    log="$(cat "$DOCKER_LOG")"
    assert_contains "$log" $'-f\n'"$root/console/Dockerfile" &&
    assert_contains "$log" $'--load\n'"$root/console"$'\n--' &&
    if [[ "$log" == *"--target"* ]]; then echo "console has no build target"; return 1; fi
}

test_image_build_rejects_unknown_image() {
    new_repo 1.2.3 1.2.3
    mock_docker
    assert_fails image_build arca-test 1.2.3
}

# --- image_push_digest -------------------------------------------------------

test_image_push_digest_pushes_untagged_and_prints_the_digest() {
    new_repo 1.2.3 1.2.3
    mock_docker
    docker_reply() {
        # Emulate buildx writing the metadata file it was pointed at.
        local prev=""
        for arg in "$@"; do
            if [[ "$prev" == "--metadata-file" ]]; then
                echo '{"containerimage.digest": "sha256:0123abcd"}' > "$arg"
            fi
            prev="$arg"
        done
    }
    local digest
    digest="$(image_push_digest arca 1.2.3 linux/amd64,linux/arm64)"
    assert_eq "sha256:0123abcd" "$digest" &&
    assert_contains "$(cat "$DOCKER_LOG")" $'--platform\nlinux/amd64,linux/arm64' &&
    assert_contains "$(cat "$DOCKER_LOG")" \
        "type=image,name=ghcr.io/dxc-technology/arca,push-by-digest=true,name-canonical=true,push=true"
}

test_image_push_digest_fails_without_a_digest() {
    new_repo 1.2.3 1.2.3
    mock_docker
    assert_fails image_push_digest arca 1.2.3 linux/amd64
}

# --- image_scan --------------------------------------------------------------

test_image_scan_runs_trivy_on_a_saved_tarball_without_the_socket() {
    new_repo 1.2.3 1.2.3
    mock_docker
    image_scan local/image:scan
    local log
    log="$(cat "$DOCKER_LOG")"
    assert_contains "$log" $'save\n-o' &&
    assert_contains "$log" $'local/image:scan\n--' &&
    assert_contains "$log" $'--exit-code\n1\n--input\n/scan/image.tar' &&
    if [[ "$log" == *"docker.sock"* ]]; then echo "Trivy must not get the Docker socket"; return 1; fi
}

test_image_scan_fails_when_trivy_reports_findings() {
    new_repo 1.2.3 1.2.3
    mock_docker
    docker_reply() { [[ "$1" != "run" ]]; }
    assert_fails image_scan local/image:scan
}

test_image_scan_cleans_up_the_tarball() {
    new_repo 1.2.3 1.2.3
    mock_docker
    export TMPDIR
    TMPDIR="$(mktemp -d)"
    image_scan local/image:scan
    assert_eq "" "$(ls -A "$TMPDIR")"
}

test_lockfile_scan_runs_trivy_on_cargo_lock_only() {
    new_repo 1.2.3 1.2.3
    mock_docker
    lockfile_scan
    local log
    log="$(cat "$DOCKER_LOG")"
    assert_contains "$log" $'fs\n--scanners\nvuln\n--exit-code\n1\nCargo.lock\n--'
}

test_lockfile_scan_fails_when_trivy_reports_findings() {
    new_repo 1.2.3 1.2.3
    mock_docker
    docker_reply() { [[ "$1" != "run" ]]; }
    assert_fails lockfile_scan
}

# --- image_verify ------------------------------------------------------------

test_image_verify_arca_also_scans_the_rust_dependencies() {
    # The scratch image holds no package database and a plain (non-auditable)
    # Rust binary: an image scan alone sees nothing to check.
    new_repo 1.2.3 1.2.3
    mock_docker
    image_verify arca 1.2.3 linux/amd64 >/dev/null
    local log
    log="$(cat "$DOCKER_LOG")"
    assert_contains "$log" $'--load\n-t\narca-scan:amd64' &&
    assert_contains "$log" $'--input\n/scan/image.tar' &&
    assert_contains "$log" $'fs\n--scanners\nvuln'
}

test_image_verify_console_scans_the_image_only() {
    new_repo 1.2.3 1.2.3
    mock_docker
    image_verify arca-console 1.2.3 linux/arm64 >/dev/null
    local log
    log="$(cat "$DOCKER_LOG")"
    assert_contains "$log" $'--input\n/scan/image.tar' &&
    if [[ "$log" == *$'\nfs\n'* ]]; then echo "console has no Cargo.lock to scan"; return 1; fi
}

test_image_verify_fails_on_dependency_findings() {
    new_repo 1.2.3 1.2.3
    mock_docker
    docker_reply() { [[ "$*" != *"Cargo.lock"* ]]; }
    assert_fails image_verify arca 1.2.3 linux/amd64
}

test_image_verify_removes_the_scan_image() {
    new_repo 1.2.3 1.2.3
    mock_docker
    image_verify arca-console 1.2.3 linux/arm64 >/dev/null
    assert_contains "$(cat "$DOCKER_LOG")" $'rmi\narca-console-scan:arm64'
}

# -----------------------------------------------------------------------------

echo "==> bin/lib/images.sh"
for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
    run_test "$t"
done

echo ""
echo "$passed passed, $failed failed"
(( failed == 0 ))
