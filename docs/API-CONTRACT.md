# CLI API contract 📐

This document describes the stable behavior of version 4 JSON reports for synchronous generation and the durable Batch lifecycle.

## JSON rules

With `--json`, the CLI writes exactly one JSON object and a newline to stdout. Ordinary application errors do not write to stderr in JSON mode. Reports never include an API key, authorization header, prompt body, server body, or base64 image data.

Every generation report includes:

```json
{
   "schema_version": 4,
  "ok": true,
  "status": "success",
  "exit_code": 0,
  "request": {
    "attempted": true,
    "image_count": 1,
    "model": "gpt-image-2",
    "provider": "api",
    "request_id": "optional-safe-request-id"
  },
  "http": { "status": 200 },
  "outputs": ["artifacts/design/hero.png"],
  "retained_artifacts": [],
   "possibly_modified_paths": [],
   "cost_preview": {
     "status": "estimated",
     "currency": "USD",
     "scope": "output_only",
     "total_cost_status": "unknown",
     "model": "gpt-image-2",
     "transport": "live",
     "image_count": 1,
     "quality": "medium",
     "size": "1536x1024",
     "pricing_version": "openai-gpt-image-2-2026-08",
     "pricing_source": "https://developers.openai.com/api/docs/guides/image-generation#calculating-costs",
     "basis": "official_per_image_output_price_table",
     "estimated_output_nano_usd": 41000000,
     "estimated_output_usd": "$0.041000",
     "excluded_charges": ["text_input_tokens", "image_input_tokens"]
   }
}
```

Failure reports add:

```json
"error": {
  "code": "transport_outcome_unknown",
  "message": "The image POST may have been processed ...",
  "automatic_retry_safe": false
}
```

`outputs` is populated only after all requested files are published. `retained_artifacts` lists private backups deliberately retained after a successful overwrite; they are not automatically unlinked because a pathname-only cleanup can race a competing writer. `possibly_modified_paths` lists outputs/private artifacts that need inspection after a failure. The CLI never calls a multi-file result successful after a partial publication.

`cost_preview` is a non-binding preflight estimate of image-output charges. Its `scope` is always `output_only` and `total_cost_status` is always `unknown`. It is included in dry-run, successful generation, and Batch lifecycle reports after request validation. The estimate uses the official GPT Image 2 per-image output table only for `1024x1024`, `1024x1536`, and `1536x1024`; Batch applies the documented 50% rate. Prompt text and input-image charges are excluded because the CLI cannot derive authoritative pre-request token usage. `status: "unavailable"` includes a machine-readable `reason` such as `custom_endpoint_unpriced`, `auto_size_not_in_official_table`, or `size_or_quality_not_in_official_table`. An unavailable preview is not a safety failure and does not imply zero cost.

`generate --dry-run` and `batch submit --dry-run` validate endpoint policy and inspect output-directory/target safety without reading a key, writing the ledger, creating stages/job files, or using the network. `--quality high` still requires confirmation for a billable request, but dry-run may show its request-specific preview without `--confirm-high-quality`.

Batch reports add `operation`, job/remote IDs, `remote_status`, `request_counts`, `next_action`, and the same `outputs`, `retained_artifacts`, and `possibly_modified_paths` fields. `batch submit` uploads bounded JSONL input and creates a 24-hour Batch job with output retention configured for 30 days. A newly created Batch may validly report `status: "validating"` with zero request counts while the service parses the input; the client preserves that state and validates counts again once processing begins. `batch status` performs one read and exposes the latest completed/failed/total counts, `batch retrieve` can poll with `--wait` and a bounded `--max-wait-seconds`, `batch cancel` sends one cancellation request, and `batch recover` resumes only a confirmed-safe local state or attaches a manually reconciled remote ID after a read-only verification. Batch is API-only, locally limited to 8 image requests, and its job record never stores the prompt, API key, or image bytes. Remote POST outcomes are never automatically retried. Custom HTTPS and loopback endpoint approvals must be repeated explicitly on each operation; editable job records never grant credential-destination approval.

Manifest run reports use `schema_version: 1` and `operation` values `run.plan`, `run.direct`, or `run.batch`. They expose `plan_digest`, aggregate asset counts, secret-free per-asset direct outcomes or per-shard Batch outcomes, and `next_action`. `run direct` requires `--run-file` and exact `--approve-plan` for billable execution; it marks each asset `dispatch_in_flight` before its one generation POST. `run batch` persists all shard/job paths before any upload/create POST. `outcome_unknown` and `dispatch_in_flight` entries are never automatically retried. `run direct` exits 5 for unknown outcomes, 10 for definitive failures/stopped work, and 0 only when every asset succeeds; `run batch` exits 8 while child Batches remain pending.

## Cost tracking

Every direct API image POST and every Batch image request gets an immutable local ledger record. A durable `started` record is synchronized before a potentially billable POST; later response/output observations resolve that record without double-counting repeated status, retrieval, or recovery operations. Unknown POST outcomes remain pending/unknown and are never silently treated as zero.

The default ledger is `XDG_STATE_HOME/codex-image/costs.jsonl`, falling back to `XDG_CONFIG_HOME/codex-image/costs.jsonl` and then `$HOME/.local/state/codex-image/costs.jsonl`. It is append-only JSONL protected by a sidecar lock and contains request metadata, safe request IDs, Batch/custom IDs, optional token usage, outcome, and estimate metadata. Prompts, API keys, authorization headers, image bytes, and response bodies are never recorded. Use `--ledger-file FILE` to inspect another ledger.

`cost` never reads a key or uses the network. Dates are inclusive UTC calendar dates. Examples:

```bash
codex-image cost --period today --json
codex-image cost --period week --day-by-day --json
codex-image cost --period month --per-request --json
codex-image cost --period year --day-by-day --per-request --json
codex-image cost --from 2026-08-01 --to 2026-08-08 --day-by-day --per-request --json
```

The report uses cost schema version 2, separates live and Batch totals, and includes requests, image counts, priced/unpriced/pending-known/unknown counts, `estimate_coverage`, day rows, and optional per-request rows. `estimate_coverage` is `complete` only when every non-rejected operation has a usable finalized usage estimate; otherwise it is `partial`. `pending_requests` and `unknown_requests` are disjoint; unknown outcomes may have been billed and must not be auto-retried. Amounts are local estimates in USD, calculated from recorded token usage using the versioned GPT Image 2 rate snapshot; OpenAI billing/dashboard records remain authoritative. Missing usage is unpriced. Compatible loopback/custom origins are recorded but unpriced because the OpenAI rate card is not assumed for non-canonical endpoints. Batch uses the documented discounted rate card. The CLI does not infer a fixed per-image charge when token usage is absent. `--day-by-day` emits every selected UTC calendar day and rejects ranges longer than 3,700 days instead of silently omitting zero-usage days.

```json
{
   "schema_version": 4,
  "operation": "batch.submit",
  "ok": true,
  "status": "validating",
  "exit_code": 0,
  "job_file": "artifacts/design/job.json",
  "job_id": "job-example",
  "batch_id": "batch_example",
  "input_file_id": "file-example",
  "remote_status": "validating",
  "request_counts": { "completed": 0, "failed": 0, "total": 0 },
  "request": { "attempted": true, "image_count": 2, "model": "gpt-image-2", "provider": "api" },
  "http": { "status": 200 },
  "outputs": [],
  "retained_artifacts": [],
  "possibly_modified_paths": [],
  "next_action": "run batch status or batch retrieve with this job file"
}
```

`request.provider` is `api` or `codex`. `request.model` is present only when the provider guarantees the model (`gpt-image-2` for `api`); Codex's built-in image model is intentionally not claimed. `http.status` is present only for API responses. Codex process failures may add `process_exit_code`, `process_timed_out`, `diagnostics_bytes`, and `diagnostics_truncated` to `error`.

A cancelled Batch without an output file is a successful terminal observation with `status: "cancelled"` and an empty `outputs` array. If a cancelled Batch has an output file, retrieval uses the same all-or-nothing result validation as other Batch outputs; partial JSONL results are not published.

Terminal `failed` or `expired` Batches are also retrieved when `output_file_id` is present, so available completed results are not discarded. Without an output file, retrieval reports the terminal Batch failure and retains any available error metadata.

## Exit codes

| Code | Status | API request attempted? | Agent behavior |
| ---: | --- | --- | --- |
| 0 | `success` / `dry_run` | yes / no | Consume outputs or planned paths. |
| 2 | `usage_error` | no | Fix flags, prompt, local key presence, or endpoint policy. |
| 3 | `preflight_error` | no | Fix directory, symlink, collision, permission, or reservation issue. |
| 4 | `api_rejected` | yes | Inspect the safe HTTP/request ID information; change input/config if appropriate. |
| 5 | `outcome_indeterminate` | possibly | Do **not** auto-retry; the POST may have been processed/billed. |
| 6 | `invalid_success_response` | yes | Do **not** auto-retry; 2xx data was unsafe/malformed/oversized. |
| 7 | `output_commit_failed` | yes | Do **not** auto-retry; inspect paths and determine whether output is recoverable. |
| 8 | `batch_not_ready` | read-only | The Batch is still processing or the bounded wait elapsed; query it again later. |
| 9 | `batch_observation_failed` | read-only | A Batch read failed; retrying the observation is safe. |
| 10 | `batch_failed` | read-only | The Batch failed, expired, or its output file is unavailable; inspect remote error/output metadata before deciding what to do next. |

A redirect is refused without forwarding credentials, but it is classified as code 5 rather than code 4 because a POST may have been processed before the redirect response arrived.

## GPT Image 2 compatibility

The CLI sends one documented Image API `POST /v1/images/generations` request with:

| Field | Local contract |
| --- | --- |
| `model` | fixed `gpt-image-2` |
| `prompt` | non-empty, ≤32,000 Unicode scalar values; prompt files are bounded to 256 KiB before decoding |
| `n` | 1–4 for `generate`; 1–8 for `batch submit` |
| `size` | `1024x1024` by default, or `auto`/dimensions meeting GPT Image 2’s current 16px/edge/ratio/pixel constraints |
| `quality` | `low`, `medium`, `high`, `auto`; `high` requires `--confirm-high-quality` |
| `background` | `auto` or `opaque`; transparent is locally rejected for GPT Image 2 |
| `output_format` | `png`, `jpeg`, `webp` |
| `output_compression` | 0–100, only JPEG/WebP |
| `moderation` | `auto`, `low` |

Responses must contain exactly `n` `data[].b64_json` values. The CLI limits a decoded image to 32 MiB and verifies the requested container signature (PNG, JPEG, or WebP) before writing.

## Manifest input

`run plan`, `run direct`, and `run batch` accept bounded UTF-8 JSONL manifests. A record is `{ "id": "safe-id", "prompt": "...", "name": "optional-safe-stem" }`; `name` defaults to `id`. The current local limits are 50,000 assets, 64 MiB per manifest, and 32,000 Unicode scalar values per prompt. IDs and names are unique and ASCII-safe. Blank lines are ignored; unknown fields and malformed records are rejected before key reads or network requests.

Run output directories must already exist as regular non-symlink directories, contain no symlinked path components, and have valid UTF-8 paths.

## Provider/credential policy

- Default provider: the direct Image API, using `OPENAI_API_KEY` from the environment.
- The Codex provider is explicit with `--provider codex`; it currently supports one PNG per command and validates the generated bytes before publication.
- Batch commands require `--provider api` and use the durable lifecycle described above.

`ai-help --json` exposes the provider capability matrix. The Codex provider accepts one PNG request and treats `size` and `quality` as best-effort hints. It rejects compression, non-auto background, and non-auto moderation before starting Codex. The API provider supports the full parameter set below.

- Default origin: `https://api.openai.com/v1`.
- API keys come only from `OPENAI_API_KEY` when `--provider api` is selected.
- Proxies and redirects are disabled.
- Local HTTP requires both a loopback host and `--allow-insecure-localhost`.
- Any non-loopback custom HTTPS origin requires `--dangerously-allow-api-key-to` containing that exact origin.
- URL userinfo, query, fragment, and non-loopback HTTP are rejected before a request.

Batch input upload requires an API key with OpenAI's `api.files.write` scope. A
recognized HTTP 401 response that explicitly reports this missing scope is
returned as the stable `api_files_write_scope_missing` API-rejection code. The
CLI never echoes the server message, never retries the rejected upload, and
does not proceed to Batch creation; grant the scope or use an authorized key
before beginning a fresh explicit submission.

Custom endpoint approval is an explicit trust decision, not a statement of OpenAI compatibility.

## Structured requests

`--request-file FILE` accepts bounded UTF-8 JSON with `schema_version: 1` and a required `prompt`. It may contain `provider`, `n`, `format`, `size`, `quality`, `background`, `compression`, and `moderation`. It is exclusive with those generation flags; output naming, overwrite, endpoint URLs, credential-destination approvals, and insecure-localhost approval remain CLI-only controls. Unknown fields, malformed JSON, stdin (`-`), and oversized files are rejected before key reads, reservation, subprocess creation, or network access.

## Secure output-platform support

Synchronous `generate` currently supports macOS and Linux. Those builds pin every output-directory component with descriptor-relative `openat` operations, reject symlinks, preflight-check final targets, reserve private stages before the billable POST, and use atomic no-clobber/exchange publication with identity checks. Batch output reservation happens during retrieval after remote work may have been billed, but uses the same atomic publication protections. Private stages/backups are retained rather than automatically deleted when a name could have been concurrently replaced. On other platforms the CLI fails closed with `secure_output_transactions_unsupported` before the API request; `--dry-run` remains available.
