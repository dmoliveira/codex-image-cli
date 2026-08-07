# Initial public launch plan 🚀

## Scope

Deliver a public Rust CLI named `codex-image` for non-interactive GPT Image 2 generation, with safe local installs, AI-agent discovery, voluntary Stripe support messaging, and companion `agents_md` guidance.

## Sequencing

1. ✅ Bootstrap the public `dmoliveira/codex-image-cli` repository and a dedicated feature worktree.
2. ✅ Implement the API-key-only core with endpoint policy, one-POST behavior, output transaction safety, JSON reports, and mock tests.
3. ✅ Added docs, hero/banner, CI/release workflow, install/update guidance, and a tmux E2E harness.
4. ✅ Updated `agents_md` through [PR #91](https://github.com/dmoliveira/agents.md/pull/91).
5. ✅ Completed high-risk validation/review, merged [core PR #1](https://github.com/dmoliveira/codex-image-cli/pull/1), protected `main`, published [v0.1.0](https://github.com/dmoliveira/codex-image-cli/releases/tag/v0.1.0), verified its checksum, and synced upstream state.

## Delivery evidence

- Core CI passed on Ubuntu and macOS after PR #1.
- The tagged release workflow reran format, Clippy, tests, install/update smoke, local tmux E2E, release build, checksum creation, and GitHub release publication.
- The release checksum was verified against the downloaded artifact; future workflows emit filename-relative checksum entries.

## Release gates

- No claim that a ChatGPT/Codex subscription grants documented Image API access.
- No key in argv, logs, reports, URLs, fixtures, tracked files, or release artifacts.
- No automatic retry after a generation POST.
- Private output stages are reserved before the request; malformed/partial responses never claim success or trigger unsafe pathname cleanup.
- `cargo fmt`, Clippy with warnings denied, tests, docs checks, release build, secret scan, and tmux E2E are green.
