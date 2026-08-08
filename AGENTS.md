# `codex-image` agent guide 🤖🖼️

`codex-image` is a zero-interaction Rust CLI for the documented OpenAI GPT Image 2 API.

## Non-negotiable safety rules

- A ChatGPT/Codex subscription and API billing are separate. Key presence does **not** prove entitlement, billing, organization verification, or model access.
- Never put keys in flags, prompts, files committed to git, URLs, process output, or chat logs.
- Never retry exit codes `5`, `6`, or `7` automatically: the POST may have been billed even when no usable output arrived.
- This CLI has no interactive mode. Provide every input through flags, an existing UTF-8 prompt file, and environment variables.

## Agent-first workflow

```bash
# Discover the stable machine contract; no API call is made.
codex-image ai-help --json

# Check local Codex/API configuration; this does not generate an image.
codex-image doctor --json

# Validate parameters/names without reading a key, reserving files, or using a network.
codex-image generate \
  --prompt "<prompt>" \
  --output-dir ./artifacts/design \
  --prefix hero \
  --n 1 \
  --dry-run \
  --json

# Generate through the direct Image API after the dry run is correct.
codex-image generate \
  --prompt "<prompt>" \
  --output-dir ./artifacts/design \
  --prefix hero \
  --n 1 \
  --json
```

Use `--provider codex` for the authenticated Codex subscription, or omit it for the direct Image API and set `OPENAI_API_KEY` explicitly. Prefer `--request-file FILE` for typed generation parameters; keep endpoint and overwrite approvals on CLI flags. Use `batch recover` for explicit reconciliation of unknown Batch POST outcomes; it never retries an unknown POST and may create once only from a confirmed uploaded-input state.

Create `--output-dir` explicitly. It must exist and contain no symlinked path components. Use `--name` only for a single image; use `--prefix` for deterministic multi-image names. Parse `retained_artifacts` and `possibly_modified_paths` before taking any cleanup action; the CLI deliberately avoids unsafe automatic deletion in a concurrently writable directory.

Read [`docs/AI-AGENT-GUIDE.md`](docs/AI-AGENT-GUIDE.md) and [`docs/API-CONTRACT.md`](docs/API-CONTRACT.md) before changing runtime behavior.
