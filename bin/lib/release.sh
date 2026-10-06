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
#   release_preview_html <version> <notes> <out.html> <flag>
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

# Renders the notes with GitHub's own Markdown renderer and wraps them in a
# page laid out like a release: title, tag, Latest / Pre-release badge.
release_preview_html() {    # release_preview_html <version> <notes.md> <out.html> <flag>
    local version="$1" notes="$2" out="$3" flag="$4" body badge=""
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
</div>
</main>
</body>
</html>
EOF
}
