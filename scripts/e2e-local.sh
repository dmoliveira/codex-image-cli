#!/usr/bin/env bash
# Offline end-to-end certification using a detached tmux fake API. It accepts
# no input and never contacts an external service.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNS_DIR="$ROOT/target/e2e"
mkdir -p "$RUNS_DIR"
RUN_DIR="$(mktemp -d "$RUNS_DIR/run.XXXXXX")"
READY_FILE="$RUN_DIR/ready.json"
SERVER_LOG="$RUN_DIR/server.jsonl"
REPORT_FILE="$RUN_DIR/report.json"
OUTPUT_DIR="$RUN_DIR/output"
SESSION="codex-image-e2e-$$"

cleanup() {
  tmux has-session -t "$SESSION" 2>/dev/null && tmux kill-session -t "$SESSION" || true
  rm -rf -- "$RUN_DIR"
}
trap cleanup EXIT INT TERM

server_command=$(printf 'cd %q && exec python3 %q --port 0 --max-requests 1 --ready-file %q --log-file %q' \
  "$ROOT" "$ROOT/scripts/mock_openai_image_api.py" "$READY_FILE" "$SERVER_LOG")
tmux new-session -d -s "$SESSION" "$server_command"

for _ in $(seq 1 100); do
  if [[ -s "$READY_FILE" ]]; then
    break
  fi
  sleep 0.05
done
[[ -s "$READY_FILE" ]] || { echo "mock API did not become ready" >&2; tmux capture-pane -pt "$SESSION" >&2 || true; exit 1; }

port=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["port"])' "$READY_FILE")
mkdir -p "$OUTPUT_DIR"

key_env="OPENAI_""API_KEY"
env "$key_env=local-e2e-key" \
  cargo run --quiet -- \
    generate \
    --provider api \
    --prompt "offline local E2E verification image" \
    --output-dir "$OUTPUT_DIR" \
    --name verification \
    --api-base-url "http://127.0.0.1:${port}/v1" \
    --allow-insecure-localhost \
    --json \
    < /dev/null \
    > "$REPORT_FILE"

python3 - "$REPORT_FILE" "$OUTPUT_DIR/verification.png" "$SERVER_LOG" <<'PY'
import json
import pathlib
import sys

report_path, image_path, server_log_path = map(pathlib.Path, sys.argv[1:])
report = json.loads(report_path.read_text(encoding="utf-8"))
assert report["ok"] is True, report
assert report["status"] == "success", report
assert report["request"]["attempted"] is True, report
assert report["request"]["request_id"] == "local-e2e-request", report
assert report["outputs"] == [str(image_path)], report
assert image_path.read_bytes().startswith(b"\x89PNG\r\n\x1a\n")

log = server_log_path.read_text(encoding="utf-8")
event = json.loads(log)
assert event == {
    "path": "/v1/images/generations",
    "method": "POST",
    "authorization_present": True,
    "request_count": 1,
    "image_count": 1,
}, event
assert "local-e2e-key" not in log
print("E2E PASS: one local request, valid PNG, JSON contract, closed stdin, no key logged")
PY
