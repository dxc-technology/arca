# Plain Alpine + the `nginx` package, NOT the `nginx:alpine` image: that image
# ships the image-filter/xslt/geoip/njs/acme modules, which the console never
# loads, and they drag in libgd → libxpm → libxt → libsm → libuuid plus
# tiff, freetype, libx11 and libavif. All dead weight on a static file server,
# and a recurring CVE surface (util-linux advisories reached us this way).
# 18 packages instead of 70, ~18 MB instead of ~65 MB.
FROM alpine

# Run as the nginx package's unprivileged `nginx` user instead of root.
# Binding ports 80/443 as non-root needs CAP_NET_BIND_SERVICE on the binary
# itself (Docker grants that capability to containers by default).
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
# `>=` rather than `=`: Alpine drops superseded versions from the repo index,
# so an exact pin turns into a hard build failure the day a newer build lands.
RUN apk add --no-cache nginx libcap \
        'libcrypto3>=3.5.8-r0' 'libssl3>=3.5.8-r0' && \
    setcap 'cap_net_bind_service=+ep' /usr/sbin/nginx && \
    apk del libcap && \
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
                         /var/cache/nginx /var/lib/nginx

EXPOSE 80 443

# In TLS mode the entrypoint replaces the server config with an HTTPS-only
# one, so port 80 stops answering: probe both schemes. 127.0.0.1, not
# localhost — busybox wget resolves that to ::1 first and nginx listens on
# IPv4 only, so the probe would always be refused. --no-check-certificate
# skips validation of self-signed dev certs (this is a liveness probe, not an
# integrity check).
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD wget -q -O- http://127.0.0.1:80/ >/dev/null 2>&1 || \
        wget -q -O- --no-check-certificate https://127.0.0.1:443/ >/dev/null 2>&1 || exit 1

USER nginx

ENTRYPOINT ["/docker-entrypoint.sh"]
