FROM mcr.microsoft.com/playwright/python:v1.49.1-noble

WORKDIR /app

RUN pip install --no-cache-dir boto3 requests playwright==1.49.1

COPY take_screenshots.py .

# Run as the image's unprivileged `pwuser` (uid 1001), which Playwright's own
# images ship for this purpose. The screenshots land in a bind-mounted host
# directory (see docker-compose.yml), so on a Linux host that directory must be
# writable by uid 1001.
RUN chown -R pwuser:pwuser /app

# A short-lived batch job, not a service: there is nothing to probe.
HEALTHCHECK NONE

USER pwuser

ENTRYPOINT ["python", "take_screenshots.py"]
