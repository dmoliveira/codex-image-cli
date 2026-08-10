# Changelog 🗒️

All notable changes are documented here.

## Unreleased

- Made `low` the explicit preferred quality in agent templates and certified omitted-quality direct and Batch requests.
- Accepted the documented zero-count `validating` Batch response and persisted the confirmed Batch state instead of incorrectly marking a successful create as unknown.
- Added a closed-stdin tmux Batch E2E harness covering upload, create, status, retrieval, output validation, and credential-safe request logging.
- Defaulted image size to a lower-cost `1024x1024` PNG and exposed Batch request counts and retrieval next actions in status reports.
- Added plan-approved manifest runs with durable direct dispatch state, bounded direct concurrency, resumable Batch shards, and secret-free aggregate reports.

## 0.1.0 — 2026-08-07

- Initial public release of the non-interactive `codex-image` Rust CLI.
- Added safe GPT Image 2 generation, deterministic output naming, JSON reports, local doctor/AI help, and API billing safeguards.
- Added offline mock-server tests and a tmux end-to-end verification harness.
