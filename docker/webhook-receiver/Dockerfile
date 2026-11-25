# Test fixture: a ~100-line stdlib HTTP server, so the stock Python Alpine
# image is the right base.
FROM python:alpine

# Point upgrades only, never a blanket `apk upgrade` (irreproducible builds).
# `>=` rather than `=` because Alpine drops superseded versions from the repo
# index, turning an exact pin into a build failure later on.
# zlib >= 1.4-r0 -- CVE-2026-4738 (the base image ships 1.3.2-r1).
# libssl3, libcrypto3 >= 3.5.9-r1 -- OpenSSL CVE-2026-107496, -95691, -95692,
#   -88822, -88823 (the base image ships 3.5.9-r0).
# pip is unused here (the server is pure stdlib) and vendors outdated copies
# of msgpack/setuptools, so it goes too.
RUN apk add --no-cache 'zlib>=1.4-r0' 'libssl3>=3.5.9-r1' 'libcrypto3>=3.5.9-r1' && \
    rm -rf /usr/local/lib/python*/site-packages/pip \
           /usr/local/lib/python*/site-packages/pip-*.dist-info \
           /usr/local/lib/python*/ensurepip /usr/local/bin/pip*

WORKDIR /app
COPY server.py .

RUN adduser -D -H receiver && chown -R receiver:receiver /app

EXPOSE 8765

HEALTHCHECK --interval=2s --timeout=5s --start-period=5s --retries=10 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8765/health', timeout=3)" || exit 1

USER receiver

CMD ["python", "server.py"]
