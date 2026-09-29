# Shared library for Arca's published container images: their names, the
# release version and tags they are published under, and the multi-arch
# build -> scan -> push -> index pipeline.
#
# Sourced by bin/build (`bin/build --push`, the local fallback) and by the
# publish-images GitHub Actions workflow, so both paths build, scan and tag
# the images the same way. Requires git, jq and docker buildx.
#
# Only the two production images are published. Local builds never use the
# release tags: compose tags them after their build target
# (`<repo>:production`, `<repo>:development`), so a working-tree build can
# neither pass for a release nor shadow a published `latest`.
#
# Usage:
#   image_repo <image>                       registry repository of an image
#   release_version                          X.Y.Z from the release tag at HEAD
#   release_tags <version>                   tags a release is published under
#   use_publish_builder                      select the buildx builder
#   image_verify <image> <version> <platform>          build one platform, scan it
#                                            (and Cargo.lock, for arca)
#   image_push_digest <image> <version> <platforms>    push untagged, print digest
#   publish_index <image> <version> <repo@digest>...   tag the multi-arch index
#   publish_release                          all of the above, for both images

ARCA_REGISTRY="ghcr.io/dxc-technology"
PUBLISHED_IMAGES=(arca arca-console)
PUBLISH_PLATFORMS="linux/amd64,linux/arm64"
PUBLISH_BUILDER="arca-publish"

image_repo() {  # image_repo <image>
    case "$1" in
        arca|arca-console) echo "$ARCA_REGISTRY/$1" ;;
        *) echo "Error: '$1' is not a published image" >&2; return 1 ;;
    esac
}

# The version being released, from the vX.Y.Z (or vX.Y.Z-pre) tag at HEAD.
# Refuses unless the tree is clean and the tag, Cargo.toml and the console
# all carry the same version: a published image must be exactly a release.
release_version() {
    local root tags tag version cargo console
    root="$(git rev-parse --show-toplevel)" || return 1

    tags="$(git tag --points-at HEAD --list 'v[0-9]*')"
    if [[ -z "$tags" ]]; then
        echo "Error: HEAD carries no release tag (vX.Y.Z)" >&2
        return 1
    fi
    if [[ "$(wc -l <<< "$tags")" -ne 1 ]]; then
        echo "Error: HEAD carries more than one release tag: ${tags//$'\n'/ }" >&2
        return 1
    fi
    tag="$tags"
    if ! [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
        echo "Error: tag '$tag' is not a semantic version (vX.Y.Z)" >&2
        return 1
    fi
    version="${tag#v}"

    if [[ -n "$(git -C "$root" status --porcelain)" ]]; then
        echo "Error: the working tree has uncommitted or untracked changes" >&2
        return 1
    fi

    cargo="$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$root/Cargo.toml")"
    if [[ "$cargo" != "$version" ]]; then
        echo "Error: tag $tag does not match Cargo.toml version '$cargo'" >&2
        return 1
    fi
    console="$(sed -n 's/.*ARCA_CONSOLE_VERSION *= *"\([^"]*\)".*/\1/p' "$root/console/index.html")"
    if [[ "$console" != "$version" ]]; then
        echo "Error: tag $tag does not match console/index.html version '$console'" >&2
        return 1
    fi

    echo "$version"
}

# The newest final release (no prerelease suffix) matching a tag glob.
_newest_release() {
    git tag --list "$1" --sort=-v:refname \
        | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -n 1 || true
}

# Tags a release is published under, one per line: always the exact version;
# X.Y only if it is the newest release of its minor line, and `latest` only
# if it is the newest release overall, so publishing a fix for an older line
# never moves either backwards. A prerelease gets its exact version only. No
# major-only tag: in 0.x a minor bump may break compatibility.
release_tags() {    # release_tags <version>
    local version="$1" minor
    echo "$version"
    [[ "$version" == *-* ]] && return 0
    minor="${version%.*}"
    [[ "$(_newest_release "v$minor.*")" == "v$version" ]] && echo "$minor"
    [[ "$(_newest_release 'v*')" == "v$version" ]] && echo "latest"
    return 0
}

# A docker-container builder: unlike the default driver it can push a
# multi-platform image by digest, and it keeps its own cache, so the push
# after a scan reuses the scanned build instead of rebuilding.
use_publish_builder() {
    if ! docker buildx inspect "$PUBLISH_BUILDER" >/dev/null 2>&1; then
        docker buildx create --name "$PUBLISH_BUILDER" --driver docker-container >/dev/null
    fi
    export BUILDX_BUILDER="$PUBLISH_BUILDER"
}

# Build a published image with its release labels. The static OCI labels
# (source, description, licenses...) live in the Dockerfiles; version and
# revision are only known here.
image_build() {     # image_build <image> <version> <buildx args...>
    local image="$1" version="$2"
    shift 2
    local root sha context
    root="$(git rev-parse --show-toplevel)" || return 1
    sha="$(git rev-parse HEAD)" || return 1
    local args=(
        --label "org.opencontainers.image.version=$version"
        --label "org.opencontainers.image.revision=$sha"
    )
    case "$image" in
        arca)
            args+=(-f "$root/docker/Dockerfile" --target production
                   --build-arg "ARCA_GIT_COMMIT=${sha:0:7}")
            context="$root"
            ;;
        arca-console)
            args+=(-f "$root/console/Dockerfile")
            context="$root/console"
            ;;
        *)
            echo "Error: '$image' is not a published image" >&2
            return 1
            ;;
    esac
    docker buildx build "${args[@]}" "$@" "$context"
}

# Run Trivy from its own image, from the repository root (read-only) so
# trivy.yaml and .trivyignore apply, with no Docker socket and no registry
# credentials. Any finding fails. Extra `docker run` options precede `--`.
_trivy() {  # _trivy [docker run options...] -- <trivy args...>
    local root opts=()
    root="$(git rev-parse --show-toplevel)" || return 1
    while [[ $# -gt 0 && "$1" != "--" ]]; do opts+=("$1"); shift; done
    shift
    docker run --rm "${opts[@]}" -v "$root:/work:ro" -w /work \
        -v arca-trivy-cache:/root/.cache \
        aquasec/trivy:latest "$@"
}

# Scan a local image, from a saved tarball.
image_scan() {      # image_scan <local image>
    local ref="$1" dir rc=0
    dir="$(mktemp -d)"
    if docker save -o "$dir/image.tar" "$ref"; then
        _trivy -v "$dir:/scan:ro" -- image --exit-code 1 --input /scan/image.tar || rc=$?
    else
        rc=1
    fi
    rm -rf "$dir"
    return "$rc"
}

# Scan the Rust dependency tree. The arca image cannot stand in for this: it
# is scratch plus a plain (non-auditable) static binary, so an image scan
# finds no package database and nothing to match advisories against.
lockfile_scan() {
    _trivy -- fs --scanners vuln --exit-code 1 Cargo.lock
}

# Build one platform of an image into the local image store and scan it.
image_verify() {    # image_verify <image> <version> <platform>
    local image="$1" version="$2" platform="$3"
    local scan_ref="$image-scan:${platform##*/}" rc=0
    echo "==> Building $image for $platform..."
    image_build "$image" "$version" --platform "$platform" --load -t "$scan_ref" || return 1
    echo "==> Scanning $image for $platform..."
    image_scan "$scan_ref" || rc=$?
    docker rmi "$scan_ref" >/dev/null 2>&1 || true
    if [[ "$rc" -eq 0 && "$image" == "arca" ]]; then
        echo "==> Scanning the Rust dependencies (Cargo.lock)..."
        lockfile_scan || rc=$?
    fi
    return "$rc"
}

# Push an image, untagged, by digest; prints the digest. Tags are applied only
# once every platform is in, by publish_index.
image_push_digest() {   # image_push_digest <image> <version> <platforms>
    local image="$1" version="$2" platforms="$3" repo meta digest
    repo="$(image_repo "$image")" || return 1
    meta="$(mktemp)"
    echo "==> Pushing $image ($platforms) by digest..." >&2
    if ! image_build "$image" "$version" --platform "$platforms" \
            --metadata-file "$meta" \
            --output "type=image,name=$repo,push-by-digest=true,name-canonical=true,push=true" >&2; then
        rm -f "$meta"
        return 1
    fi
    digest="$(jq -r '."containerimage.digest" // empty' "$meta" 2>/dev/null)"
    rm -f "$meta"
    if [[ -z "$digest" ]]; then
        echo "Error: buildx reported no digest for $image" >&2
        return 1
    fi
    echo "$digest"
}

# OCI labels of an image, as JSON. For an index, those of its first real
# platform (attestation manifests have platform unknown/unknown).
image_labels() {    # image_labels <repo@digest>
    local ref="$1" raw digest
    raw="$(docker buildx imagetools inspect --raw "$ref")" || return 1
    if jq -e '.manifests' <<< "$raw" >/dev/null 2>&1; then
        digest="$(jq -r '[.manifests[] | select(.platform.os != "unknown")][0].digest' <<< "$raw")"
        ref="${ref%@*}@$digest"
    fi
    docker buildx imagetools inspect "$ref" --format '{{json .Image.Config.Labels}}'
}

# OCI labels (JSON on stdin) as index-level annotations, one per line. GHCR
# reads a multi-arch image's description and source repository from the
# index, not from the per-platform image configs.
annotation_args() {
    jq -r 'to_entries[]? | select(.key | startswith("org.opencontainers.image."))
           | "index:\(.key)=\(.value)"'
}

# Merge per-platform pushes into one index carrying the image's annotations,
# and tag it with the release tags.
publish_index() {   # publish_index <image> <version> <repo@digest>...
    local image="$1" version="$2"
    shift 2
    if [[ $# -eq 0 ]]; then
        echo "Error: publish_index needs at least one source digest" >&2
        return 1
    fi
    local repo tag line labels args=()
    repo="$(image_repo "$image")" || return 1
    while IFS= read -r tag; do
        args+=(-t "$repo:$tag")
    done < <(release_tags "$version")
    labels="$(image_labels "$1")" || return 1
    while IFS= read -r line; do
        [[ -n "$line" ]] && args+=(--annotation "$line")
    done < <(annotation_args <<< "$labels")
    echo "==> Publishing $repo: $(release_tags "$version" | tr '\n' ' ')"
    docker buildx imagetools create "${args[@]}" "$@"
}

# The whole release from this machine: every platform of every image is
# built and scanned before anything is pushed. Needs `docker login ghcr.io`
# with a token that has the write:packages scope. Platforms other than the
# host's build under emulation, which for the Rust image is slow.
publish_release() {
    local version image repo digest platform
    version="$(release_version)" || return 1
    use_publish_builder
    for image in "${PUBLISHED_IMAGES[@]}"; do
        for platform in ${PUBLISH_PLATFORMS//,/ }; do
            image_verify "$image" "$version" "$platform" || return 1
        done
    done
    for image in "${PUBLISHED_IMAGES[@]}"; do
        repo="$(image_repo "$image")"
        digest="$(image_push_digest "$image" "$version" "$PUBLISH_PLATFORMS")" || return 1
        publish_index "$image" "$version" "$repo@$digest" || return 1
    done
}
