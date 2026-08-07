#!/usr/bin/env python3
"""A one-purpose local Image API stand-in for offline E2E checks.

It intentionally logs only request shape and authorization *presence*, never
the Authorization value, prompt, or response payload.
"""

from __future__ import annotations

import argparse
import base64
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PNG_BASE64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/"
    "ScLxOQAAAABJRU5ErkJggg=="
)


class ImageHandler(BaseHTTPRequestHandler):
    request_count = 0
    max_requests = 1
    log_path: Path

    def log_message(self, _format: str, *_args: object) -> None:
        # BaseHTTPRequestHandler's default logging can expose paths/headers.
        return

    def do_POST(self) -> None:  # noqa: N802 - required stdlib handler name
        content_length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(content_length)
        try:
            payload = json.loads(body)
        except json.JSONDecodeError:
            payload = {}

        ImageHandler.request_count += 1
        count = payload.get("n", 1)
        if not isinstance(count, int) or not 1 <= count <= 4:
            count = 1
        event = {
            "path": self.path,
            "method": "POST",
            "authorization_present": bool(self.headers.get("Authorization")),
            "request_count": ImageHandler.request_count,
            "image_count": count,
        }
        ImageHandler.log_path.write_text(json.dumps(event) + "\n", encoding="utf-8")

        if self.path != "/v1/images/generations":
            self.send_response(404)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":{"code":"not_found"}}')
            return

        response = json.dumps(
            {"data": [{"b64_json": PNG_BASE64} for _ in range(count)]}
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(response)))
        self.send_header("X-Request-ID", "local-e2e-request")
        self.end_headers()
        self.wfile.write(response)

        if ImageHandler.request_count >= ImageHandler.max_requests:
            threading.Thread(target=self.server.shutdown, daemon=True).start()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--max-requests", type=int, default=1)
    parser.add_argument("--ready-file", required=True, type=Path)
    parser.add_argument("--log-file", required=True, type=Path)
    args = parser.parse_args()

    if args.max_requests < 1:
        raise SystemExit("--max-requests must be positive")
    ImageHandler.max_requests = args.max_requests
    ImageHandler.log_path = args.log_file
    server = ThreadingHTTPServer((args.host, args.port), ImageHandler)
    args.ready_file.write_text(
        json.dumps({"host": args.host, "port": server.server_address[1]}),
        encoding="utf-8",
    )
    server.serve_forever()
    server.server_close()


if __name__ == "__main__":
    main()
