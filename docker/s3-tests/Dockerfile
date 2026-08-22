FROM python:3.12-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        git build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /s3-tests

# Pin to a specific commit for reproducibility
ARG S3TESTS_COMMIT=06e2c5789a39991eed6008b8f728914fe7c7ee3a
RUN git clone https://github.com/ceph/s3-tests.git . \
    && git checkout ${S3TESTS_COMMIT}

RUN pip install --no-cache-dir -r requirements.txt pytest-timeout

COPY s3tests.conf /s3-tests/s3tests.conf
COPY report.py /s3-tests/report.py

ENV S3TEST_CONF=/s3-tests/s3tests.conf

# Runs as a non-root user by default. report.py writes results into a
# bind-mounted host directory (../s3-tests:/s3-tests/output), so
# docker-compose.yml overrides this UID/GID at runtime (via S3TESTS_UID/
# S3TESTS_GID, set by bin/s3-tests) to match the host user and keep those
# writes working regardless of host filesystem permissions.
RUN useradd --uid 1000 --no-create-home --shell /usr/sbin/nologin tester
USER tester

ENTRYPOINT ["pytest"]
CMD ["s3tests/functional/test_s3.py", "-v", "--tb=short"]
