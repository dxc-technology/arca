FROM python:3.12-alpine
WORKDIR /app
COPY server.py .

RUN adduser -D -H receiver && chown -R receiver:receiver /app

EXPOSE 8765

HEALTHCHECK --interval=2s --timeout=5s --start-period=5s --retries=10 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8765/health', timeout=3)" || exit 1

USER receiver

CMD ["python", "server.py"]
