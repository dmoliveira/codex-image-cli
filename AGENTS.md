# `codex-image` agent guide 🤖🖼️

`codex-image` is a zero-interaction Rust CLI for the documented OpenAI GPT Image 2 API.

## Non-negotiable safety rules

- Use **only** `OPENAI_API_KEY`; never attempt to reuse a ChatGPT, Codex, browser, or subscription session credential.
- A ChatGPT/Codex subscription and API billing are separate. Key presence does **not** prove entitlement, billing, organization verification, or model access.
- Never put keys in flags, prompts, files committed to git, URLs, process output, or chat logs.
- Never retry exit codes `5`, `6`, or `7` automatically: the POST may have been billed even when no usable output arrived.
- This CLI has no interactive mode. Provide every input through flags, an existing UTF-8 prompt file, and environment variables.

## Agent-first workflow

```bash
# Discover the stable machine contract; no API call is made.
codex-image ai-help --json

# Check only local key presence; this does not authenticate remotely.
codex-image doctor --json

# Validate parameters/names without reading a key, reserving files, or using a network.
codex-image generate \
  --prompt "<prompt>" \
  --output-dir ./artifacts/design \
  --prefix hero \
  --n 1 \
  --dry-run \
  --json

# Make one billable request only after the dry run is correct.
OPENAI_API_KEY="${OPENAI_API_KEY:?set this in your environment}" \
codex-image generate \
  --prompt "<prompt>" \
  --output-dir ./artifacts/design \
  --prefix hero \
  --n 1 \
  --json
```

Create `--output-dir` explicitly. It must exist and contain no symlinked path components. Use `--name` only for a single image; use `--prefix` for deterministic multi-image names. Parse `retained_artifacts` and `possibly_modified_paths` before taking any cleanup action; the CLI deliberately avoids unsafe automatic deletion in a concurrently writable directory.

Read [`docs/AI-AGENT-GUIDE.md`](docs/AI-AGENT-GUIDE.md) and [`docs/API-CONTRACT.md`](docs/API-CONTRACT.md) before changing runtime behavior.
