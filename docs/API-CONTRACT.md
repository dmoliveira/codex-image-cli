# CLI API contract 📐

This document describes the stable behavior of version 2 JSON reports for synchronous generation and the durable Batch lifecycle.

## JSON rules

With `--json`, the CLI writes exactly one JSON object and a newline to stdout. Ordinary application errors do not write to stderr in JSON mode. Reports never include an API key, authorization header, prompt body, server body, or base64 image data.

Every generation report includes:

```json
{
  "schema_version": 2,
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
  "possibly_modified_paths": []
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

Batch reports add `operation`, job/remote IDs, `remote_status`, `request_counts`, `next_action`, and the same `outputs`, `retained_artifacts`, and `possibly_modified_paths` fields. `batch submit` uploads bounded JSONL input and creates a 24-hour Batch job with output retention configured for 30 days. A newly created Batch may validly report `status: "validating"` with zero request counts while the service parses the input; the client preserves that state and validates counts again once processing begins. `batch status` performs one read and exposes the latest completed/failed/total counts, `batch retrieve` can poll with `--wait` and a bounded `--max-wait-seconds`, `batch cancel` sends one cancellation request, and `batch recover` resumes only a confirmed-safe local state or attaches a manually reconciled remote ID after a read-only verification. Batch is API-only, locally limited to 8 image requests, and its job record never stores the prompt, API key, or image bytes. Remote POST outcomes are never automatically retried. Custom HTTPS and loopback endpoint approvals must be repeated explicitly on each operation; editable job records never grant credential-destination approval.

```json
{
  "schema_version": 2,
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
