# Stage 1: Build
FROM rust:1.85-bookworm AS builder

WORKDIR /build

# Copy workspace manifest and lockfile for dependency caching
COPY Cargo.toml Cargo.lock* ./
COPY crates/arca-core/Cargo.toml crates/arca-core/Cargo.toml
COPY crates/arca-auth/Cargo.toml crates/arca-auth/Cargo.toml
COPY crates/arca-storage/Cargo.toml crates/arca-storage/Cargo.toml
COPY crates/arca-proto/Cargo.toml crates/arca-proto/Cargo.toml
COPY crates/arca-server/Cargo.toml crates/arca-server/Cargo.toml

# Create dummy source files so cargo can resolve the dependency graph
RUN mkdir -p crates/arca-core/src && echo "" > crates/arca-core/src/lib.rs && \
    mkdir -p crates/arca-auth/src && echo "" > crates/arca-auth/src/lib.rs && \
    mkdir -p crates/arca-storage/src && echo "" > crates/arca-storage/src/lib.rs && \
    mkdir -p crates/arca-proto/src && echo "" > crates/arca-proto/src/lib.rs && \
    mkdir -p crates/arca-server/src && echo "fn main() {}" > crates/arca-server/src/main.rs

# Build dependencies only (cached layer)
RUN cargo build --release --workspace 2>/dev/null || true

# Copy real source code
COPY crates/ crates/
COPY config/ config/

# Touch source files to invalidate the cached build for our crates only
RUN touch crates/*/src/*.rs

# Build the actual binary
RUN cargo build --release --workspace

# Stage 2: Development (for running tests)
FROM builder AS dev

ENTRYPOINT ["cargo"]
CMD ["test", "--workspace"]

# Stage 3: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/arca /usr/local/bin/arca
COPY config/default.toml /etc/arca/default.toml

RUN mkdir -p /data

EXPOSE 9000

ENTRYPOINT ["arca"]
CMD ["serve", "--config-path", "/etc/arca/default.toml"]
