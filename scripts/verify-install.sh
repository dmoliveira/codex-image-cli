#!/usr/bin/env bash
# Verify fresh installation and forced update without touching a user's PATH.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_PARENT="$ROOT/target/install-check"
mkdir -p "$INSTALL_PARENT"
INSTALL_ROOT="$(mktemp -d "$INSTALL_PARENT/run.XXXXXX")"

cleanup() {
  rm -rf -- "$INSTALL_ROOT"
}
trap cleanup EXIT INT TERM

CARGO_INSTALL_ROOT="$INSTALL_ROOT" cargo install --path "$ROOT" --locked
(
  cd /
  PATH="$INSTALL_ROOT/bin:$PATH" codex-image ai-help --json | python3 -c '
import json, sys
report = json.load(sys.stdin)
assert report["schema_version"] == 4
assert report["non_interactive"] is True
assert report["command"] == "codex-image generate"
'
)
CARGO_INSTALL_ROOT="$INSTALL_ROOT" cargo install --path "$ROOT" --locked --force
echo "Install/update smoke test: PASS"
