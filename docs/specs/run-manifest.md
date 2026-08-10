# Run Manifest Contract

The `run` commands execute a bounded, reviewable set of API image requests. Existing `generate` and `batch` commands remain unchanged.

## Manifest

The manifest is an existing UTF-8 JSONL file. Blank lines are ignored. Each non-empty line is one asset:

```json
{"id":"hero-001","prompt":"A warm editorial hero image","name":"hero-001"}
```

`id` and `prompt` are required. `name` is optional and defaults to `id`; it is an output stem, not a filename. IDs and stems use the same ASCII-safe grammar as `--name`. Unknown fields, empty manifests, duplicate IDs, duplicate output stems, oversized files, and oversized prompts are rejected before any API request.

The parser is bounded to 50,000 assets and 64 MiB per manifest. The plan digest binds the ordered manifest contents, output directory, generation parameters, endpoint, execution mode, and worker/shard policy. Polling and wait duration are read-only operational controls and do not change the approved billable plan. It never appears with prompt contents in reports or run state.

Run output directories must already exist, be regular non-symlink directories with no symlinked path components, and have valid UTF-8 paths.

## Execution

```bash
codex-image run plan \
  --manifest assets.jsonl \
  --output-dir artifacts/design \
  --mode direct \
  --json

codex-image run direct \
  --manifest assets.jsonl \
  --output-dir artifacts/design \
  --run-file artifacts/design/run.json \
  --approve-plan PLAN_SHA256 \
  --max-concurrency 2 \
  --json
```

`run direct` is API-only. It defaults to one worker and allows at most four workers. Coordinators lock the run file's parent directory for the invocation, so the concurrency limit and default fail-stop policy apply across processes; use a dedicated state directory when unrelated runs must proceed concurrently. Run-file paths must be absolute-normalizable UTF-8 paths. Existing run files must be regular files with exactly one filesystem link; hard-link aliases are rejected before any API request. A run file and its companion `<run-file>.assets` directory are created before the first generation POST. Each asset has a complete durable sidecar state file, and missing or unsafe sidecars fail closed rather than reverting an asset to `planned`; legacy coordinator-only state is migrated under the execution lock. The sidecar directory is pinned and sidecar files are opened relative to that verified directory on macOS and Linux. Each asset is durably marked `dispatch_in_flight` before its one POST. A crash or unknown result is never automatically retried; resume reports it for explicit reconciliation. Definitive failures stop new dispatches by default; `--continue-on-error` may continue only after definitive, non-ambiguous failures.

`run batch` uses the same manifest and creates bounded durable child Batch jobs. Each child is prepared before its coordinator shard becomes `submitting`, and the complete shard map is persisted before the first upload/create POST. A planned shard without a child is prepared on resume; existing `prepared` or confirmed `input_uploaded` children resume only from those safe pre-POST states. A submitting shard without its child is incomplete and blocks rather than risking a duplicate; in-flight or unknown children are never resubmitted automatically. Competing `run batch` coordinators for the same parent directory are serialized for the invocation. A status observation without an authoritative remote status stops new shard submissions and returns the read-only observation error for safe retry. Batch coordinator state files use the same single-link/original-path restriction as direct runs. Batch output remains subject to the existing per-shard all-or-nothing publication contract.

Billable Batch submission and creation are supported only on macOS and Linux, matching the secure output backend. Other platforms may use `--dry-run` and read-only reconciliation commands; billable upload/create paths fail before reading the API key or sending a request.

Submission-only runs keep at most `--max-active-batches` remote child jobs in flight. Retrieval is intentionally serialized while output JSONL is buffered; this avoids multiplying the current response and decoded-image memory ceilings. Streaming retrieval is a follow-up optimization, not an implicit memory promise.

Plan approval is an integrity check, not entitlement or billing confirmation. API keys remain environment-only. Reports and run state contain IDs, paths, safe error codes/messages, and remote request IDs, never prompts, keys, authorization values, server bodies, or base64 image data.
