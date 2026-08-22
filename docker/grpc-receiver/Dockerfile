FROM python:3.12-alpine

RUN apk add --no-cache gcc g++ musl-dev libffi-dev linux-headers

WORKDIR /app

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY arca_notifications.proto ./
RUN python -m grpc_tools.protoc \
        -I. \
        --python_out=. \
        --grpc_python_out=. \
        arca_notifications.proto

COPY server.py ./

RUN adduser --disabled-password --no-create-home --gecos '' receiver && \
    chown -R receiver:receiver /app

EXPOSE 50051 8080

HEALTHCHECK --interval=2s --timeout=5s --start-period=5s --retries=15 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8080/health', timeout=3)" || exit 1

USER receiver

CMD ["python", "-u", "server.py"]
