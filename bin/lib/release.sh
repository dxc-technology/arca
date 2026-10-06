# Shared library for Arca's GitHub releases: the release notes, derived from
# CHANGELOG.md, the Latest / pre-release flags and an HTML preview rendered by
# GitHub itself.
#
# Sourced by bin/release. Requires git, awk and, for the preview, gh.
#
# The notes are a summary, never hand-written: for every section of the
# version's CHANGELOG entry they keep the heading and the bold title of each
# bullet, then link the full entry, the compare view and the published images.
# So the GitHub release can never say something the CHANGELOG does not.
#
# Usage:
#   release_notes <version> [changelog]      notes in Markdown, on stdout
#   previous_release_tag <version>           final release tag before it
#   release_flags <version>                  gh flag: --latest, --latest=false
#                                            or --prerelease
#   release_archive_names <version>          binary archives a release carries
#   release_assets_verify <dir> <version>    check a downloaded release-binaries
#                                            artifact (archives, SHA256SUMS)
#   release_preview_html <version> <notes> <out.html> <flag> [assets dir]
#                                            the release page as GitHub shows it

# shellcheck source=bin/lib/images.sh
source "$(dirname "${BASH_SOURCE[0]}")/images.sh"

GITHUB_REPO="dxc-technology/arca"
GITHUB_URL="https://github.com/$GITHUB_REPO"

# The final release (no prerelease suffix) tagged right before v<version>,
# empty for the first release. Needs the tag itself to exist.
previous_release_tag() {    # previous_release_tag <version>
    git tag --list 'v*' --sort=-v:refname | awk -v tag="v$1" '
        $0 == tag { found = 1; next }
        found && /^v[0-9]+\.[0-9]+\.[0-9]+$/ { print; exit }'
}

release_notes() {   # release_notes <version> [changelog]
    local version="$1" changelog="${2:-CHANGELOG.md}" body rc=0 previous repo
    if [[ ! -f "$changelog" ]]; then
        echo "Error: $changelog not found" >&2
        return 1
    fi
    # Exit 2: no section for the version; 3: a bullet without a bold title;
    # 4: a section with no bullet at all.
    body="$(awk -v head="## [$version]" '
        index($0, head) == 1 { found = 1; next }
        !found { next }
        /^## \[/ { exit }
        /^### / { if (sections++) print ""; print; print ""; next }
        /^- / {
            if (!match($0, /^- \*\*[^*]+\*\*/)) { bad = 1; exit }
            title = substr($0, 5, RLENGTH - 6)
            sub(/\.$/, "", title)
            print "- " title
            bullets++
        }
        END {
            if (!found) exit 2
            if (bad) exit 3
            if (!bullets) exit 4
        }' "$changelog")" || rc=$?
    case "$rc" in
        0) ;;
        2) echo "Error: $changelog has no [$version] section" >&2; return 1 ;;
        3) echo "Error: a [$version] bullet in $changelog does not start with a **bold title**" >&2; return 1 ;;
        4) echo "Error: the [$version] section of $changelog lists no change" >&2; return 1 ;;
        *) echo "Error: could not read $changelog" >&2; return 1 ;;
    esac

    previous="$(previous_release_tag "$version")"
    printf '%s\n\n---\n\n' "$body"
    printf 'Full release notes: [CHANGELOG.md](%s/blob/v%s/CHANGELOG.md)\n\n' "$GITHUB_URL" "$version"
    if [[ -n "$previous" ]]; then
        printf '**Full changelog**: %s/compare/%s...v%s\n\n' "$GITHUB_URL" "$previous" "$version"
    fi
    printf '**Container images** (`linux/amd64`, `linux/arm64`):\n\n```bash\n'
    for repo in "${PUBLISHED_IMAGES[@]}"; do
        printf 'docker pull %s:%s\n' "$(image_repo "$repo")" "$version"
    done
    printf '```\n'
}

# The same rule as the image tags (release_tags in images.sh): only the newest
# final release overall is Latest, so a fix for an older line never takes the
# badge from a newer release.
release_flags() {   # release_flags <version>
    if [[ "$1" == *-* ]]; then
        echo "--prerelease"
    elif release_tags "$1" | grep -qx latest; then
        echo "--latest"
    else
        echo "--latest=false"
    fi
}

# The binary archives a release carries, one per published platform (built by
# binary_archive in images.sh).
release_archive_names() {   # release_archive_names <version>
    local platform
    for platform in ${PUBLISH_PLATFORMS//,/ }; do
        echo "arca-$1-linux-${platform##*/}.tar.gz"
    done
}

_sha256_check() {   # _sha256_check <SHA256SUMS>
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$1"
    else
        shasum -a 256 -c "$1"    # macOS
    fi
}

# A downloaded release-binaries artifact is complete and intact: every archive
# the release must carry is there and listed in SHA256SUMS, and every listed
# file matches its checksum.
release_assets_verify() {   # release_assets_verify <dir> <version>
    local dir="$1" version="$2" name
    if [[ ! -f "$dir/SHA256SUMS" ]]; then
        echo "Error: SHA256SUMS is missing from the release-binaries artifact" >&2
        return 1
    fi
    while IFS= read -r name; do
        if [[ ! -f "$dir/$name" ]]; then
            echo "Error: $name is missing from the release-binaries artifact" >&2
            return 1
        fi
        if ! grep -q "  $name\$" "$dir/SHA256SUMS"; then
            echo "Error: $name is not listed in SHA256SUMS" >&2
            return 1
        fi
    done < <(release_archive_names "$version")
    if ! (cd "$dir" && _sha256_check SHA256SUMS) >/dev/null 2>&1; then
        echo "Error: checksum mismatch in the release-binaries artifact" >&2
        return 1
    fi
}

_human_size() {     # _human_size <file>
    wc -c < "$1" | awk '{ s = $1; split("B KB MB GB", u, " "); i = 1
        while (s >= 1024 && i < 4) { s /= 1024; i++ }
        printf (i == 1 ? "%d %s" : "%.1f %s"), s, u[i] }'
}

# The "Assets" box of the release page: the attached files, then the two
# source archives GitHub adds to every release by itself.
_assets_html() {    # _assets_html <assets dir or empty>
    local dir="$1" file items="" count=2
    if [[ -n "$dir" ]]; then
        for file in "$dir"/*.tar.gz "$dir/SHA256SUMS"; do
            [[ -f "$file" ]] || continue
            items+="<li><span>$(basename "$file")</span><span class=\"size\">$(_human_size "$file")</span></li>"
            count=$((count + 1))
        done
    fi
    items+='<li><span>Source code (zip)</span><span class="size">added by GitHub</span></li>'
    items+='<li><span>Source code (tar.gz)</span><span class="size">added by GitHub</span></li>'
    printf '<details class="assets" open><summary>Assets <span class="count">%d</span></summary><ul>%s</ul></details>' \
        "$count" "$items"
}

# Renders the notes with GitHub's own Markdown renderer and wraps them in a
# page laid out like a release: title, tag, Latest / Pre-release badge.
release_preview_html() {    # release_preview_html <version> <notes.md> <out.html> <flag> [assets dir]
    local version="$1" notes="$2" out="$3" flag="$4" assets="${5:-}" body badge=""
    body="$(gh api markdown -f mode=gfm -f context="$GITHUB_REPO" -F text=@"$notes")" || {
        echo "Error: GitHub could not render the release notes" >&2
        return 1
    }
    case "$flag" in
        --latest) badge='<span class="badge latest">Latest</span>' ;;
        --prerelease) badge='<span class="badge pre">Pre-release</span>' ;;
    esac
    cat > "$out" <<EOF
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>v$version release preview</title>
<link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/github-markdown-css/5.9.0/github-markdown.min.css">
<style>
  :root { --bg: #ffffff; --fg: #1f2328; --muted: #59636e; --border: #d1d9e0; }
  @media (prefers-color-scheme: dark) {
    :root { --bg: #0d1117; --fg: #f0f6fc; --muted: #9198a1; --border: #3d444d; }
  }
  body { margin: 0; background: var(--bg); color: var(--fg);
         font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif; }
  main { max-width: 980px; margin: 32px auto; padding: 0 16px; }
  .note { color: var(--muted); font-size: 14px; margin: 0 0 16px; }
  .release { border: 1px solid var(--border); border-radius: 6px; padding: 24px; }
  h1 { font-size: 32px; margin: 0 12px 0 0; display: inline-block; vertical-align: middle; }
  .badge { display: inline-block; vertical-align: middle; border: 1px solid; border-radius: 2em;
           padding: 0 10px; font-size: 12px; font-weight: 500; line-height: 22px; }
  .badge.latest { color: #1a7f37; border-color: #1a7f37; }
  .badge.pre { color: #9a6700; border-color: #9a6700; }
  .tag { color: var(--muted); font-size: 14px; margin: 8px 0 24px; }
  .markdown-body { background: transparent; }
  .assets { margin-top: 24px; }
  .assets summary { font-weight: 600; font-size: 20px; cursor: pointer; }
  .assets .count { display: inline-block; min-width: 20px; padding: 0 6px; border-radius: 2em;
                   font-size: 12px; text-align: center; background: var(--border); }
  .assets ul { list-style: none; padding: 0; margin: 12px 0 0;
               border: 1px solid var(--border); border-radius: 6px; }
  .assets li { display: flex; justify-content: space-between; padding: 8px 16px;
               border-top: 1px solid var(--border); font-size: 14px; }
  .assets li:first-child { border-top: 0; }
  .assets .size { color: var(--muted); }
</style>
</head>
<body>
<main>
<p class="note">Preview of the GitHub release, not published yet.</p>
<div class="release">
<h1>v$version</h1>$badge
<p class="tag">Tag v$version</p>
<article class="markdown-body">
$body
</article>
$(_assets_html "$assets")
</div>
</main>
</body>
</html>
EOF
}
