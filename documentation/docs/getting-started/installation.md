# Installation

## Prerequisites

- **Docker** and **Docker Compose** (v2)
- **aws-cli** or **MinIO Client (mc)** (optional, for S3 CLI operations)
- **Git**

## Pre-built Images

Every release from v0.31.0 on is published to the GitHub Container Registry as a
multi-arch image for `linux/amd64` and `linux/arm64`, so the same tag runs on x86 servers
and on Apple Silicon or ARM hosts:

| Image | Content |
|-------|---------|
| `ghcr.io/dxc-technology/arca` | Arca server — the scratch-based production image |
| `ghcr.io/dxc-technology/arca-console` | Web console |

Both images are **public**: they can be pulled without an account or a login, so
there is no registry credential or image pull secret to configure anywhere.

Each release is published under these tags:

| Tag | Points to |
|-----|-----------|
| `X.Y.Z` | Exactly that release, never moves |
| `X.Y` | The newest patch release of the `X.Y` line |
| `latest` | The newest release |

### Choosing a tag

Pick how much change you accept without being asked:

| You want | Use | Trade-off |
|----------|-----|-----------|
| The newest release, bleeding edge | `latest` | Every pull may bring a new minor release, which at `0.x` can change behaviour or configuration. Fine for evaluation, development and the examples in this documentation |
| Patch and fix updates automatically | `X.Y` (e.g. `0.31`) | Security and bug fixes only, no new features; read the changelog before moving to the next `X.Y` |
| Every update, even minor ones, from 1.0 on | `X` (e.g. `1`) | New features without breaking changes (not published while Arca is at `0.x`) |
| Maximum stability, nothing changes unless you change it | `X.Y.Z` | Business-critical production: you upgrade deliberately, after reading the changelog and testing |

A moving tag only changes when the image is pulled again: restart the container
after `docker pull` (Compose: `docker compose pull && docker compose up -d`; Kubernetes
with `latest` pulls on every pod start). Run the console at the same version as the server.

The examples in this documentation and the manifests in `deploy/` use `latest`.

With the repository checked out, the same images run without building anything:

```bash
bin/arca start -d --release latest      # or 0.31, 0.31.2, ...
bin/console start -d                    # follows the server's release
bin/cluster up -d --release latest      # the 3-node HA cluster
```

`bin/arca start`, `bin/console start` and `bin/cluster up` do not rebuild: they run the
image you built before with `--build`, or, if there is none, pull and run the published
`latest` (the development image, `--dev`, is never published, so it is built). `--release` pulls the published image (no
login) and cannot be combined with `--build` or `--dev`; `bin/arca` remembers the
release until its next start (and the console follows it), `bin/cluster` does not.
The compose files and the configuration still come from your checkout, so prefer a
release close to it: a setting newer than the release is silently ignored by it.

There is no major-only tag while Arca is at `0.x`, where a minor release may
break compatibility. A prerelease (`X.Y.Z-rc.N`) gets its exact tag only.

Without the repository, plain Docker runs the server:

```bash
docker run -d --name arca -p 9000:9000 -v arca-data:/data \
    --ulimit nofile=65536:65536 \
    ghcr.io/dxc-technology/arca:latest
docker logs arca | grep "Access Key"
```

`--ulimit` raises the open-files limit, which Docker often leaves at 1024:
every concurrent request holds file descriptors, so a busy server needs more
(see [Troubleshooting](../operations/troubleshooting.md#too-many-open-files-under-load)).
In a Compose file, set `ulimits: nofile:` on the service.

The console runs the same way and needs no volume:

```bash
docker run -d --name arca-console -p 9080:8080 \
    -e ARCA_ENDPOINT=http://localhost:9000 \
    ghcr.io/dxc-technology/arca-console:latest
```

The console listens on 8080 (HTTP) and 8443 (HTTPS) inside the container, not on
80/443, and runs as the unprivileged user `10001:10001`; `-p 9080:8080` publishes
it on host port 9080, as `bin/console` does. `ARCA_ENDPOINT` is the Arca URL the
**browser** connects to (it must be reachable from the browser, not from the
container); leave it out to type the endpoint at the login screen. To serve HTTPS,
mount a directory with the certificate and key PEM files at `/etc/nginx/certs` and
publish port 8443; the private key is group-readable by Arca's service group, so
add `--group-add 65532`:

```bash
docker run -d --name arca-console -p 9443:8443 --group-add 65532 \
    -v "$PWD/certs:/etc/nginx/certs:ro" \
    ghcr.io/dxc-technology/arca-console:latest
```

Stop either with `docker stop arca` or `docker stop arca-console`.

The images carry standard OCI labels (`org.opencontainers.image.version`,
`.revision`, `.source`, …), so `docker inspect` tells which release and commit
a running container was built from.

## Clone and Build

```bash
git clone https://github.com/dxc-technology/arca.git
cd arca
bin/build
```

This builds the production Docker image — a minimal scratch-based container containing only the statically-linked `arca` binary and the default configuration file.

### Development Image

For debugging, build the development image which includes a shell (debian-slim based):

```bash
bin/build --dev
```

Or start the development image directly:

```bash
bin/arca start -d --build --dev
```

### Console Image

Build the web console image separately:

```bash
bin/build --console
```

Or build and start it directly:

```bash
bin/console start -d --build
```

## Verify

Start the server:

```bash
bin/arca start -d --build
```

!!! tip
    Drop the `-d` flag to run in the foreground and see logs in real time. Press ++ctrl+c++ to stop the server.

Check that it's running:

```bash
bin/arca logs | grep "Access Key"
```

Arca auto-generates a root credential on first startup and prints it to the logs. Set the credentials and verify with your S3 client of choice:

=== "aws-cli"

    ```bash
    export AWS_ACCESS_KEY_ID=<your-access-key>
    export AWS_SECRET_ACCESS_KEY=<your-secret-key>

    aws s3 ls --endpoint-url http://localhost:9000
    ```

=== "MinIO Client (mc)"

    ```bash
    mc alias set arca http://localhost:9000 <your-access-key> <your-secret-key>

    mc ls arca
    ```

## Standalone Binary

From v0.31.1 on, every [GitHub release](https://github.com/dxc-technology/arca/releases)
carries the server binary for Linux, `arca-X.Y.Z-linux-amd64.tar.gz` and
`arca-X.Y.Z-linux-arm64.tar.gz`, with their `SHA256SUMS`. It is the static (musl) binary of
the release image, built and scanned by the release pipeline, so it runs on any Linux of that
architecture with no dependency. Each archive also contains the licence (`LICENSE-AGPL-3.0`),
`NOTICE` and `THIRD-PARTY-NOTICES.md`.

```bash
v=0.32.1; arch=amd64        # or arm64
base=https://github.com/dxc-technology/arca/releases/download/v$v
curl -fLO "$base/arca-$v-linux-$arch.tar.gz" -fLO "$base/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf "arca-$v-linux-$arch.tar.gz"
./arca-$v-linux-$arch/arca --version
./arca-$v-linux-$arch/arca serve --config-path /etc/arca/config.toml
```

Start from [`config/default.toml`](https://github.com/dxc-technology/arca/blob/main/config/default.toml)
and see [Configuration](../guide/configuration.md). The notification connectors that deliver
over HTTPS (webhook, Elasticsearch) use the system CA bundle: without one the server still
starts, and those connectors report `HTTP client unavailable` until a bundle is installed.

To build the binary yourself instead, extract it from a local build (without Docker on the
target server):

```bash
# Build for host architecture
bin/build --binary

# Cross-compile for a specific architecture
bin/build --binary --arch amd64
bin/build --binary --arch arm64
```

The binary is written to `build/arca-<arch>` (e.g., `build/arca-arm64`). These are Linux ELF binaries, they cannot run on macOS directly.

## Docker Images

| Build Target | Base | Use Case |
|-------------|------|----------|
| `production` (default) | `scratch` | Production deployments — minimal attack surface |
| `development` | Debian slim | Debugging — includes shell, coreutils |

Both are produced by an Alpine-based Rust builder stage. Every image Arca
distributes — these two and the console — pins its base image to an exact
version tag, so rebuilding months later yields the same toolchain and the same
runtime packages. Auxiliary test and tooling images are intentionally left on
floating tags. The exact tags in force are the ones in `docker/Dockerfile` and
`console/Dockerfile`.

Local builds are tagged after their build target —
`ghcr.io/dxc-technology/arca:production`, `ghcr.io/dxc-technology/arca:development`
and `ghcr.io/dxc-technology/arca-console:production` — never with a release tag,
so a build of your working tree can neither pass for a release nor shadow the
published `latest`.

Switch between them using `BUILD_TARGET`:

```bash
# Production (default)
bin/arca start -d --build

# Development
bin/arca start -d --build --dev
```

## Running Tests

```bash
# All tests (unit + integration)
bin/test

# Unit tests only
bin/test unit

# Integration tests only (server must be running)
bin/test integration

# Ceph s3-tests compatibility suite (stops your server and wipes its data volume)
bin/s3-tests

# Tests of the s3-tests report generator (no server needed)
bin/test s3-report
```

The [Ceph s3-tests](https://github.com/ceph/s3-tests) suite runs the industry-standard S3 compatibility tests against a fresh Arca instance with encryption enabled: `bin/s3-tests` stops the running server, deletes its data volume for a clean state and leaves the server stopped at the end. Results are saved to `s3-tests/results.xml`, an HTML report to `s3-tests/report.html`, and a machine-readable summary to `s3-tests/summary.json`. Ceph/RGW-only extensions are excluded from the score, and a test where Ceph expects RGW's answer while Arca deliberately gives AWS's counts as passed only when it fails on exactly that documented assertion (listed under "AWS Divergences" in the report).

## Next Steps

- [Quick Start](quick-start.md) — create buckets and upload objects
- [Configuration](../guide/configuration.md) — customize server settings
