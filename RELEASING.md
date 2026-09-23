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
