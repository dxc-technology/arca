# Test fixture: a ~100-line stdlib HTTP server, so the stock Python Alpine
# image is the right base.
FROM python:alpine

# Point upgrade only, never a blanket `apk upgrade` (irreproducible builds).
# CPython's _uuid extension links libuuid, and the base image still ships
# 2.42.1-r0 -- CVE-2026-53612, -53613, -53614, -76642, -78408, -78409, -78410,
# -27456. `>=` rather than `=` because Alpine drops superseded versions from
# the repo index, turning an exact pin into a build failure later on.
# pip is unused here (the server is pure stdlib) and vendors outdated copies
# of msgpack/setuptools, so it goes too.
RUN apk add --no-cache 'libuuid>=2.42.3-r1' && \
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
