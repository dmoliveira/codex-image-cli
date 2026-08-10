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
5. Parse `ok`, `status`, `exit_code`, `outputs`, `retained_artifacts`, `possibly_modified_paths`, `cost_preview`, and Batch `request_counts` when present.
6. Never auto-retry codes 5–7. Surface the API request ID and path list to the caller instead.

The API provider defaults to a cost-conscious `1024x1024` PNG at `low` quality. Choose `--size auto` or a larger size explicitly when the request needs it.

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
  --quality low \
  --dry-run \
  --json

codex-image generate \
  --provider api \
  --prompt "Minimal editorial hero illustration: a robot gardener tending a rust-colored flower" \
  --output-dir artifacts/design \
  --prefix robot-garden \
  --n 1 \
  --format png \
  --quality low \
  --json
```

For a canonical API request with an exact supported standard size, the dry-run
report contains a non-binding `cost_preview` based on the official GPT Image 2
per-image output table. It is output-only: `scope: "output_only"`,
`total_cost_status: "unknown"`, and `excluded_charges` identify prompt and
input-image charges that are not guessed locally. Treat `status: "unavailable"`
as unknown, not zero; custom origins, Codex, `auto`, and unsupported
sizes/qualities are intentionally unpriced. Dry-run also validates endpoint
policy and output collisions without creating files or reading a key; high
quality can be planned without confirmation, while billable high-quality runs
still require `--confirm-high-quality`.

For asynchronous API work, persist the job file and never resubmit after an unknown POST outcome:

```bash
codex-image batch submit \
  --provider api \
  --prompt "Minimal editorial hero illustration: a robot gardener tending a rust-colored flower" \
  --output-dir artifacts/design \
  --prefix robot-garden \
  --n 2 \
  --quality low \
  --job-file artifacts/design/robot-garden-job.json \
  --json

codex-image batch retrieve \
  --job-file artifacts/design/robot-garden-job.json \
  --wait \
  --max-wait-seconds 300 \
  --json
```

To inspect local API estimates without a key or network access, use `cost`. Add `--day-by-day` and `--per-request` when a detailed audit is needed:

```bash
codex-image cost --period week --day-by-day --per-request --json
```

For large asset sets, use a bounded manifest run. Plan first, then pass the exact returned digest to the billable command:

```bash
codex-image run plan \
  --manifest assets.jsonl \
  --output-dir artifacts/design \
  --mode batch \
  --parallelism 8 \
  --max-active-batches 1 \
  --wait \
  --max-wait-seconds 300 \
  --poll-interval-seconds 10 \
  --json

codex-image run batch \
  --manifest assets.jsonl \
  --output-dir artifacts/design \
  --run-file artifacts/design/run.json \
  --approve-plan <plan-sha256> \
  --shard-size 8 \
  --wait \
  --json
```

The manifest is bounded and contains only `id`, `prompt`, and optional safe `name` fields. Run state stores IDs, paths, shard/job state, and the plan digest, never prompts or credentials. Use `run direct` for bounded parallel synchronous work; its default concurrency is one and it never retries a generation POST.

## Contract for tool authors

- **No interaction:** do not use `stdin`; `--prompt-file -` is deliberately rejected to prevent blocking.
- **No credential argument:** keys belong in the child process environment only.
- **No hidden retry:** one command makes at most one image-generation operation.
- **Batch recovery:** `batch submit` persists a job file; use `batch status`, `batch retrieve`, or `batch cancel` instead of resubmitting an unknown POST.
- **Files permission:** Batch input upload requires OpenAI `api.files.write`. A
  401 `api_files_write_scope_missing` result is terminal for that submission;
  grant the scope or use an authorized key, then start a fresh explicit job.
- **Explicit reconciliation:** use `batch recover` with a manually verified `--input-file-id` or `--batch-id` for unknown POST outcomes. It never retries the unknown POST, and may issue one new creation POST only from the confirmed `input_uploaded` state.
- **Endpoint trust:** repeat exact custom-origin or loopback approval flags on each Batch operation; never treat an editable job file as credential-destination approval.
- **No unsafe cleanup:** synchronous `generate` reserves private stages before its billable POST, so a known non-overwrite collision yields code 3 and no request. Batch output collisions are checked during retrieval, after remote Batch work may already have been billed; atomic no-clobber publication still protects competing writers.
- **No silent partial success:** every returned image must decode and match the requested PNG/JPEG/WebP container before publishing. Multi-file publishing cannot be atomically visible as a set, so an error reports possibly modified/retained paths instead of claiming success or deleting a concurrent replacement.
- **Provider choice:** the default selects API billing through `OPENAI_API_KEY`; `--provider codex` explicitly selects the authenticated Codex CLI subscription.
- **Quality gate:** `low` is the default; `--quality high` requires `--confirm-high-quality` after reviewing the cost warning.
- **Cost accounting:** `cost` reads only the local append-only ledger; missing token usage and custom origins are unpriced, `estimate_coverage: "partial"` means the displayed amount is not complete, and unknown outcomes are never treated as zero or auto-retried.
- **Preflight cost:** `cost_preview` is a planning estimate, not an authoritative bill or spending cap. Keep it separate from usage-derived `cost` totals and do not infer zero from an unavailable preview.
- **Large runs:** plan and approve a manifest digest; never edit a manifest or run file after approval, and never resubmit assets marked in-flight or outcome-unknown.

See [API contract](API-CONTRACT.md) for schemas and exit-code semantics.
