use super::flush::flush_diff;
use super::grid::Grid;
use super::Theme;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::QueueableCommand;
use std::io::Write;

/// Double-buffered terminal renderer. Diffs `current` against `previous`
/// and flushes only changed cells; `force_redraw` triggers a full repaint.
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

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.current.resize(width, height);
        self.previous.resize(width, height);
        self.force_redraw = true;
    }

    /// Render one frame. `paint` writes into `current`. The hardware caret
    /// stays hidden for the lifetime of the app - any visible cursor is
    /// painted into the grid as a styled cell, so it rides the diff atomically
    /// with the rest of the frame and can never flicker through the
    /// intermediate `MoveTo`s that `flush_diff` emits between cell runs.
    pub fn render_with<W: Write, F: FnOnce(&mut Grid, &Theme)>(
        &mut self,
        theme: &Theme,
        w: &mut W,
        paint: F,
    ) -> std::io::Result<()> {
        self.current.clear_all();
        paint(&mut self.current, theme);

        w.queue(BeginSynchronizedUpdate)?;

        if self.force_redraw {
            flush_full(&self.current, w)?;
        } else {
            flush_diff(w, self.current.diff(&self.previous))?;
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

    /// The most recently flushed grid (snapshot harnesses read this after a discard-writer render).
    pub fn previous(&self) -> &Grid {
        &self.previous
    }
}

fn flush_full<W: Write>(grid: &Grid, w: &mut W) -> std::io::Result<()> {
    use super::grid::Style;
    use crossterm::cursor::MoveTo;
    use crossterm::style::{
        Attribute, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    };
    use unicode_width::UnicodeWidthChar;

    let mut current_style = Style::default();
    for y in 0..grid.height() {
        w.queue(MoveTo(0, y))?;
        let mut terminal_col: u16 = 0;
        let mut x = 0u16;
        while x < grid.width() {
            let cell = grid.cell(x, y);
            // `\0` is a wide-char continuation slot - paint a space to
            // keep the cursor in sync rather than emitting a literal NUL.
            let symbol = if cell.symbol == '\0' {
                ' '
            } else {
                cell.symbol
            };
            let cw = UnicodeWidthChar::width(symbol).unwrap_or(1).max(1) as u16;

            // Wide char overflowing the right edge: emit a space to prevent wrapping.
            let (sym, emit_w) = if terminal_col + cw > grid.width() {
                (' ', 1u16)
            } else {
                (symbol, cw)
            };

            if cell.style != current_style {
                w.queue(SetAttribute(Attribute::Reset))?;
                w.queue(ResetColor)?;
                if let Some(fg) = cell.style.fg {
                    w.queue(SetForegroundColor(super::grid::to_crossterm_color(fg)))?;
                }
                if let Some(bg) = cell.style.bg {
                    w.queue(SetBackgroundColor(super::grid::to_crossterm_color(bg)))?;
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
                if cell.style.reverse {
                    w.queue(SetAttribute(Attribute::Reverse))?;
                }
                current_style = cell.style;
            }
            let mut buf = [0u8; 4];
            let s = sym.encode_utf8(&mut buf);
            w.write_all(s.as_bytes())?;

            terminal_col += emit_w;
            // Skip the continuation cell so the grid cursor matches the terminal's visual width.
            x += emit_w;
        }
    }
    w.queue(SetAttribute(Attribute::Reset))?;
    w.queue(ResetColor)?;
    Ok(())
}
