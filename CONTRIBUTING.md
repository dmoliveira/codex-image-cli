# Contributing 🤝

Thanks for improving `codex-image`.

## Local checks

```bash
make check
make e2e
```

The E2E harness uses only a local fake endpoint. Never use real API keys in tests, fixtures, screenshots, issues, or commits.

## Runtime changes

- Preserve zero-interaction behavior: no prompts, no browser auth, no implicit stdin reads.
- Keep API keys environment-only and redacted from every report/error path.
- A generation POST can be billable. Do not add automatic retries without documented, verified server-side idempotency.
- Add unit/integration coverage for endpoint policy, output handling, and JSON/exit-code behavior when changing them.
- Run `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets`, and `make e2e` before a pull request.
