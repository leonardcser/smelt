use crate::flush::flush_diff;
use crate::grid::Grid;
use crate::theme::Theme;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::QueueableCommand;
use std::io::Write;

/// Double-buffered terminal renderer. Owns the in-flight `current` grid
/// and the previously-flushed `previous` grid; `render_with` lets the
/// caller paint into `current`, then diffs against `previous` and flushes
/// only the changed cells. Resize / Ctrl-L / first frame use a full
/// repaint by setting `force_redraw`.
pub struct Compositor {
    current: Grid,
    previous: Grid,
    width: u16,
    height: u16,
    force_redraw: bool,
}

impl Compositor {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            current: Grid::new(width, height),
            previous: Grid::new(width, height),
            width,
            height,
            force_redraw: true,
        }
    }

    /// Read the most recently flushed grid. Used by in-crate tests
    /// that drive `Ui::render` and want to assert on what landed on
    /// the terminal-bound surface (post-swap, so `previous` carries
    /// the just-rendered frame).
    #[cfg(test)]
    pub(crate) fn previous_for_test(&self) -> &Grid {
        &self.previous
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.current.resize(width, height);
        self.previous.resize(width, height);
        self.force_redraw = true;
    }

    /// Render one frame. The caller paints into the in-flight `current`
    /// grid via `paint`, then optionally returns an absolute `(col, row)`
    /// hardware cursor position. `Ui::render` uses the closure to paint
    /// painted splits + overlays and to surface the focused leaf's
    /// hardware cursor.
    pub fn render_with<W: Write, F: FnOnce(&mut Grid, &Theme) -> Option<(u16, u16)>>(
        &mut self,
        theme: &Theme,
        w: &mut W,
        paint: F,
    ) -> std::io::Result<()> {
        self.current.clear_all();
        let cursor = paint(&mut self.current, theme);

        w.queue(BeginSynchronizedUpdate)?;

        if self.force_redraw {
            flush_full(&self.current, w)?;
        } else {
            flush_diff(w, self.current.diff(&self.previous))?;
        }

        if let Some((x, y)) = cursor {
            w.queue(crossterm::cursor::Show)?;
            w.queue(crossterm::cursor::MoveTo(x, y))?;
        } else {
            w.queue(crossterm::cursor::Hide)?;
        }

        w.queue(EndSynchronizedUpdate)?;
        w.flush()?;

        self.current.swap_with(&mut self.previous);
        self.force_redraw = false;

        Ok(())
    }

    pub fn force_redraw(&mut self) {
        self.force_redraw = true;
    }
}

fn flush_full<W: Write>(grid: &Grid, w: &mut W) -> std::io::Result<()> {
    use crate::grid::Style;
    use crossterm::cursor::MoveTo;
    use crossterm::style::{
        Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    };
    use unicode_width::UnicodeWidthChar;

    let mut current_style = Style::default();
    for y in 0..grid.height() {
        w.queue(MoveTo(0, y))?;
        let mut terminal_col: u16 = 0;
        let mut x = 0u16;
        while x < grid.width() {
            let cell = grid.cell(x, y);
            // `\0` marks the continuation half of a preceding wide
            // char. If the path through the row is aligned it should
            // have been skipped; if we somehow land on one, paint a
            // space so the cursor stays in sync instead of emitting a
            // literal NUL.
            let symbol = if cell.symbol == '\0' {
                ' '
            } else {
                cell.symbol
            };
            let cw = UnicodeWidthChar::width(symbol).unwrap_or(1).max(1) as u16;

            // Wide char whose second cell would fall past the terminal edge:
            // emit a space instead so the terminal doesn't wrap.
            let (sym, emit_w) = if terminal_col + cw > grid.width() {
                (' ', 1u16)
            } else {
                (symbol, cw)
            };

            if cell.style != current_style {
                w.queue(SetAttribute(Attribute::Reset))?;
                w.queue(ResetColor)?;
                if let Some(fg) = cell.style.fg {
                    w.queue(SetForegroundColor(fg))?;
                }
                if let Some(bg) = cell.style.bg {
                    w.queue(SetBackgroundColor(bg))?;
                }
                if cell.style.bold {
                    w.queue(SetAttribute(Attribute::Bold))?;
                }
                if cell.style.dim {
                    w.queue(SetAttribute(Attribute::Dim))?;
                }
                if cell.style.italic {
                    w.queue(SetAttribute(Attribute::Italic))?;
                }
                if cell.style.underline {
                    w.queue(SetAttribute(Attribute::Underlined))?;
                }
                if cell.style.crossedout {
                    w.queue(SetAttribute(Attribute::CrossedOut))?;
                }
                current_style = cell.style;
            }
            let mut buf = [0u8; 4];
            let s = sym.encode_utf8(&mut buf);
            w.queue(Print(s.to_string()))?;

            terminal_col += emit_w;
            // Advance grid by emit_w so wide chars consume their
            // continuation cell — the grid allocates 1 slot per char,
            // so the compositor must skip the next column to stay in
            // sync with the terminal's visual width.
            x += emit_w;
        }
    }
    w.queue(SetAttribute(Attribute::Reset))?;
    w.queue(ResetColor)?;
    Ok(())
}
