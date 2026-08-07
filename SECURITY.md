# Security policy 🔒

## Supported versions

Only the latest released version receives security fixes.

## Reporting a vulnerability

Please use [GitHub private vulnerability reporting](https://github.com/dmoliveira/codex-image-cli/security/advisories/new) rather than a public issue for credential handling, unsafe file writes, or billing-related flaws.

## Key-handling commitments

- `OPENAI_API_KEY` is read only for a real generation request.
- The key is never accepted via command-line flag, URL, prompt file, JSON output, or logs.
- Redirects and proxies are disabled so the selected endpoint is the credential destination.
- Custom origins require an explicit exact-origin acknowledgement; insecure HTTP is limited to acknowledged loopback tests.
- macOS/Linux output writes use descriptor-relative, identity-checked publication. Unsupported platforms fail before an API request instead of using a weaker fallback.
- Private stage/backup entries are retained when deletion could race a competing writer; inspect JSON `retained_artifacts` or `possibly_modified_paths` before manual cleanup.
- No telemetry is collected by this CLI.

Use a scoped API key, rotate it if you suspect exposure, and review the OpenAI API activity/billing dashboard after any indeterminate generation result.
