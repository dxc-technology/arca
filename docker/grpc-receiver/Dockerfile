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

EXPOSE 50051 8080

CMD ["python", "-u", "server.py"]
