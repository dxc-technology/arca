# Plain Alpine + the `nginx` package, NOT the `nginx:alpine` image: that image
# ships the image-filter/xslt/geoip/njs/acme modules, which the console never
# loads, and they drag in libgd → libxpm → libxt → libsm → libuuid plus
# tiff, freetype, libx11 and libavif. All dead weight on a static file server,
# and a recurring CVE surface (util-linux advisories reached us this way).
# 18 packages instead of 70, ~18 MB instead of ~65 MB.
#
# Pinned to an exact Alpine release for reproducible builds. Note that this
# pins the rootfs, not the apk repository index: the release branch keeps
# receiving package updates, so the version constraints below can be satisfied
# by newer builds without changing this tag. Upgrade procedure: see
# "Base Image Pinning and Upgrades" in AGENTS.md.
FROM alpine:3.24.1

# OCI metadata. `source` is what links the package on ghcr.io to this
# repository; GHCR shows `description` on the package page. Version and
# revision are added at publish time by bin/lib/images.sh.
LABEL org.opencontainers.image.title="Arca Console" \
      org.opencontainers.image.description="Web console for Arca, the S3-compatible object storage server" \
      org.opencontainers.image.source="https://github.com/dxc-technology/arca" \
      org.opencontainers.image.url="https://dxc-technology.github.io/arca/guide/console/" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"

# Run as the nginx package's `nginx` account instead of root, renumbered to
# 10001:10001 (the package creates it as 100:101): an id below 10000 can
# collide with a host account, which container scanners flag, and Kubernetes
# needs a numeric id to verify `runAsNonRoot`.
# The ports are unprivileged (8080 for HTTP, 8443 for HTTPS), so the nginx
# binary needs no file capability and the container runs with every capability
# dropped. A `setcap`ped binary would not even start under `drop: ["ALL"]`.
# The entrypoint rewrites http.d/default.conf (TLS mode) and index.html
# (endpoint injection) at startup, so those paths — plus nginx's own temp/
# cache/pid dirs, which default to root-owned /run — must be owned by that
# user.
# Point upgrades only, never a blanket `apk upgrade`: that would pull an
# unknown set of package versions at build time and make the build
# irreproducible. Each constraint here names one package and the advisory that
# forced it, so the diff says what changed and why.
#   libcrypto3/libssl3 >= 3.5.8-r0 -- openssl CVE-2026-14456, -14457, -18798,
#   -54874, -63072, -63073, -63074, -63075, -63076, -75803 (the base image
#   still ships 3.5.7-r0).
#   zlib >= 1.3.2-r1 -- CVE-2026-85091 (the base image ships 1.3.2-r0, and so
#   does alpine 3.24.2).
# `>=` rather than `=`: Alpine drops superseded versions from the repo index,
# so an exact pin turns into a hard build failure the day a newer build lands.
RUN apk add --no-cache nginx \
        'libcrypto3>=3.5.8-r0' 'libssl3>=3.5.8-r0' 'zlib>=1.3.2-r1' && \
    deluser nginx && \
    addgroup -g 10001 nginx && \
    adduser -D -H -h /var/lib/nginx -u 10001 -G nginx -s /sbin/nologin nginx && \
    sed -i 's|^pid\s\+.*|pid /tmp/nginx.pid;|' /etc/nginx/nginx.conf && \
    sed -i '/^user\s\+nginx;/d' /etc/nginx/nginx.conf

# Web server config. Alpine's nginx includes /etc/nginx/http.d/*.conf inside
# the http block; the shipped default.conf there is replaced by ours.
COPY nginx.conf /etc/nginx/http.d/default.conf
COPY nginx-tls.conf /etc/nginx/nginx-tls.conf

# Web console files
COPY index.html /usr/share/nginx/html/
COPY js/ /usr/share/nginx/html/js/
COPY assets/ /usr/share/nginx/html/assets/

COPY docker-entrypoint.sh /docker-entrypoint.sh

RUN chmod +x /docker-entrypoint.sh && \
    mkdir -p /var/cache/nginx && \
    chown -R nginx:nginx /etc/nginx/http.d /usr/share/nginx/html \
                         /var/cache/nginx /var/lib/nginx /var/log/nginx /run/nginx

EXPOSE 8080 8443

# In TLS mode the entrypoint replaces the server config with an HTTPS-only
# one, so port 8080 stops answering: probe both schemes. 127.0.0.1, not
# localhost — busybox wget resolves that to ::1 first and nginx listens on
# IPv4 only, so the probe would always be refused. --no-check-certificate
# skips validation of self-signed dev certs (this is a liveness probe, not an
# integrity check).
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD wget -q -O- http://127.0.0.1:8080/ >/dev/null 2>&1 || \
        wget -q -O- --no-check-certificate https://127.0.0.1:8443/ >/dev/null 2>&1 || exit 1

USER 10001:10001

ENTRYPOINT ["/docker-entrypoint.sh"]
