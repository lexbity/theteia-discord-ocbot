#!/usr/bin/env python3
"""Convert Markdown documentation into Hugo-compatible content.

Reads every .md file from a source directory (default: ./docs), categorizes
it by filename keywords, and writes it to content/docs/<category>/ with
generated Hugo front matter.

Usage:
    python3 export_docs.py [--source ./docs] [--dest content/docs] [--project NAME]
"""

from __future__ import annotations

import argparse
import re
from datetime import datetime, timezone
from pathlib import Path

# Ordered categories: the position in this list is the category_index used
# for the front-matter weight (10 + category_index * 5).
CATEGORIES: list[tuple[str, tuple[str, ...]]] = [
    ("getting-started", ("readme", "intro", "overview", "install", "setup", "configure")),
    ("guides", ("usage", "guide", "tutorial")),
    ("reference", ("api", "reference", "docs")),
    ("miscellaneous", ()),  # fallback: everything else
]

# An existing YAML/TOML front-matter block at the top of a source file.
FRONT_MATTER_RE = re.compile(r"\A---[^\n]*\n.*?\n---[^\n]*\n?", re.DOTALL)
# First level-1 ATX heading in the body.
HEADING_RE = re.compile(r"^#\s+(.+?)\s*$", re.MULTILINE)


def categorize(stem: str) -> tuple[str, int]:
    """Return (category_name, category_index) for a filename stem."""
    lowered = stem.lower()
    for index, (name, keywords) in enumerate(CATEGORIES):
        if any(keyword in lowered for keyword in keywords):
            return name, index
    fallback_index = len(CATEGORIES) - 1
    return CATEGORIES[fallback_index][0], fallback_index


def extract_title(content: str, stem: str) -> str:
    """Use the first `# heading` in the body, else a readable filename."""
    match = HEADING_RE.search(content)
    if match:
        return match.group(1).strip()
    return re.sub(r"[-_]+", " ", stem).strip().title()


def yaml_quote(value: str) -> str:
    """Quote a string for double-quoted YAML."""
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def lastmod(mtime: float) -> str:
    return datetime.fromtimestamp(mtime, tz=timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%S+00:00"
    )


def build_front_matter(title: str, weight: int, mtime: float) -> str:
    return (
        "---\n"
        f"title: {yaml_quote(title)}\n"
        f"weight: {weight}\n"
        "type: docs\n"
        "draft: false\n"
        f"lastmod: {lastmod(mtime)}\n"
        "---\n\n"
    )


def convert_file(path: Path, project: str) -> tuple[str, str, int]:
    """Return (category_name, rel_output_path, weight, rendered_document)."""
    category, category_index = categorize(path.stem)
    raw = path.read_text(encoding="utf-8")
    body = FRONT_MATTER_RE.sub("", raw, count=1).lstrip("\ufeff")
    title = f"{project} - {extract_title(body, path.stem)}"
    weight = 10 + category_index * 5
    front_matter = build_front_matter(title, weight, path.stat().st_mtime)
    rel = Path(category) / path.name
    return category, rel, weight, front_matter + body


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--source", type=Path, default=Path("docs"),
                        help="Directory containing source .md files")
    parser.add_argument("--dest", type=Path, default=Path("content/docs"),
                        help="Hugo content output directory")
    parser.add_argument("--project", default=None,
                        help="Project name for titles (default: repo directory name)")
    args = parser.parse_args()

    project = args.project or Path.cwd().name
    md_files = sorted(p for p in args.source.glob("*.md") if p.is_file())
    if not md_files:
        raise SystemExit(f"No .md files found in {args.source}/")

    written: list[tuple[str, Path, int]] = []
    for path in md_files:
        category, rel, weight, output = convert_file(path, project)
        out_path = args.dest / rel
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(output, encoding="utf-8")
        written.append((category, out_path, weight))

    for category, out_path, weight in written:
        print(f"[{category:<15}] weight={weight:<3} {out_path}")
    print(f"\nConverted {len(written)} file(s) into {args.dest}/")


if __name__ == "__main__":
    main()
