# Initial public launch plan 🚀

## Scope

Deliver a public Rust CLI named `codex-image` for non-interactive GPT Image 2 generation, with safe local installs, AI-agent discovery, voluntary Stripe support messaging, and companion `agents_md` guidance.

## Sequencing

1. ✅ Bootstrap the public `dmoliveira/codex-image-cli` repository and a dedicated feature worktree.
2. ✅ Implement the API-key-only core with endpoint policy, one-POST behavior, output transaction safety, JSON reports, and mock tests.
3. 🔄 Add docs, hero/banner, CI/release workflow, install/update guidance, and tmux E2E harness.
4. ⏳ Update `agents_md` after the CLI contract is validated.
5. ⏳ Run high-risk validation, security review, PR merge, tag/release, and upstream confirmation.

## Release gates

- No claim that a ChatGPT/Codex subscription grants documented Image API access.
- No key in argv, logs, reports, URLs, fixtures, tracked files, or release artifacts.
- No automatic retry after a generation POST.
- Private output stages are reserved before the request; malformed/partial responses never claim success or trigger unsafe pathname cleanup.
- `cargo fmt`, Clippy with warnings denied, tests, docs checks, release build, secret scan, and tmux E2E are green.
