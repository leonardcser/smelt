#!/usr/bin/env python3
"""Write the smelt logo SVG assets.

Imports the pixel art + palette from `smelt_logo` and emits:
  - docs/docs/logo.svg       (icon only, 14x12 pixels)
  - docs/docs/logo-full.svg  (icon + wordmark, 36x12 pixels)
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from smelt_logo import PALETTE, PIXEL_HEIGHT, full_rows, icon_rows


def render_svg(rows: list[str], label: str) -> str:
    px_h = PIXEL_HEIGHT
    width = len(rows[0])
    height = len(rows) * px_h
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height:g}" '
        f'shape-rendering="crispEdges" role="img" aria-label="{label}">',
    ]
    for y, row in enumerate(rows):
        for x, ch in enumerate(row):
            entry = PALETTE.get(ch)
            if entry is None:
                continue
            parts.append(
                f'<rect x="{x}" y="{y * px_h:g}" width="1" height="{px_h:g}" fill="{entry[1]}"/>'
            )
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def main() -> None:
    repo_root = Path(__file__).resolve().parent.parent
    out_dir = repo_root / "docs" / "docs"

    (out_dir / "logo.svg").write_text(render_svg(icon_rows(), "smelt logo"))
    (out_dir / "logo-full.svg").write_text(
        render_svg(full_rows(), "smelt logo with wordmark")
    )

    sys.stderr.write(
        f"wrote {out_dir / 'logo.svg'} and {out_dir / 'logo-full.svg'}\n"
    )


if __name__ == "__main__":
    main()
