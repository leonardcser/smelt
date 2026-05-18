#!/usr/bin/env python3
"""Generate the smelt SVG logo assets.

The logo is the wordmark with one extracted fire frame above it. This script is
the only Python logo generator; it writes the docs/homepage light and dark
variants plus the default nav logo alias.
"""

from __future__ import annotations

from pathlib import Path

# SVG pixels are 1 wide; bump height slightly so proportions stay close to the
# terminal half-block rendering.
PIXEL_HEIGHT = 1.1

FIRE_FRAME = [
    "......R.............",
    "......OO............",
    ".....ROooOR.........",
    "....ROoYYoOR.RO.....",
    "...ROoYYYYYoOooO....",
    ".ROooYYYYYYYoYYoOR..",
    "....................",
]

WORDMARK_PIXELS = [
    "WWW.WWWWW.WWW.W..WWW",
    "WGG.WGWGW.WGG.W..GWG",
    "..W.W.W.W.W...W...W.",
    "WWW.W.W.W.WWW.WW..WW",
]

FIRE_COLORS = {
    "R": "#af0000",
    "O": "#ff5f00",
    "o": "#ff8700",
    "Y": "#ffd700",
}

THEMES = {
    "light": {
        "W": "#241611",
        "G": "#7c6257",
    },
    "dark": {
        "W": "#fff6ef",
        "G": "#9b8880",
    },
}


def logo_rows() -> list[str]:
    rows = [*FIRE_FRAME, *WORDMARK_PIXELS]
    width = len(rows[0])
    if any(len(row) != width for row in rows):
        raise ValueError("logo rows must share one width")
    return rows


def render_svg(rows: list[str], label: str, theme: str) -> str:
    wordmark_colors = THEMES[theme]
    colors = {**FIRE_COLORS, **wordmark_colors}
    width = len(rows[0])
    height = len(rows) * PIXEL_HEIGHT
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height:g}" '
        f'shape-rendering="crispEdges" role="img" aria-label="{label}">',
    ]
    for y, row in enumerate(rows):
        for x, ch in enumerate(row):
            color = colors.get(ch)
            if not color:
                continue
            parts.append(
                f'<rect x="{x}" y="{y * PIXEL_HEIGHT:g}" width="1" '
                f'height="{PIXEL_HEIGHT:g}" fill="{color}"/>'
            )
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def main() -> None:
    repo_root = Path(__file__).resolve().parent.parent
    out_dir = repo_root / "docs" / "docs"
    rows = logo_rows()

    light = render_svg(rows, "smelt logo", "light")
    dark = render_svg(rows, "smelt logo", "dark")

    (out_dir / "logo-light.svg").write_text(light)
    (out_dir / "logo-dark.svg").write_text(dark)
    # The docs nav has one logo path; use the dark wordmark so it remains
    # visible on the orange header.
    (out_dir / "logo.svg").write_text(dark)

    print(
        "wrote "
        f"{out_dir / 'logo-light.svg'}, "
        f"{out_dir / 'logo-dark.svg'}, "
        f"{out_dir / 'logo.svg'}"
    )


if __name__ == "__main__":
    main()
