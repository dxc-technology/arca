#!/usr/bin/env bash
# Unit tests for bin/lib/release.sh and bin/release (GitHub releases). Run
# them with `bin/test scripts`, which executes this file in a throwaway
# container.
#
# Every test runs in its own subshell, inside a scratch git repository with a
# bare `origin`, and `gh` is a fake executable on PATH that records its
# arguments: nothing here talks to GitHub.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LIB="$ROOT/bin/lib/release.sh"
RELEASE="$ROOT/bin/release"

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

assert_not_contains() {     # assert_not_contains <haystack> <needle>
    if [[ "$1" == *"$2"* ]]; then
        echo "did not expect to find: $2"
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

CHANGELOG='# Changelog

## [Unreleased]

## [0.3.0] — 2026-10-06

### Added

- **A new command.** It does many things, described at length here.
  A continuation line that must not appear in the notes.
- **`[storage] knob` setting** (default `true`). Details.

### Fixed

- **A bug no longer happens** (TD-001). Long explanation.

## [0.2.0] — 2026-09-01

### Added

- **Something older.** Must not appear in the 0.3.0 notes.
'

# A scratch repository with a CHANGELOG, final releases v0.1.0 and v0.2.0
# plus the given tags, all pushed to a bare `origin`.
new_repo() {    # new_repo [tag...]
    local dir
    dir="$(mktemp -d)"
    git init -q --bare "$dir/origin.git"
    git init -q "$dir/work"
    cd "$dir/work" || exit 1
    git config user.email test@example.com
    git config user.name test
    git remote add origin "$dir/origin.git"
    printf '%s' "$CHANGELOG" > CHANGELOG.md
    git add -A
    git commit -qm init
    local t
    for t in v0.1.0 v0.2.0 "$@"; do
        git tag "$t"
    done
    git push -q origin --tags 2>/dev/null
}

# Puts a fake `gh` first on PATH. It appends its arguments, one per line and
# followed by `--`, to $GH_LOG, and answers like GitHub would:
#   GH_RELEASE_EXISTS=1    `release view` succeeds (a release already exists)
#   GH_RUN_CONCLUSION      what `run list` reports for the publish run
mock_gh() {
    local bin
    bin="$(mktemp -d)"
    GH_LOG="$(mktemp)"
    export GH_LOG GH_RELEASE_EXISTS="${GH_RELEASE_EXISTS:-0}" GH_RUN_CONCLUSION="${GH_RUN_CONCLUSION:-success}"
    cat > "$bin/gh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" -- >> "$GH_LOG"
case "$1 $2" in
    "release view") [[ "$GH_RELEASE_EXISTS" == 1 ]] ;;
    "run list") echo "$GH_RUN_CONCLUSION" ;;
    "api markdown") echo "<p>rendered by github</p>" ;;
    "release create") echo "https://github.com/dxc-technology/arca/releases/tag/$3" ;;
    *) exit 0 ;;
esac
EOF
    chmod +x "$bin/gh"
    PATH="$bin:$PATH"
}

source "$LIB"

# --- release_notes -----------------------------------------------------------

test_notes_keep_section_headings_and_bullet_titles_only() {
    new_repo v0.3.0
    local notes
    notes="$(release_notes 0.3.0 CHANGELOG.md)" || return 1
    assert_contains "$notes" $'### Added\n\n- A new command\n- `[storage] knob` setting\n\n### Fixed\n\n- A bug no longer happens' &&
    assert_not_contains "$notes" "described at length" &&
    assert_not_contains "$notes" "continuation line" &&
    assert_not_contains "$notes" "TD-001"
}

test_notes_stop_at_the_next_version() {
    new_repo v0.3.0
    assert_not_contains "$(release_notes 0.3.0 CHANGELOG.md)" "Something older"
}

test_notes_footer_links_changelog_compare_and_images() {
    new_repo v0.3.0
    local notes
    notes="$(release_notes 0.3.0 CHANGELOG.md)" || return 1
    assert_contains "$notes" "https://github.com/dxc-technology/arca/blob/v0.3.0/CHANGELOG.md" &&
    assert_contains "$notes" "https://github.com/dxc-technology/arca/compare/v0.2.0...v0.3.0" &&
    assert_contains "$notes" "docker pull ghcr.io/dxc-technology/arca:0.3.0" &&
    assert_contains "$notes" "docker pull ghcr.io/dxc-technology/arca-console:0.3.0"
}

test_notes_of_the_first_release_have_no_compare_link() {
    new_repo
    printf '## [0.1.0] — 2026-01-01\n\n### Added\n\n- **Everything.**\n' > CHANGELOG.md
    assert_eq "" "$(previous_release_tag 0.1.0)" &&
    assert_not_contains "$(release_notes 0.1.0 CHANGELOG.md)" "/compare/"
}

test_notes_fail_without_the_version_section() {
    new_repo v0.4.0
    assert_fails release_notes 0.4.0 CHANGELOG.md
}

test_notes_fail_on_a_bullet_without_a_bold_title() {
    new_repo v0.3.0
    printf '## [0.3.0] — 2026-10-06\n\n### Added\n\n- plain bullet, no title\n' > CHANGELOG.md
    assert_fails release_notes 0.3.0 CHANGELOG.md
}

test_notes_fail_on_a_section_without_bullets() {
    new_repo v0.3.0
    printf '## [0.3.0] — 2026-10-06\n\nNothing here.\n\n## [0.2.0] — 2026-09-01\n' > CHANGELOG.md
    assert_fails release_notes 0.3.0 CHANGELOG.md
}

# --- previous_release_tag / release_flags ------------------------------------

test_previous_release_tag_skips_prereleases() {
    new_repo v0.3.0-rc.1 v0.3.0
    assert_eq "v0.2.0" "$(previous_release_tag 0.3.0)"
}

test_flags_mark_the_newest_release_latest() {
    new_repo v0.3.0
    assert_eq "--latest" "$(release_flags 0.3.0)"
}

test_flags_never_move_latest_backwards_for_an_older_line() {
    new_repo v0.3.0 v0.2.1
    assert_eq "--latest=false" "$(release_flags 0.2.1)"
}

test_flags_mark_prereleases() {
    new_repo v0.4.0-rc.1
    assert_eq "--prerelease" "$(release_flags 0.4.0-rc.1)"
}

# --- release_preview_html ----------------------------------------------------

test_preview_uses_githubs_renderer_and_shows_the_badge() {
    new_repo v0.3.0
    mock_gh
    release_notes 0.3.0 CHANGELOG.md > notes.md
    release_preview_html 0.3.0 notes.md preview.html --latest || return 1
    local html
    html="$(cat preview.html)"
    assert_contains "$html" "<p>rendered by github</p>" &&
    assert_contains "$html" "Latest" &&
    assert_contains "$(cat "$GH_LOG")" $'api\nmarkdown'
}

# --- bin/release -------------------------------------------------------------

test_release_dry_run_never_creates_the_release() {
    new_repo v0.3.0
    mock_gh
    bash "$RELEASE" v0.3.0 --dry-run >/dev/null || return 1
    assert_not_contains "$(cat "$GH_LOG")" $'release\ncreate'
}

test_release_yes_creates_it_on_the_existing_tag() {
    new_repo v0.3.0
    mock_gh
    bash "$RELEASE" v0.3.0 --yes >/dev/null || return 1
    local log
    log="$(cat "$GH_LOG")"
    assert_contains "$log" $'release\ncreate\nv0.3.0\n--repo\ndxc-technology/arca\n--verify-tag\n--title\nv0.3.0\n--notes-file' &&
    assert_contains "$log" $'\n--latest\n--'
}

test_release_asks_and_stops_on_no() {
    new_repo v0.3.0
    mock_gh
    echo n | bash "$RELEASE" v0.3.0 >/dev/null
    assert_not_contains "$(cat "$GH_LOG")" $'release\ncreate'
}

# Runs bin/release expecting it to fail; prints its output.
release_must_fail() {   # release_must_fail <args...>
    local out
    if out="$(bash "$RELEASE" "$@" 2>&1)"; then
        echo "expected bin/release $* to fail"
        return 1
    fi
    echo "$out"
}

test_release_refuses_when_the_release_exists() {
    new_repo v0.3.0
    GH_RELEASE_EXISTS=1
    mock_gh
    local out
    out="$(release_must_fail v0.3.0 --yes)" || { echo "$out"; return 1; }
    assert_contains "$out" "already exists" &&
    assert_not_contains "$(cat "$GH_LOG")" $'release\ncreate'
}

test_release_refuses_until_the_images_are_published() {
    new_repo v0.3.0
    GH_RUN_CONCLUSION=failure
    mock_gh
    local out
    out="$(release_must_fail v0.3.0 --yes)" || { echo "$out"; return 1; }
    assert_contains "$out" "Publish images run for v0.3.0 has not succeeded (failure)" &&
    assert_not_contains "$(cat "$GH_LOG")" $'release\ncreate'
}

test_release_refuses_a_tag_missing_on_origin() {
    new_repo v0.3.0
    git tag v0.3.1
    mock_gh
    local out
    out="$(release_must_fail v0.3.1 --yes)" || { echo "$out"; return 1; }
    assert_contains "$out" "not on origin"
}

test_release_reads_the_changelog_of_the_tag() {
    new_repo v0.3.0
    mock_gh
    # A later, uncommitted edit must not leak into the v0.3.0 notes.
    printf '## [0.3.0] — 2026-10-06\n\n### Added\n\n- **Edited later.**\n' > CHANGELOG.md
    local out
    out="$(bash "$RELEASE" v0.3.0 --dry-run)" || return 1
    local notes
    notes="$(sed -n 's/^Notes: *//p' <<< "$out")"
    assert_contains "$(cat "$notes")" "- A new command" &&
    assert_not_contains "$(cat "$notes")" "Edited later"
}

# -----------------------------------------------------------------------------

echo "==> bin/lib/release.sh, bin/release"
for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
    run_test "$t"
done

echo ""
echo "$passed passed, $failed failed"
(( failed == 0 ))
