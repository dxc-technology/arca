"""Minimal webhook receiver for notification integration tests.

Accepts POST on /webhook and stores events in memory.
GET /events returns all captured events as JSON.
DELETE /events clears the event list.
GET /health returns 200.
"""

import json
import threading
from http.server import HTTPServer, BaseHTTPRequestHandler

events = []
events_lock = threading.Lock()


class WebhookHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path == "/webhook":
            content_length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(content_length).decode("utf-8") if content_length > 0 else ""
            try:
                payload = json.loads(body)
            except json.JSONDecodeError:
                payload = body

            with events_lock:
                events.append({
                    "timestamp": self.date_time_string(),
                    "payload": payload,
                })

            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"ok"}')
        else:
            self.send_response(404)
            self.end_headers()

    def do_GET(self):
        if self.path == "/events":
            with events_lock:
                data = json.dumps(events)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(data.encode("utf-8"))
        elif self.path == "/health":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"healthy"}')
        else:
            self.send_response(404)
            self.end_headers()

    def do_DELETE(self):
        if self.path == "/events":
            with events_lock:
                events.clear()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"cleared"}')
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, format, *args):
        # Suppress default logging
        pass


if __name__ == "__main__":
    server = HTTPServer(("0.0.0.0", 8765), WebhookHandler)
    print("Webhook receiver listening on :8765", flush=True)
    server.serve_forever()
