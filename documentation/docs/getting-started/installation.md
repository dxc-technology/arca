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

Or start the development image directly:

```bash
bin/arca start -d --build --dev
```

## Verify

Start the server:

```bash
bin/arca start -d --build
```

Check that it's running:

```bash
bin/arca logs | grep "Access Key"
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
```

## Next Steps

- [Quick Start](quick-start.md) — create buckets and upload objects
- [Configuration](../guide/configuration.md) — customize server settings
