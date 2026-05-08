//! Frame snapshot for storybook tests. Captures text and per-cell style
//! so tests can assert on rendered state without parsing SGR escapes.

use super::grid::{Cell, Grid, Style};

/// Structured copy of one rendered frame. Wide-char continuation cells
/// (`\0`) collapse to a space in `rows`.
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

    /// Rows joined by `\n` with trailing whitespace stripped per row.
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

    /// Style sidecar as one line per non-default styled run.
    /// Adjacent equal-style cells are merged. Format: `row col len fg=… bg=… attrs=…`.
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
