#!/usr/bin/env python3
"""Print the changelog notes for a release version.

Usage: release_notes.py <version> [pages-dir]

The version may carry a leading "v". Exits non-zero when the version has no
changelog page or the page still has its draft summary, so a tag cannot ship
without curated notes.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from changelog import PAGES, PLACEHOLDER, front_matter, notes  # noqa: E402


def main() -> int:
    if len(sys.argv) not in (2, 3):
        print(__doc__, file=sys.stderr)
        return 2
    version = sys.argv[1].removeprefix("v")
    pages = Path(sys.argv[2]) if len(sys.argv) == 3 else PAGES
    page = pages / f"{version}.md"
    text = page.read_text(encoding="utf-8") if page.exists() else ""
    body = notes(text)
    if not body:
        print(f"no changelog page for {version} in {pages}", file=sys.stderr)
        return 1
    if front_matter(text).get("summary") in ("", PLACEHOLDER):
        print(f"{page} still has the draft summary", file=sys.stderr)
        return 1
    print(body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
