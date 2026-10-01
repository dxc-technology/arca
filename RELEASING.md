# Release Procedure

This document describes the step-by-step process for creating a new Arca release.

## Steps

### 1. Analyze changes

Run `git log --oneline $(git describe --tags --abbrev=0)..HEAD` to list all commits since the last release tag. Categorize them (features, fixes, refactors, docs).

### 2. Determine semver bump

- **PATCH** — bug fixes, internal refactors, doc-only changes, no API/behavior changes.
- **MINOR** — new features, new S3 operations, new CLI commands, new Admin API endpoints, backward-compatible additions.
- **MAJOR** — breaking changes to config format, storage layout, API contracts, or anything requiring user migration steps.

### 3. Propose and get approval

Present the proposed version number with a brief summary of what changed and why it maps to that semver level. Wait for explicit approval before proceeding.

### 4. Update version in all locations

- `Cargo.toml` → `[workspace.package] version = "X.Y.Z"`
- `console/index.html` → `window.ARCA_CONSOLE_VERSION = "X.Y.Z"`
- `documentation/docs/roadmap.md` → Phase Summary table version column (if completing a phase)
- `README.md` → Status section, "latest release **vX.Y.Z**"
- `publiccode.yml` → `softwareVersion: "X.Y.Z"` **and** `releaseDate: "YYYY-MM-DD"` (the release date, not today's date if they differ)

Grep for the previous version string across the repo (`grep -rn "vX.Y.Z-old"`) to catch any location missed above. Note that `releaseDate` carries no version string, so the grep will not catch it — check it by hand.

After editing `publiccode.yml`, re-validate it; the Developers Italia crawler rejects an invalid file:

```bash
docker run --rm -v "$PWD":/work -w /work italia/publiccode-parser-go:latest publiccode.yml
```

No output and exit code 0 means valid.

### 5. Update CHANGELOG.md

- Move all `[Unreleased]` entries into a new `[X.Y.Z] — YYYY-MM-DD` section
- Clear the `[Unreleased]` section (keep the heading)
- Update comparison links at the bottom of the file: add `[X.Y.Z]` link, update `[Unreleased]` to compare against the new tag

### 6. Update README.md

Refresh test coverage counts if they changed since the last release.

### 7. Rebuild documentation

Run `bin/docs-build` to regenerate the `docs/` output directory.

### 8. Commit

Single commit with message: `Release vX.Y.Z` (no other changes in this commit).

### 9. Create tag

```bash
git tag vX.Y.Z
```

### 10. Push

```bash
git push && git push --tags
```

### 11. Check the published container images

Pushing the `vX.Y.Z` tag triggers the **Publish images** workflow
(`.github/workflows/publish-images.yml`), which publishes both production
images as multi-arch (`linux/amd64` + `linux/arm64`) images:

- `ghcr.io/dxc-technology/arca`
- `ghcr.io/dxc-technology/arca-console`

Each is tagged `X.Y.Z`; `X.Y`, `X` and `latest` move only if this is the
newest release of its minor line / major line / overall, so releasing a fix
for an older line never moves them backwards. A prerelease tag
(`vX.Y.Z-rc.N`) is published under its exact version only.

The major-only `X` tag is not published while `X` is 0: in 0.x a minor bump
may break compatibility, so `:0` would promise a stability the project does
not offer. `release_tags` (`bin/lib/images.sh`) adds it automatically from
1.0.0, so nothing needs to be done by hand at that point.

Every platform is built natively on its own runner and scanned by Trivy before
anything is tagged — the image itself, plus `Cargo.lock` for `arca`, whose
scratch image carries nothing an image scan can check. **Any finding fails
the release**: fix it (a pinned package upgrade, see "Base Image Pinning and
Upgrades" in `AGENTS.md`, or a dependency bump), make a new patch release, and
never delete or move the tag. The workflow also refuses a tag that disagrees
with `Cargo.toml` or `console/index.html`.

Watch the run and check the result:

```bash
gh run watch --repo dxc-technology/arca
docker buildx imagetools inspect ghcr.io/dxc-technology/arca:X.Y.Z
```

The index must list `linux/amd64` and `linux/arm64`.

**Re-publishing** an existing release (e.g. after a registry-side mishap):
run the workflow by hand with *Run workflow* and select the release **tag**,
not a branch.

**Fallback without CI:** from a clean checkout of the release tag, logged in
with `docker login ghcr.io` using a token with the `write:packages` scope:

```bash
bin/build --push
```

It runs the same pipeline (`bin/lib/images.sh`), with the same scans, from the
local machine. The non-native platform builds under emulation, which is slow
for the Rust image.

#### One-time setup (first publication only)

A new GHCR package starts **private**. After the first run, an organization or
package admin must, for each of the two packages
(`https://github.com/orgs/dxc-technology/packages`):

1. open **Package settings** and check that the package is linked to the
   `arca` repository (the workflow links it automatically) with
   *Inherit access from source repository* enabled;
2. under **Danger Zone**, **Change visibility** to **Public** — this needs the
   organization to allow public packages.
