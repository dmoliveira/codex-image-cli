# AI agent guide 🤖

`codex-image` is designed to be called by agents without a terminal prompt, browser, or human confirmation.

## Discover first

```bash
codex-image ai-help --json
codex-image doctor --json
```

`doctor` checks that the local `codex` executable is available. It does not send a request and does not verify login, entitlement, or billing remotely.

## Safe execution recipe

1. Create a project-owned, non-symlinked directory such as `artifacts/design`.
2. Choose a safe output stem: ASCII letters/digits, then letters/digits/`_`/`-`, 1–80 characters.
3. Run `generate --dry-run --json` and parse its one JSON object.
4. Invoke `generate --json` once; the default provider uses the authenticated Codex CLI subscription.
5. Parse `ok`, `status`, `exit_code`, `outputs`, `retained_artifacts`, and `possibly_modified_paths`.
6. Never auto-retry codes 5–7. Surface the API request ID and path list to the caller instead.

```bash
mkdir -p artifacts/design

codex-image generate \
  --prompt "Minimal editorial hero illustration: a robot gardener tending a rust-colored flower" \
  --output-dir artifacts/design \
  --prefix robot-garden \
  --n 1 \
  --format png \
  --quality medium \
  --dry-run \
  --json

codex-image generate \
  --prompt "Minimal editorial hero illustration: a robot gardener tending a rust-colored flower" \
  --output-dir artifacts/design \
  --prefix robot-garden \
  --n 1 \
  --format png \
  --quality medium \
  --json
```

## Contract for tool authors

- **No interaction:** do not use `stdin`; `--prompt-file -` is deliberately rejected to prevent blocking.
- **No credential argument:** keys belong in the child process environment only.
- **No hidden retry:** one command makes at most one image POST.
- **No unsafe cleanup:** private stages are reserved before the POST. A known non-overwrite collision yields code 3 and no request; a later competing target is protected by atomic no-clobber publication.
- **No silent partial success:** every returned image must decode and match the requested PNG/JPEG/WebP container before publishing. Multi-file publishing cannot be atomically visible as a set, so an error reports possibly modified/retained paths instead of claiming success or deleting a concurrent replacement.
- **Provider choice:** the default delegates to the authenticated Codex CLI subscription; `--provider api` explicitly selects API billing through `OPENAI_API_KEY`.

See [API contract](API-CONTRACT.md) for schemas and exit-code semantics.
