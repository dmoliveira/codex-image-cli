#!/usr/bin/env bash
# Offline Batch lifecycle certification using a detached tmux fake API.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNS_DIR="$ROOT/target/e2e-batch"
mkdir -p "$RUNS_DIR"
RUN_DIR="$(mktemp -d "$RUNS_DIR/run.XXXXXX")"
READY_FILE="$RUN_DIR/ready.json"
SERVER_LOG="$RUN_DIR/server.jsonl"
SUBMIT_REPORT="$RUN_DIR/submit.json"
STATUS_REPORT="$RUN_DIR/status.json"
RETRIEVE_REPORT="$RUN_DIR/retrieve.json"
OUTPUT_DIR="$RUN_DIR/output"
JOB_FILE="$RUN_DIR/job.json"
SESSION="codex-image-batch-e2e-$$"
SOCKET="codex-image-batch-e2e-$$"

cleanup() {
  tmux -L "$SOCKET" has-session -t "$SESSION" 2>/dev/null && tmux -L "$SOCKET" kill-session -t "$SESSION" || true
  tmux -L "$SOCKET" kill-server 2>/dev/null || true
  rm -rf -- "$RUN_DIR"
}
trap cleanup EXIT INT TERM

mkdir -p "$OUTPUT_DIR"
tmux -L "$SOCKET" new-session -d -s "$SESSION" -c "$ROOT" \
  "exec python3 scripts/mock_openai_batch_api.py --port 0 --max-requests 5 --ready-file '$READY_FILE' --log-file '$SERVER_LOG'"

for _ in $(seq 1 100); do
  if [[ -s "$READY_FILE" ]]; then
    break
  fi
  sleep 0.05
done
[[ -s "$READY_FILE" ]] || {
  echo "mock Batch API did not become ready" >&2
  tmux -L "$SOCKET" capture-pane -pt "$SESSION" >&2 || true
  exit 1
}

port="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["port"])' "$READY_FILE")"
key_env="OPENAI_""API_KEY"

env "$key_env=local-batch-e2e-key" \
  cargo run --quiet -- \
    batch submit \
    --provider api \
    --prompt "offline Batch lifecycle verification image" \
    --output-dir "$OUTPUT_DIR" \
    --name verification \
    --job-file "$JOB_FILE" \
    --api-base-url "http://127.0.0.1:${port}/v1" \
    --allow-insecure-localhost \
    --json \
    < /dev/null \
    > "$SUBMIT_REPORT"

env "$key_env=local-batch-e2e-key" \
  cargo run --quiet -- \
    batch status \
    --job-file "$JOB_FILE" \
    --allow-insecure-localhost \
    --json \
    < /dev/null \
    > "$STATUS_REPORT"

env "$key_env=local-batch-e2e-key" \
  cargo run --quiet -- \
    batch retrieve \
    --job-file "$JOB_FILE" \
    --allow-insecure-localhost \
    --json \
    < /dev/null \
    > "$RETRIEVE_REPORT"

python3 - "$SUBMIT_REPORT" "$STATUS_REPORT" "$RETRIEVE_REPORT" "$JOB_FILE" "$OUTPUT_DIR/verification.png" "$SERVER_LOG" <<'PY'
import json
import pathlib
import sys

submit_path, status_path, retrieve_path, job_path, image_path, log_path = map(
    pathlib.Path, sys.argv[1:]
)
submit = json.loads(submit_path.read_text(encoding="utf-8"))
status = json.loads(status_path.read_text(encoding="utf-8"))
retrieve = json.loads(retrieve_path.read_text(encoding="utf-8"))
job = json.loads(job_path.read_text(encoding="utf-8"))
assert submit["ok"] is True and submit["status"] == "validating", submit
assert status["ok"] is True and status["status"] == "completed", status
assert retrieve["ok"] is True and retrieve["status"] == "retrieved", retrieve
assert image_path.read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
assert "offline Batch lifecycle verification image" not in job

events = [json.loads(line) for line in log_path.read_text(encoding="utf-8").splitlines()]
assert len(events) == 5, events
assert [event["operation"] for event in events] == [
    "file_upload",
    "batch_create",
    "batch_status",
    "batch_status",
    "batch_output",
], events
assert all(event["authorization_present"] for event in events), events
assert events[0]["quality"] == "low", events[0]
print("Batch E2E PASS: submit, zero-count validation, status, retrieval, low quality, closed stdin")
PY
