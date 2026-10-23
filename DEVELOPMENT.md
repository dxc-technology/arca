# Development Procedures

Detailed maintainer procedures that `AGENTS.md` points to. Read the relevant section before doing the task it covers; the always-applicable rules stay in `AGENTS.md`.

## Dependency licence audit

The policy is in `AGENTS.md` (Dependency Licensing Policy). Before adding or upgrading any dependency, run the full transitive audit in a container and confirm zero copyleft:

```bash
docker run --rm -v "$PWD":/work -w /work rust:alpine sh -c \
  'apk add --no-cache musl-dev >/dev/null; cargo install cargo-license >/dev/null; \
   cargo license --all-features --tsv' \
  | awk -F"\t" "NR>1{print \$5}" | grep -wiE "GPL|AGPL|LGPL|MPL-|SSPL|CDDL|EPL|OSL"
# must print nothing (an OR-with-permissive-option match must be reviewed by hand)
```

After any dependency change, regenerate `THIRD-PARTY-NOTICES.md` (config in `about.toml` / `about.hbs`). The `cargo-about` binary is gated behind the `cli` feature, so the install must pass `--features cli`:

```bash
docker run --rm -v "$PWD":/work -w /work rust:alpine sh -c \
  'apk add --no-cache build-base perl; cargo install cargo-about --features cli; \
   cargo about generate --all-features about.hbs -o THIRD-PARTY-NOTICES.md'
```

## Self-contained test modes

Self-contained test modes (`tls`, `encryption`, `kms`, `postgres`, the connectors, ..., and `bin/perf-test --tls/--encryption` / `bin/perf-test suite`) never touch the developer's stack: `use_test_project <mode>` (`bin/lib/compose.sh`) gives each its own compose project (`arca-test-<mode>`), its own `config/.generated-test-<mode>.toml` and ephemeral loopback host ports, so they can run while `bin/arca start` is up.

- A new self-contained mode must call `use_test_project` first and get a case in `tests/scripts/test_isolation_test.sh`.
- A port published in a compose overlay must be a `${<NAME>_HOST_PORT:-<port>}` that `use_test_project` remaps (`bin/test scripts` checks every overlay).
- Modes that start no server (`integration`, `presigned`, `ssec`) run against the running one with `--no-deps`.
- The suites that need names only a mode's own compose project defines (`test_cluster.py`, `test_replication.py`, the delivery classes of `test_notifications.py`) skip themselves unless that mode set their environment variable (`ARCA_NODE1_ENDPOINT`, `ARCA_REPLICA_ENDPOINT`, `WEBHOOK_RECEIVER_URL`), so a plain `bin/test integration` has no failing test to learn to ignore; a new suite of that kind must do the same.
- The cluster mode looks node ports up with `docker compose port` on every poll (an ephemeral port changes when a container restarts) and `bin/cluster partition/heal` follow it through `ARCA_CLUSTER_PROJECT`.
- `scripts`, `s3-report` and `perf-report` need no server at all.

## Base Image Pinning and Upgrades

Every image Arca **deploys** pins its base image to an exact version tag, so the same Dockerfile keeps producing the same toolchain and the same runtime packages over time:

| Dockerfile | Stage | Base |
|---|---|---|
| `docker/Dockerfile` | `builder` (and `dev`, which inherits it) | Rust on Alpine — **pinned** |
| `docker/Dockerfile` | `production` | `scratch` — no tag exists, nothing to pin |
| `docker/Dockerfile` | `development` | Debian slim — **pinned** |
| `console/Dockerfile` | — | Alpine — **pinned** |

**Never write the concrete pinned versions anywhere but the `FROM` lines themselves.** The Dockerfiles are the single source of truth; a tag copied into another file is a second place to forget at the next bump. State the principle and point at the Dockerfile. (`CHANGELOG.md` is the one exception — it records what changed on a given date, so exact versions belong there.)

Images used **only for testing and tooling** deliberately stay unpinned (`python:alpine`, `python:3-slim`, `mcr.microsoft.com/playwright/python:…`, and the third-party services in the `docker-compose.connector-*.yml` / `docker-compose.kms.yml` / `docker-compose.cluster.yml` overlays). They are never distributed, and floating tags there mean one less thing to maintain. Do not pin them.

The one tooling exception is the documentation image in `docker/docker-compose.docs.yml`: its output (`docs/`) is committed, and a floating tag would rewrite every page of the site whenever a new theme release came out. It is bumped only on request, in a commit of its own that rebuilds the whole site and changes no content.

### Upgrading a pinned base image

Bumping a base image tag is a deliberate, reviewed step, never a drive-by edit, because **the in-Dockerfile package pins are relative to a specific base image version** and a newer base can already ship the fixed version — or a newer one, which makes the pin dead weight (or, with `=`, a build failure).

Those package pins exist to remediate a specific advisory the base image had not picked up yet. Each one sits next to a comment naming the package, the advisories that forced it, and the version the base ships — find them with:

```bash
grep -rn "apk add\|apt-get install" docker/Dockerfile console/Dockerfile \
    docker/webhook-receiver/Dockerfile docker/grpc-receiver/Dockerfile
```

As of writing, the console pins `libcrypto3`/`libssl3` (openssl) and the two connector-test receivers pin `libuuid` (util-linux). Read the comments for the current constraints rather than trusting this paragraph.

Procedure:

1. **Find the new tag.** Query the registry, don't guess — for Docker Hub library images:
   ```bash
   curl -s "https://hub.docker.com/v2/repositories/library/alpine/tags/?page_size=100&ordering=last_updated" \
     | jq -r '.results[] | "\(.name)\t\(.last_updated)"'
   ```
   Pick the exact version that the floating tag currently resolves to, and confirm the pin is digest-identical to it before committing:
   ```bash
   docker manifest inspect alpine:latest     | jq -r '.manifests[0].digest'
   docker manifest inspect alpine:<new-tag>  | jq -r '.manifests[0].digest'   # must match
   ```
   For Debian prefer the **numeric** point release (`<major>.<minor>-slim`) over `stable-slim`: same rootfs, but the numeric tag also pins the apt suite to the codename, so `apt-get install` cannot silently cross a Debian major release. For Rust keep the Alpine suffix (`-alpine<major>.<minor>`) — it fixes the apk repository branch, not just the compiler.

2. **Revise every package pin for that image.** For each pin the `grep` above turns up, check what the *new* base already ships and drop or raise the pin accordingly:
   ```bash
   docker run --rm alpine:<new-tag> sh -c 'apk update >/dev/null && apk policy libcrypto3 libssl3'
   docker run --rm debian:<new-tag> sh -c 'apt-get update >/dev/null 2>&1 && apt-cache policy ca-certificates curl'
   ```
   If the base's own version already satisfies the advisory, **remove the pin** and delete its comment block — a stale `>=` constraint that the base has overtaken is misleading, and the CVE list in the comment stops being true. If it does not, keep the pin and update the comment to name the new base's version. Never replace a pin with a blanket `apk upgrade` / `apt upgrade`: that makes the build irreproducible.

3. **Re-check the Rust side** when bumping the builder: a new compiler can unblock crate versions that MSRV pins in `Cargo.toml` were working around (see the `time`/TD-017 bullet under Build constraints in `AGENTS.md`), and Alpine branch changes can move the C dependencies `ring`/`rdkafka` build against.

4. **Rebuild and verify.** `bin/build && bin/build --console` (the production images — `--dev` builds the Debian image, which never ships), then `bin/test unit` and `bin/test integration`, then scan: `trivy image` must report **0 findings** for `ghcr.io/dxc-technology/arca:production`, `ghcr.io/dxc-technology/arca-console:production`, `arca-webhook-receiver` and `arca-grpc-receiver`. An image scan of `arca` checks next to nothing — scratch has no package database and the Rust binary is not built with `cargo auditable` — so also run `trivy fs --scanners vuln Cargo.lock`, which the publish pipeline enforces too (`image_verify` in `bin/lib/images.sh`). Scan the old and the new image with the *same* tool before claiming an improvement — counts from different scanners are not comparable.

5. **Record it in `CHANGELOG.md`** — and nowhere else. If the bump changed a package pin or the Rust version, say so there. No other file should need editing; if one does, it was carrying a version it shouldn't have. Then `bin/docs-build` if you touched the changelog (the docs site symlinks it).

## Published Container Images

Only the two production images are published, to the GitHub Container Registry, as multi-arch (`linux/amd64` + `linux/arm64`) images: `ghcr.io/dxc-technology/arca` and `ghcr.io/dxc-technology/arca-console`. Test and tooling images (`arca-test`, `arca-unit-test`, the receivers, …) keep their bare local names and are never pushed. Release tags, the publish workflow and the release binaries are described in step 11 of `RELEASING.md`.

- **Local builds** are tagged after their build target — `ghcr.io/dxc-technology/arca:production` / `:development` (the compose `image:` interpolates `BUILD_TARGET`) and `arca-console:production` — so a working-tree build never carries a release tag or shadows `latest`.
- **Pipeline logic**: `.github/workflows/publish-images.yml` and `bin/build --push` are thin wrappers around `bin/lib/images.sh`, which holds all the logic (and is unit-tested by `bin/test scripts`) — change the pipeline there, not in the workflow.
- **Metadata**: the static OCI labels (`source`, `description`, `licenses`, …) live in the `LABEL` of each Dockerfile; `source` is what links a GHCR package to this repository. `version` and `revision` are added at publish time, and all labels are copied to index-level annotations, which is where GHCR reads a multi-arch image's description from.

## Console documentation and screenshots

The console user manual lives at `documentation/docs/guide/console.md`. Screenshots are automated via Playwright in Docker. When the console gains new features, update both the screenshots and the documentation:

1. **Update `docker/screenshots/take_screenshots.py`** — add new screenshot captures (seed data in Phase A, capture in Phase B). Each screenshot is a numbered step. The script runs inside a container that shares network with a dedicated `screenshots-console` service (required for Web Crypto API / localhost).
2. **Update `documentation/docs/guide/console.md`** — add/update sections and screenshot references.
3. **Run `bin/screenshots --build`** — rebuilds all images (arca, console, screenshots), starts Arca with encryption enabled (for encryption indicators in screenshots), seeds sample data, and captures all screenshots to `documentation/docs/assets/screenshots/`. Pass `--no-encryption` to capture without encryption indicators.
4. **Run `bin/docs-build`** — rebuilds the MkDocs site so `docs/` has the new screenshots and HTML.
5. Commit changes in `bin/`, `docker/screenshots/`, `documentation/`, and `docs/`.
