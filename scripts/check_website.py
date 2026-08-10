#!/usr/bin/env python3
"""Validate the dependency-free GitHub Pages artifact without network access."""

from __future__ import annotations

import html.parser
import sys
from pathlib import Path
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parent.parent / "website"
PROJECT_BASE = "/codex-image-cli/"
SITE_URL = "https://dmoliveira.github.io/codex-image-cli/"
PAGES = {"index.html", "get-started.html", "guides.html", "reference.html", "project.html", "404.html"}
REQUIRED = {"styles.css", "script.js", "assets/hero-banner.svg"}
CANONICAL = {
    "docs/API-CONTRACT.md": ("automatic_retry_safe", "possibly_modified_paths", "Exit codes"),
    "docs/AI-AGENT-GUIDE.md": ("Never auto-retry codes 5–7", "--dry-run --json", "OPENAI_API_KEY"),
}


class PageParser(html.parser.HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.links: list[str] = []
        self.resources: list[str] = []
        self.canonicals: list[str] = []
        self.ids: list[str] = []
        self.h1_count = 0
        self.lang: str | None = None
        self.title_text = ""
        self.meta_description = False
        self._in_title = False

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        values = dict(attrs)
        if tag == "html":
            self.lang = values.get("lang")
        if tag == "a" and values.get("href"):
            self.links.append(values["href"] or "")
        if tag in {"img", "script"} and values.get("src"):
            self.resources.append(values["src"] or "")
        if tag == "link" and values.get("href"):
            self.resources.append(values["href"] or "")
            if "canonical" in values.get("rel", "").lower().split():
                self.canonicals.append(values["href"] or "")
        if values.get("id"):
            self.ids.append(values["id"] or "")
        if tag == "h1":
            self.h1_count += 1
        if tag == "title":
            self._in_title = True
        if tag == "meta" and values.get("name", "").lower() == "description":
            self.meta_description = bool(values.get("content"))

    def handle_data(self, data: str) -> None:
        if self._in_title:
            self.title_text += data

    def handle_endtag(self, tag: str) -> None:
        if tag == "title":
            self._in_title = False


def local_target(page: Path, raw: str) -> tuple[Path | None, str | None]:
    parsed = urlsplit(raw)
    if parsed.scheme or parsed.netloc or raw.startswith("mailto:"):
        return None, None
    target = parsed.path or page.name
    if target.startswith(PROJECT_BASE):
        target = target[len(PROJECT_BASE):]
    return (page.parent / target).resolve(), parsed.fragment or None


def main() -> int:
    errors: list[str] = []
    if not ROOT.is_dir():
        print(f"Missing website directory: {ROOT}", file=sys.stderr)
        return 1
    repository_root = ROOT.parent
    for relative, phrases in CANONICAL.items():
        source = repository_root / relative
        text = source.read_text(encoding="utf-8")
        for phrase in phrases:
            if phrase not in text:
                errors.append(f"canonical document changed unexpectedly: {relative} lacks {phrase!r}")
    for required in sorted(REQUIRED):
        if not (ROOT / required).is_file():
            errors.append(f"missing required website asset: {required}")
    pages = sorted(ROOT.glob("*.html"))
    if {page.name for page in pages} != PAGES:
        errors.append(f"expected pages {sorted(PAGES)}, found {sorted(page.name for page in pages)}")
    for page in pages:
        parser = PageParser()
        try:
            parser.feed(page.read_text(encoding="utf-8"))
        except (OSError, UnicodeError) as error:
            errors.append(f"{page.name}: cannot read: {error}")
            continue
        if parser.lang != "en":
            errors.append(f"{page.name}: html lang must be en")
        if parser.h1_count != 1:
            errors.append(f"{page.name}: expected exactly one h1, found {parser.h1_count}")
        if not parser.title_text.strip():
            errors.append(f"{page.name}: missing title text")
        if not parser.meta_description:
            errors.append(f"{page.name}: missing meta description")
        expected_canonical = SITE_URL if page.name == "index.html" else f"{SITE_URL}{page.name}"
        if parser.canonicals != [expected_canonical]:
            errors.append(f"{page.name}: expected one canonical link to {expected_canonical}")
        if any(not item for item in parser.ids):
            errors.append(f"{page.name}: empty fragment id")
        if len(parser.ids) != len(set(parser.ids)):
            errors.append(f"{page.name}: duplicate fragment id")
        for raw in [*parser.links, *parser.resources]:
            target, fragment = local_target(page, raw)
            if target is None:
                continue
            try:
                target.relative_to(ROOT.resolve())
            except ValueError:
                errors.append(f"{page.name}: link escapes website: {raw}")
                continue
            if not target.is_file():
                errors.append(f"{page.name}: missing local link: {raw}")
            elif fragment:
                target_parser = PageParser()
                target_parser.feed(target.read_text(encoding="utf-8"))
                if fragment not in target_parser.ids:
                    errors.append(f"{page.name}: missing fragment {raw}")
    if errors:
        print("Website validation failed:", *errors, sep="\n", file=sys.stderr)
        return 1
    print(f"Website validation: OK ({len(pages)} HTML pages)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
