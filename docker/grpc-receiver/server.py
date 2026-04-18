"""Minimal gRPC receiver for Arca notification connector tests.

Implements `arca.notifications.v1.NotificationService` on TCP port 50051
(h2c). Captured payloads (plus gRPC request metadata) are kept in memory and
surfaced over HTTP on port 8080 for the Python test suite to inspect:

    GET  /messages       -> JSON list of received payloads
    DELETE /messages     -> clear the captured list
    GET  /health         -> 200 OK
"""

import json
import threading
from concurrent import futures
from http.server import BaseHTTPRequestHandler, HTTPServer

import grpc

import arca_notifications_pb2 as pb2
import arca_notifications_pb2_grpc as pb2_grpc


_messages = []
_lock = threading.Lock()


class NotificationServiceServicer(pb2_grpc.NotificationServiceServicer):
    def Notify(self, request, context):
        # Capture the gRPC metadata (e.g. authorization header) for tests
        # that verify Bearer-token forwarding.
        meta = {}
        for k, v in context.invocation_metadata():
            meta[k] = v

        entry = {
            "event_payload": request.event_payload,
            "connector_id": request.connector_id,
            "metadata": dict(request.metadata),
            "grpc_metadata": meta,
        }
        with _lock:
            _messages.append(entry)

        return pb2.NotificationResponse(success=True, message="ok")


class AdminHTTPHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"healthy"}')
        elif self.path == "/messages":
            with _lock:
                body = json.dumps(_messages).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.end_headers()

    def do_DELETE(self):
        if self.path == "/messages":
            with _lock:
                _messages.clear()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"cleared"}')
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, fmt, *args):
        pass  # suppress default stdout noise


def _serve_grpc():
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=8))
    pb2_grpc.add_NotificationServiceServicer_to_server(
        NotificationServiceServicer(), server
    )
    server.add_insecure_port("[::]:50051")
    server.start()
    print("gRPC NotificationService listening on :50051", flush=True)
    server.wait_for_termination()


def _serve_http():
    server = HTTPServer(("0.0.0.0", 8080), AdminHTTPHandler)
    print("Admin HTTP server listening on :8080", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    threading.Thread(target=_serve_http, daemon=True).start()
    _serve_grpc()
