# Installation

## Prerequisites

- **Docker** and **Docker Compose** (v2)
- **aws-cli** (optional, for S3 CLI operations)
- **Git**

## Clone and Build

```bash
git clone https://github.com/dxc-technology/arca.git
cd arca
bin/build
```

This builds the production Docker image — a minimal scratch-based container (~8.6 MB) containing only the statically-linked `arca` binary and the default configuration file.

### Development Image

For debugging, build the development image which includes a shell (debian-slim based):

```bash
bin/build --dev
```

Or set the `BUILD_TARGET` environment variable:

```bash
BUILD_TARGET=development bin/run --build -d
```

## Verify

Start the server:

```bash
bin/run --build -d
```

Check that it's running:

```bash
docker compose -f docker/docker-compose.yml logs arca | grep "Access Key"
```

Arca auto-generates a root credential on first startup and prints it to the logs. Set the credentials and verify with aws-cli:

```bash
export AWS_ACCESS_KEY_ID=<your-access-key>
export AWS_SECRET_ACCESS_KEY=<your-secret-key>

aws s3 ls --endpoint-url http://localhost:9000
```

## Docker Images

| Build Target | Base | Size | Use Case |
|-------------|------|------|----------|
| `production` (default) | `scratch` | ~8.6 MB | Production deployments — minimal attack surface |
| `development` | `debian:bookworm-slim` | ~80 MB | Debugging — includes shell, coreutils |

Switch between them using `BUILD_TARGET`:

```bash
# Production (default)
bin/run --build -d

# Development
BUILD_TARGET=development bin/run --build -d
```

## Running Tests

```bash
# All tests (unit + integration)
bin/test

# Unit tests only
bin/test unit

# Integration tests only (server must be running)
bin/test integration
```

## Next Steps

- [Quick Start](quick-start.md) — create buckets and upload objects
- [Configuration](../guide/configuration.md) — customize server settings
