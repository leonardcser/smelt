//! Frame snapshot for renderer storybook tests. Captures the text +
//! per-cell style sidecar of a rendered grid so snapshot tests can
//! assert on the post-render state without parsing SGR escapes back
//! from a writer.

use super::grid::{Cell, Grid, Style};

/// Structured copy of one rendered frame. `rows` is the visible
/// glyph grid (one `String` per row, full-width); `styles` carries
/// per-cell style for the same coordinates. Cells flagged as wide-char
/// continuation slots (`\0` sentinel) collapse to a space in `rows`.
#[derive(Clone, Debug)]
pub struct SnapshotFrame {
    pub width: u16,
    pub height: u16,
    pub rows: Vec<String>,
    pub styles: Vec<Vec<Style>>,
}

impl SnapshotFrame {
    pub fn from_grid(grid: &Grid) -> Self {
        let w = grid.width();
        let h = grid.height();
        let mut rows = Vec::with_capacity(h as usize);
        let mut styles = Vec::with_capacity(h as usize);
        for y in 0..h {
            let mut row = String::with_capacity(w as usize);
            let mut row_styles = Vec::with_capacity(w as usize);
            for x in 0..w {
                let cell: &Cell = grid.cell(x, y);
                let ch = if cell.symbol == '\0' {
                    ' '
                } else {
                    cell.symbol
                };
                row.push(ch);
                row_styles.push(cell.style);
            }
            rows.push(row);
            styles.push(row_styles);
        }
        Self {
            width: w,
            height: h,
            rows,
            styles,
        }
    }

    /// Plain-text view: rows joined by `\n`, trailing whitespace
    /// stripped per row so snapshots are diff-friendly.
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.rows.iter().map(|r| r.len() + 1).sum());
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(row.trim_end());
        }
        out
    }

    /// Style sidecar serialized as one line per styled run. Runs
    /// collapse adjacent cells with identical style; default-styled
    /// runs are omitted. Format: `row col len fg=… bg=… attrs=…`.
    /// Stable ordering (row, col) for diff stability.
    pub fn styles_text(&self) -> String {
        let mut out = String::new();
        for (y, row) in self.styles.iter().enumerate() {
            let mut x = 0usize;
            while x < row.len() {
                let s = row[x];
                if s == Style::default() {
                    x += 1;
                    continue;
                }
                let mut end = x + 1;
                while end < row.len() && row[end] == s {
                    end += 1;
                }
                let len = end - x;
                out.push_str(&format!("{y:>3} {x:>3} {len:>3} {}\n", fmt_style(&s)));
                x = end;
            }
        }
        out
    }
}

fn fmt_style(s: &Style) -> String {
    let mut parts = Vec::new();
    if let Some(fg) = s.fg {
        parts.push(format!("fg={fg:?}"));
    }
    if let Some(bg) = s.bg {
        parts.push(format!("bg={bg:?}"));
    }
    let mut attrs = Vec::new();
    if s.bold {
        attrs.push("bold");
    }
    if s.dim {
        attrs.push("dim");
    }
    if s.italic {
        attrs.push("italic");
    }
    if s.underline {
        attrs.push("underline");
    }
    if s.crossedout {
        attrs.push("crossedout");
    }
    if !attrs.is_empty() {
        parts.push(format!("attrs={}", attrs.join("|")));
    }
    parts.join(" ")
}
