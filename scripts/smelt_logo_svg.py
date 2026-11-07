#!/usr/bin/env python3
"""Generate the smelt SVG logo assets.

The logo is the wordmark with one extracted fire frame above it. This script is
the only Python logo generator; it writes the docs/homepage light and dark
variants, the default nav logo alias, and the favicon (the outlined S mark).
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

FAVICON_PIXELS = [
    "................",
    "................",
    "...KKKKKKKKKK...",
    "...KKKKKKKKKK...",
    "...KKWWWWWWKK...",
    "...KKWWWWWWKK...",
    "...KKWWggggKK...",
    "...KKWWggggKK...",
    "...KKKKKKWWKK...",
    "...KKKKKKWWKK...",
    "...KKWWWWWWKK...",
    "...KKWWWWWWKK...",
    "...KKKKKKKKKK...",
    "...KKKKKKKKKK...",
    "................",
    "................",
]

FAVICON_COLORS = {
    "K": "#17140f",
    "W": "#ffffff",
    "g": "#92908e",
}

FIRE_COLORS = {
    "R": "#af0000",
    "O": "#ff5f00",
    "o": "#ff8700",
    "Y": "#ffd700",
}

THEMES = {
    "light": {
        "W": "#241611",
        "G": "#a8958d",
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


def render_svg(
    rows: list[str],
    label: str,
    colors: dict[str, str],
    pixel_height: float = PIXEL_HEIGHT,
) -> str:
    width = len(rows[0])
    height = len(rows) * pixel_height
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
                f'<rect x="{x}" y="{y * pixel_height:g}" width="1" '
                f'height="{pixel_height:g}" fill="{color}"/>'
            )
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def main() -> None:
    repo_root = Path(__file__).resolve().parent.parent
    out_dir = repo_root / "docs" / "docs"
    rows = logo_rows()

    light = render_svg(rows, "smelt logo", {**FIRE_COLORS, **THEMES["light"]})
    dark = render_svg(rows, "smelt logo", {**FIRE_COLORS, **THEMES["dark"]})
    favicon = render_svg(FAVICON_PIXELS, "smelt", FAVICON_COLORS, pixel_height=1)

    (out_dir / "logo-light.svg").write_text(light)
    (out_dir / "logo-dark.svg").write_text(dark)
    # The docs nav has one logo path; use the dark wordmark so it remains
    # visible on the orange header.
    (out_dir / "logo.svg").write_text(dark)
    (out_dir / "favicon.svg").write_text(favicon)

    print(
        "wrote "
        f"{out_dir / 'logo-light.svg'}, "
        f"{out_dir / 'logo-dark.svg'}, "
        f"{out_dir / 'logo.svg'}, "
        f"{out_dir / 'favicon.svg'}"
    )


if __name__ == "__main__":
    main()
