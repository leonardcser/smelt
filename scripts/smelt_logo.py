#!/usr/bin/env python3
"""Smelt logo: source of truth for the pixel art + terminal renderer.

Running this script prints the colored flame+wordmark to stdout using ANSI
256 half-block chars (each terminal cell stacks two vertical pixels).

The pixel grid, palette, and pixel font are also imported by
`smelt_logo_svg.py` to emit the SVG assets.
"""

from __future__ import annotations

import re
import shutil
import sys
import time
from pathlib import Path

# Pixel symbol -> (ANSI 256-color index, hex). None entries are transparent.
PALETTE: dict[str, tuple[int, str] | None] = {
    ".": None,
    "R": (124, "#af0000"),  # dark red (outer edge of flame)
    "O": (202, "#ff5f00"),  # red-orange
    "o": (208, "#ff8700"),  # orange
    "Y": (220, "#ffd700"),  # yellow (hot inner glow)
    "L": (223, "#ffd7af"),  # peach (face)
    "E": (52, "#5f0000"),  # eyes / smile
    "W": (15, "#ffffff"),  # eye sparkle
    "G": (244, "#808080"),  # eye sparkle
}

RESET = "\x1b[0m"
DIM = "\x1b[2m"
WORDMARK = "smelt"

# SVG pixels are 1 wide; bump height slightly so the proportions match the
# terminal half-block rendering (monospace cells are taller than 2:1).
PIXEL_HEIGHT = 1.1

LOGO = """\
......RR......
.....ROOR.....
..R..RoooR....
..RRRoYYYoR...
.RoYYLLLLYYoR.
RoYLWELLLWEYoR
RoYLELLLLELYoR
RoYYLLEELLYYoR
.RoYYYYYYYYoR.
..RROooooORR..
....RRRRRR....
"""

# Animation keyframes for the wordmark+fire variant. All frames share the
# same dimensions (8 wide x 5 tall); cycle through them to flicker the flame.
MINI_FIRE_FRAMES = [
    """\
....R...
.R..OR..
..RRoR..
.ROYoOR.
ROYYYoOR
""",
    """\
....R...
....OR..
.RRRoR..
.RoYoOR.
ROYYYoOR
""",
    """\
....R...
...OOR..
..RRoR..
.ROYoOR.
RoYYYoOR
""",
]

FONT = {
    "s": [
        "WWW",
        "WGG",
        "..W",
        "WWW",
    ],
    "m": [
        "WWWWW",
        "WGWGW",
        "W.W.W",
        "W.W.W",
    ],
    "e": [
        "WWW",
        "WGG",
        "W..",
        "WWW",
    ],
    "l": [
        "W.",
        "W.",
        "W.",
        "WW",
    ],
    "t": [
        "WWW",
        "GWG",
        ".W.",
        ".WW",
    ],
}


def pixel_text(text: str) -> list[str]:
    height = len(next(iter(FONT.values())))
    rows = [""] * height
    for index, char in enumerate(text):
        glyph = FONT[char]
        if len(glyph) != height:
            raise ValueError(f"glyph {char!r} has {len(glyph)} rows, expected {height}")
        if index:
            rows = [row + "." for row in rows]
        rows = [row + glyph_row for row, glyph_row in zip(rows, glyph)]
    return rows


def pad_even(rows: list[str]) -> list[str]:
    if len(rows) % 2:
        rows.append("." * len(rows[0]))
    return rows


def icon_rows() -> list[str]:
    return pad_even(LOGO.strip("\n").splitlines())


def full_rows() -> list[str]:
    logo_rows = LOGO.strip("\n").splitlines()
    text_rows = pixel_text(WORDMARK)
    text_width = len(text_rows[0])
    gap = ".."
    top_padding = (len(logo_rows) - len(text_rows)) // 2
    top_padding += top_padding % 2

    composed = []
    for y, logo_row in enumerate(logo_rows):
        text_y = y - top_padding
        wordmark_row = (
            text_rows[text_y] if 0 <= text_y < len(text_rows) else "." * text_width
        )
        composed.append(logo_row + gap + wordmark_row)
    return pad_even(composed)


def wordmark_rows() -> list[str]:
    return pad_even(pixel_text(WORDMARK))


def wordmark_with_fire_rows(frame: int = 0) -> list[str]:
    fire = MINI_FIRE_FRAMES[frame % len(MINI_FIRE_FRAMES)].strip("\n").splitlines()
    wm = pixel_text(WORDMARK)
    fire_w, wm_w = len(fire[0]), len(wm[0])
    width = max(fire_w, wm_w)

    def centered(row: str, row_w: int) -> str:
        left = (width - row_w) // 2
        return "." * left + row + "." * (width - left - row_w)

    rows = [centered(r, fire_w) for r in fire]
    rows += [centered(r, wm_w) for r in wm]
    # Pad at the top (not bottom) so the wordmark sits flush against the
    # version line below.
    if len(rows) % 2:
        rows.insert(0, "." * width)
    return rows


def read_version() -> str:
    cargo_toml = Path(__file__).resolve().parent.parent / "Cargo.toml"
    match = re.search(r'^version\s*=\s*"([^"]+)"', cargo_toml.read_text(), re.MULTILINE)
    if not match:
        raise RuntimeError(f"could not find version in {cargo_toml}")
    return match.group(1)


def _centered_dim_line(width: int, version: str) -> str:
    pad = max(0, (width - len(version)) // 2)
    return f"{' ' * pad}{DIM}{version}{RESET}"


def render_ansi(
    rows: list[str], overlays: dict[tuple[int, int], str] | None = None
) -> str:
    overlays = overlays or {}
    out_lines = []
    for i in range(0, len(rows), 2):
        top, bot = rows[i], rows[i + 1]
        cell_row = i // 2
        cells = []
        for x, (ct, cb) in enumerate(zip(top, bot)):
            if (cell_row, x) in overlays:
                cells.append(f"{DIM}{overlays[(cell_row, x)]}{RESET}")
                continue
            fg = PALETTE.get(ct)
            bg = PALETTE.get(cb)
            fg_idx = fg[0] if fg else None
            bg_idx = bg[0] if bg else None
            if fg_idx is None and bg_idx is None:
                cells.append(" ")
            elif fg_idx is None:
                cells.append(f"\x1b[38;5;{bg_idx}m▄{RESET}")
            elif bg_idx is None:
                cells.append(f"\x1b[38;5;{fg_idx}m▀{RESET}")
            else:
                cells.append(f"\x1b[38;5;{fg_idx};48;5;{bg_idx}m▀{RESET}")
        out_lines.append("".join(cells))
    return "\n".join(out_lines)


def render_full_with_version() -> str:
    logo_rows = LOGO.strip("\n").splitlines()
    text_rows = pixel_text(WORDMARK)
    gap = 2
    top_pad = (len(logo_rows) - len(text_rows)) // 2
    top_pad += top_pad % 2
    wordmark_col = len(logo_rows[0]) + gap
    version_cell_row = (top_pad + len(text_rows)) // 2
    version = f"v{read_version()}"
    overlays = {(version_cell_row, wordmark_col + i): c for i, c in enumerate(version)}
    return render_ansi(full_rows(), overlays)


def render_wordmark_with_version() -> str:
    rows = wordmark_rows()
    return (
        f"{render_ansi(rows)}\n{_centered_dim_line(len(rows[0]), f'v{read_version()}')}"
    )


def render_wordmark_fire_with_version(frame: int = 0) -> str:
    rows = wordmark_with_fire_rows(frame)
    return (
        f"{render_ansi(rows)}\n{_centered_dim_line(len(rows[0]), f'v{read_version()}')}"
    )


def divider(label: str) -> str:
    width = shutil.get_terminal_size((80, 24)).columns
    prefix = f"── {label} "
    return f"{DIM}{prefix}{'─' * max(0, width - len(prefix))}{RESET}"


# Each entry: (label, render_fn(frame_idx) -> str, frame_count).
# frame_count > 1 means the variant animates when --loop is passed.
VARIANTS = [
    ("full",                lambda _f: render_ansi(full_rows()), 1),
    ("full + version",      lambda _f: render_full_with_version(), 1),
    ("icon",                lambda _f: render_ansi(icon_rows()), 1),
    ("wordmark + version",  lambda _f: render_wordmark_with_version(), 1),
    ("wordmark + fire",     render_wordmark_fire_with_version, len(MINI_FIRE_FRAMES)),
]


def render_block(frame: int) -> str:
    parts = []
    for label, render, _ in VARIANTS:
        parts.append(divider(label))
        parts.append(render(frame))
        parts.append("")
    return "\n".join(parts)


def run_loop(fps: float = 6.0) -> None:
    interval = 1.0 / fps
    sys.stdout.write("\x1b[?25l")  # hide cursor
    sys.stdout.flush()
    try:
        block = render_block(0)
        sys.stdout.write(block)
        sys.stdout.flush()
        if not any(frames > 1 for _, _, frames in VARIANTS):
            return
        line_count = block.count("\n")
        frame = 1
        while True:
            time.sleep(interval)
            sys.stdout.write(f"\x1b[{line_count}A\r")
            sys.stdout.write(render_block(frame))
            sys.stdout.flush()
            frame += 1
    except KeyboardInterrupt:
        pass
    finally:
        sys.stdout.write("\x1b[?25h\n")  # restore cursor
        sys.stdout.flush()


if __name__ == "__main__":
    if "--loop" in sys.argv[1:]:
        run_loop()
    else:
        sys.stdout.write(render_block(0))
