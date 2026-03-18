# Installation

## Prerequisites

- **Docker** and **Docker Compose** (v2)
- **aws-cli** or **MinIO Client (mc)** (optional, for S3 CLI operations)
- **Git**

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
| `development` | `debian:stable-slim` | Debugging — includes shell, coreutils |

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

# Ceph s3-tests compatibility suite (server must be running)
bin/s3-tests
```

The [Ceph s3-tests](https://github.com/ceph/s3-tests) suite runs 829 industry-standard S3 compatibility tests. Results are saved to `s3-tests/results.xml`, an HTML report to `s3-tests/report.html`, and a machine-readable summary to `s3-tests/summary.json`.

## Next Steps

- [Quick Start](quick-start.md) — create buckets and upload objects
- [Configuration](../guide/configuration.md) — customize server settings
