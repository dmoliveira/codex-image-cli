#!/usr/bin/env python3
"""A bounded local Batch API stand-in for offline tmux certification.

The server records request shape and authorization presence only. It returns a
zero-count validating Batch first so the client exercises the documented
initial response state before reading a completed result.
"""

from __future__ import annotations

import argparse
import json
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PNG_BASE64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/"
    "ScLxOQAAAABJRU5ErkJggg=="
)


class BatchHandler(BaseHTTPRequestHandler):
    request_count = 0
    max_requests = 4
    log_path: Path
    custom_ids: list[str] = []
    quality = "unknown"
    size = "unknown"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_POST(self) -> None:  # noqa: N802 - required stdlib handler name
        body = self._read_body()
        BatchHandler.request_count += 1
        if self.path == "/v1/files":
            self._record_input(body)
            self._send_json(200, {"id": "file-input", "purpose": "batch"})
        elif self.path == "/v1/batches":
            self._record_event("batch_create")
            self._send_json(
                200,
                {
                    "id": "batch-local",
                    "status": "validating",
                    "input_file_id": "file-input",
                    "endpoint": "/v1/images/generations",
                    "completion_window": "24h",
                    "request_counts": {"completed": 0, "failed": 0, "total": 0},
                },
            )
        else:
            self._send_json(404, {"error": {"code": "not_found"}})
        self._maybe_shutdown()

    def do_GET(self) -> None:  # noqa: N802 - required stdlib handler name
        BatchHandler.request_count += 1
        if self.path == "/v1/batches/batch-local":
            self._record_event("batch_status")
            self._send_json(
                200,
                {
                    "id": "batch-local",
                    "status": "completed",
                    "input_file_id": "file-input",
                    "endpoint": "/v1/images/generations",
                    "completion_window": "24h",
                    "output_file_id": "file-output",
                    "request_counts": {"completed": 1, "failed": 0, "total": 1},
                },
            )
        elif self.path == "/v1/files/file-output/content":
            self._record_event("batch_output")
            body = "\n".join(
                json.dumps(
                    {
                        "custom_id": custom_id,
                        "response": {
                            "status_code": 200,
                            "body": {"data": [{"b64_json": PNG_BASE64}]},
                        },
                    }
                )
                for custom_id in BatchHandler.custom_ids
            ).encode()
            self._send_bytes(200, "application/jsonl", body)
        else:
            self._send_json(404, {"error": {"code": "not_found"}})
        self._maybe_shutdown()

    def _read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length)

    def _record_input(self, body: bytes) -> None:
        text = body.decode("utf-8", errors="replace")
        BatchHandler.custom_ids = re.findall(r'"custom_id"\s*:\s*"([^"]+)"', text)
        qualities = re.findall(r'"quality"\s*:\s*"([^"]+)"', text)
        BatchHandler.quality = qualities[0] if qualities else "missing"
        sizes = re.findall(r'"size"\s*:\s*"([^"]+)"', text)
        BatchHandler.size = sizes[0] if sizes else "missing"
        self._record_event("file_upload")

    def _record_event(self, operation: str) -> None:
        event = {
            "operation": operation,
            "path": self.path,
            "method": self.command,
            "authorization_present": bool(self.headers.get("Authorization")),
            "request_count": BatchHandler.request_count,
            "quality": BatchHandler.quality,
            "size": BatchHandler.size,
        }
        with BatchHandler.log_path.open("a", encoding="utf-8") as log:
            log.write(json.dumps(event) + "\n")

    def _send_json(self, status: int, payload: object) -> None:
        self._send_bytes(status, "application/json", json.dumps(payload).encode())

    def _send_bytes(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.send_header("X-Request-ID", "local-batch-request")
        self.end_headers()
        self.wfile.write(body)

    def _maybe_shutdown(self) -> None:
        if BatchHandler.request_count >= BatchHandler.max_requests:
            threading.Thread(target=self.server.shutdown, daemon=True).start()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--max-requests", type=int, default=4)
    parser.add_argument("--ready-file", required=True, type=Path)
    parser.add_argument("--log-file", required=True, type=Path)
    args = parser.parse_args()
    if args.max_requests < 1:
        raise SystemExit("--max-requests must be positive")

    BatchHandler.max_requests = args.max_requests
    BatchHandler.log_path = args.log_file
    server = ThreadingHTTPServer((args.host, args.port), BatchHandler)
    args.ready_file.write_text(
        json.dumps({"host": args.host, "port": server.server_address[1]}),
        encoding="utf-8",
    )
    server.serve_forever()
    server.server_close()


if __name__ == "__main__":
    main()
