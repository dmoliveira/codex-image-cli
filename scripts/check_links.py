#!/usr/bin/env python3
"""Validate repository-local Markdown links without network access."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LINK = re.compile(r"(?<!!)\[[^]]*\]\(([^)]+)\)")
SKIP_PREFIXES = ("http://", "https://", "mailto:", "#")


def local_target(raw: str) -> str | None:
    target = raw.strip().split(maxsplit=1)[0].strip("<>")
    if not target or target.startswith(SKIP_PREFIXES):
        return None
    return target.split("#", maxsplit=1)[0]


def main() -> int:
    missing: list[str] = []
    for markdown in ROOT.rglob("*.md"):
        if any(part in {".git", "target", ".opencode"} for part in markdown.parts):
            continue
        text = markdown.read_text(encoding="utf-8")
        for match in LINK.finditer(text):
            target = local_target(match.group(1))
            if target and not (markdown.parent / target).exists():
                missing.append(f"{markdown.relative_to(ROOT)} -> {target}")
    if missing:
        print("Broken local Markdown links:", *missing, sep="\n", file=sys.stderr)
        return 1
    print("Markdown local links: OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
