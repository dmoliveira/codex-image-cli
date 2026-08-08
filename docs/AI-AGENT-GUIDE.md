# AI agent guide 🤖

`codex-image` is designed to be called by agents without a terminal prompt, browser, or human confirmation.

## Discover first

```bash
codex-image ai-help --json
codex-image doctor --json
```

`doctor` checks the local Codex executable/login status and API-key presence without generating an image. Its login/key results are non-authoritative and do not verify entitlement, billing, or model access.

## Safe execution recipe

1. Create a project-owned, non-symlinked directory such as `artifacts/design`.
2. Choose a safe output stem: ASCII letters/digits, then letters/digits/`_`/`-`, 1–80 characters.
3. Run `generate --dry-run --json` and parse its one JSON object. The default provider is the direct Image API; use `--provider codex` only when deliberately selecting the local subscription path.
4. Invoke `generate --json` once, or use the explicit Batch lifecycle for asynchronous work.
5. Parse `ok`, `status`, `exit_code`, `outputs`, `retained_artifacts`, and `possibly_modified_paths`.
6. Never auto-retry codes 5–7. Surface the API request ID and path list to the caller instead.

For typed callers, prefer a version 1 `--request-file` JSON request. Keep output naming and security approvals as separate CLI flags.

```bash
mkdir -p artifacts/design

codex-image generate \
  --provider api \
  --prompt "Minimal editorial hero illustration: a robot gardener tending a rust-colored flower" \
  --output-dir artifacts/design \
  --prefix robot-garden \
  --n 1 \
  --format png \
  --quality medium \
  --dry-run \
  --json

codex-image generate \
  --provider api \
  --prompt "Minimal editorial hero illustration: a robot gardener tending a rust-colored flower" \
  --output-dir artifacts/design \
  --prefix robot-garden \
  --n 1 \
  --format png \
  --quality medium \
  --json
```

For asynchronous API work, persist the job file and never resubmit after an unknown POST outcome:

```bash
codex-image batch submit \
  --provider api \
  --prompt "Minimal editorial hero illustration: a robot gardener tending a rust-colored flower" \
  --output-dir artifacts/design \
  --prefix robot-garden \
  --n 2 \
  --job-file artifacts/design/robot-garden-job.json \
  --json

codex-image batch retrieve \
  --job-file artifacts/design/robot-garden-job.json \
  --wait \
  --max-wait-seconds 300 \
  --json
```

## Contract for tool authors

- **No interaction:** do not use `stdin`; `--prompt-file -` is deliberately rejected to prevent blocking.
- **No credential argument:** keys belong in the child process environment only.
- **No hidden retry:** one command makes at most one image-generation operation.
- **Batch recovery:** `batch submit` persists a job file; use `batch status`, `batch retrieve`, or `batch cancel` instead of resubmitting an unknown POST.
- **Explicit reconciliation:** use `batch recover` with a manually verified `--input-file-id` or `--batch-id` for unknown POST outcomes. It never retries the unknown POST, and may issue one new creation POST only from the confirmed `input_uploaded` state.
- **Endpoint trust:** repeat exact custom-origin or loopback approval flags on each Batch operation; never treat an editable job file as credential-destination approval.
- **No unsafe cleanup:** synchronous `generate` reserves private stages before its billable POST, so a known non-overwrite collision yields code 3 and no request. Batch output collisions are checked during retrieval, after remote Batch work may already have been billed; atomic no-clobber publication still protects competing writers.
- **No silent partial success:** every returned image must decode and match the requested PNG/JPEG/WebP container before publishing. Multi-file publishing cannot be atomically visible as a set, so an error reports possibly modified/retained paths instead of claiming success or deleting a concurrent replacement.
- **Provider choice:** the default selects API billing through `OPENAI_API_KEY`; `--provider codex` explicitly selects the authenticated Codex CLI subscription.
- **Quality gate:** `low` is the default; `--quality high` requires `--confirm-high-quality` after reviewing the cost warning.

See [API contract](API-CONTRACT.md) for schemas and exit-code semantics.
