# Stage 1: Build (Alpine for static musl linking — binary runs in scratch)
FROM rust:1.85-alpine AS builder

RUN apk add --no-cache musl-dev perl cmake make g++ curl-dev linux-headers zlib-dev zlib-static protobuf-dev protoc

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

# Pin transitive dependencies that exceed our Rust version (1.85).
# home 0.5.12 requires Rust 1.88; pin to 0.5.11.
RUN cargo update -p home --precise 0.5.11 2>/dev/null || true
# serde_with 3.13+ requires Rust 1.88 (pulled by mongodb); pin to 3.12.0.
RUN cargo update -p serde_with --precise 3.12.0 2>/dev/null || true
# darling 0.21+ requires Rust 1.88 (pulled by serde_with); pin to 0.20.10.
RUN cargo update -p darling --precise 0.20.10 2>/dev/null || true
RUN cargo update -p darling_core --precise 0.20.10 2>/dev/null || true
RUN cargo update -p darling_macro --precise 0.20.10 2>/dev/null || true

# Copy real source code
COPY crates/ crates/

# Touch source files to invalidate the cached build for our crates only
RUN touch crates/*/src/*.rs

# Git commit hash, passed from bin/build, baked into the binary via env!()
ARG ARCA_GIT_COMMIT=unknown
ENV ARCA_GIT_COMMIT=${ARCA_GIT_COMMIT}

# Build the actual binary
RUN cargo build --release --workspace

# Stage 2: Development tools (for running unit tests)
FROM builder AS dev

ENTRYPOINT ["cargo"]
CMD ["test", "--workspace"]

# Stage 3: Production runtime (minimal — no OS, no shell, no tools)
FROM scratch AS production

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /build/target/release/arca /usr/local/bin/arca
COPY config/default.toml /etc/arca/config.toml

EXPOSE 9000

ENTRYPOINT ["/usr/local/bin/arca"]
CMD ["serve"]

# Stage 4: Development runtime (with shell for debugging)
FROM debian:stable-slim AS development

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/arca /usr/local/bin/arca
COPY config/default.toml /etc/arca/config.toml

RUN mkdir -p /data

EXPOSE 9000

ENTRYPOINT ["arca"]
CMD ["serve"]
