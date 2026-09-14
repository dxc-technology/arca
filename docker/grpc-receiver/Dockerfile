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
# this image to 820 MB. pip itself is removed once the requirements are
# installed: nothing needs it at runtime, and its vendored copies of
# msgpack/setuptools are perpetually behind their own advisories.
FROM python:alpine

# Point upgrade only, never a blanket `apk upgrade` (irreproducible builds).
# CPython's _uuid extension links libuuid, and the base image still ships
# 2.42.1-r0 -- CVE-2026-53612, -53613, -53614, -76642, -78408, -78409, -78410,
# -27456. `>=` rather than `=` because Alpine drops superseded versions from
# the repo index, turning an exact pin into a build failure later on.
RUN apk add --no-cache 'libuuid>=2.42.3-r1'

WORKDIR /app

COPY requirements.txt ./
RUN pip install --no-cache-dir --only-binary=:all: -r requirements.txt && \
    rm -rf /usr/local/lib/python*/site-packages/pip \
           /usr/local/lib/python*/site-packages/pip-*.dist-info \
           /usr/local/lib/python*/ensurepip /usr/local/bin/pip*

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
