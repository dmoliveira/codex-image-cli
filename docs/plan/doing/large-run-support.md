# Large Run Support

## Scope

Add a manifest-driven `run` surface without changing existing `generate`, `batch`, or schema-v2 contracts.

## Slices

1. Add bounded manifest parsing and canonical plan digests.
2. Add durable, plan-approved direct execution with bounded concurrency and no POST retries.
3. Add durable Batch shard coordination over the existing single-job lifecycle.
4. Add offline fault-boundary, concurrency, resume, and secret-leakage coverage.
5. Document operational limits and validate the full repository gate.

## Safety Invariants

- All manifest and output validation happens before the first billable request.
- A run file is persisted before any request and binds the approved plan digest.
- Direct assets are marked `dispatch_in_flight` before their POST.
- Batch shard mapping is persisted before upload/create.
- Upload, create, cancel, and direct generation POSTs are never automatically retried.
- Read-only Batch observation may be repeated; ambiguous states stop new work.
- Reports and state never contain prompts, credentials, server bodies, or image bytes.
