FROM mcr.microsoft.com/playwright/python:v1.49.1-noble

WORKDIR /app

RUN pip install --no-cache-dir boto3 requests playwright==1.49.1

COPY take_screenshots.py .

ENTRYPOINT ["python", "take_screenshots.py"]
