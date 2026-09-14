# Stage 1: generate the protobuf/gRPC stubs. grpcio-tools is build-time only
# (~50 MB of protoc machinery) and has no business in the runtime image.
FROM python:alpine AS stubs

WORKDIR /stubs
COPY arca_notifications.proto ./
RUN pip install --no-cache-dir --only-binary=:all: grpcio-tools && \
    python -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. \
        arca_notifications.proto

# Stage 2: runtime. `--only-binary=:all:` fails the build loudly if a musl
# wheel is ever missing, instead of silently falling back to compiling grpcio
# from source — which is what used to pull gcc/g++/musl-dev in and inflate
# this image to 820 MB. `apk upgrade` keeps the base OS packages patched
# (CPython links libuuid, a recurring util-linux CVE surface) without waiting
# for an upstream base image rebuild.
FROM python:alpine

RUN apk upgrade --no-cache

WORKDIR /app

COPY requirements.txt ./
RUN pip install --no-cache-dir --only-binary=:all: -r requirements.txt

COPY --from=stubs /stubs/arca_notifications_pb2.py \
                  /stubs/arca_notifications_pb2_grpc.py ./
COPY server.py ./

RUN adduser --disabled-password --no-create-home --gecos '' receiver && \
    chown -R receiver:receiver /app

EXPOSE 50051 8080

HEALTHCHECK --interval=2s --timeout=5s --start-period=5s --retries=15 \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://localhost:8080/health', timeout=3)" || exit 1

USER receiver

CMD ["python", "-u", "server.py"]
