# Test fixture: a ~100-line stdlib HTTP server, so the stock Python Alpine
# image is the right base — but its OS packages are only as fresh as the
# upstream image rebuild, and CPython genuinely links libuuid (_uuid), which
# has a steady stream of util-linux advisories. `apk upgrade` at build time
# picks up patched Alpine packages without waiting for a new base image.
FROM python:alpine

RUN apk upgrade --no-cache

WORKDIR /app
COPY server.py .

RUN adduser -D -H receiver && chown -R receiver:receiver /app

EXPOSE 8765

HEALTHCHECK --interval=2s --timeout=5s --start-period=5s --retries=10 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8765/health', timeout=3)" || exit 1

USER receiver

CMD ["python", "server.py"]
