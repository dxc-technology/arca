# Installation

## Prerequisites

- **Docker** and **Docker Compose** (v2)
- **aws-cli** or **MinIO Client (mc)** (optional, for S3 CLI operations)
- **Git**

## Pre-built Images

Every release after v0.30.2 is published to the GitHub Container Registry as a
multi-arch image for `linux/amd64` and `linux/arm64`, so the same tag runs on x86 servers
and on Apple Silicon or ARM hosts:

| Image | Content |
|-------|---------|
| `ghcr.io/dxc-technology/arca` | Arca server — the scratch-based production image |
| `ghcr.io/dxc-technology/arca-console` | Web console |

Each release is published under three tags:

| Tag | Points to |
|-----|-----------|
| `X.Y.Z` | Exactly that release — use this in production |
| `X.Y` | The newest patch release of the `X.Y` line |
| `latest` | The newest release |

There is no major-only tag while Arca is at `0.x`, where a minor release may
break compatibility. A prerelease (`X.Y.Z-rc.N`) gets its exact tag only.

```bash
docker run -d --name arca -p 9000:9000 -v arca-data:/data \
    ghcr.io/dxc-technology/arca:latest
docker logs arca | grep "Access Key"
```

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

Extract the statically-linked Linux binary for deployment on servers (without Docker):

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
