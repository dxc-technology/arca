# Stage 1: Build (Alpine for static musl linking — binary runs in scratch)
FROM rust:alpine AS builder

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

# Copy real source code, then mark it newer than the dummy-build artifacts.
# The touch is REQUIRED for correctness, not a cache hack: COPY preserves the
# context's mtimes, which on a cold build (fresh cache) are OLDER than the
# dummy layer's fingerprints just built above — cargo would consider the real
# sources unchanged and link the empty dummy crates (hundreds of
# "unresolved import" errors). On warm builds it costs nothing: an unchanged
# context makes COPY a cache hit, so the touch and build layers are cache
# hits too; a changed context rebuilds only the workspace crates, never the
# dependency layer.
COPY crates/ crates/
RUN find crates -name '*.rs' -exec touch {} +

# Git commit hash, passed from bin/build, baked into the binary via env!()
ARG ARCA_GIT_COMMIT=unknown
ENV ARCA_GIT_COMMIT=${ARCA_GIT_COMMIT}

# Build the actual binary
RUN cargo build --release --workspace

# scratch has no shell/mkdir/chown, so the /data mount point is created and
# owned by the non-root runtime UID here, in the builder, then copied in.
# This lets Docker seed a freshly-created named volume at /data with matching
# ownership, keeping it writable by that user.
RUN mkdir -p /data-seed && chown 65532:65532 /data-seed

# Stage 2: Development tools (for running unit tests)
FROM builder AS dev

ENTRYPOINT ["cargo"]
CMD ["test", "--workspace"]

# Stage 3: Production runtime (minimal — no OS, no shell, no tools)
FROM scratch AS production

COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /build/target/release/arca /usr/local/bin/arca
COPY config/default.toml /etc/arca/config.toml
COPY --from=builder --chown=65532:65532 /data-seed /data

EXPOSE 9000

# No HEALTHCHECK: scratch ships no shell/curl/wget to run one, and adding a
# static binary just for this is not worth the extra attack surface for a
# minimal hardening item — orchestrated deployments use the Kubernetes
# manifests' own probes against GET /admin/health instead.

USER 65532:65532

ENTRYPOINT ["/usr/local/bin/arca"]
CMD ["serve"]

# Stage 4: Development runtime (with shell for debugging)
FROM debian:stable-slim AS development

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 65532 arca \
    && useradd --uid 65532 --gid 65532 --no-create-home --shell /usr/sbin/nologin arca

COPY --from=builder /build/target/release/arca /usr/local/bin/arca
COPY config/default.toml /etc/arca/config.toml

RUN mkdir -p /data && chown 65532:65532 /data

EXPOSE 9000

# TLS mode serves the same port as HTTPS-only, so try both schemes; -k skips
# verification of self-signed dev certs (this is a liveness probe, not an
# integrity check).
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD curl -fsS http://localhost:9000/admin/health || curl -fsSk https://localhost:9000/admin/health || exit 1

USER 65532:65532

ENTRYPOINT ["arca"]
CMD ["serve"]
