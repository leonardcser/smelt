use super::flush::flush_diff;
use super::grid::Grid;
use super::Theme;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::QueueableCommand;
use std::io::Write;

/// Double-buffered terminal renderer. Diffs `current` against `previous`
/// and flushes only changed cells; `force_redraw` triggers a full repaint.
#[derive(Clone)]
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

    /// Paint a frame into an owned grid without flushing it.
    ///
    /// Separating paint from flush lets callers safely run callbacks against the
    /// completed frame after releasing borrows of the UI that produced it.
    pub fn paint_frame<F: FnOnce(&mut Grid, &Theme)>(&mut self, theme: &Theme, paint: F) -> Grid {
        let mut frame = std::mem::replace(&mut self.current, Grid::new(0, 0));
        if frame.width() != self.width || frame.height() != self.height {
            frame.resize(self.width, self.height);
        } else {
            frame.clear_all();
        }
        paint(&mut frame, theme);
        frame
    }

    /// Flush a frame returned by [`Self::paint_frame`] and recycle its grid.
    pub fn flush_frame<W: Write>(&mut self, w: &mut W, mut frame: Grid) -> std::io::Result<()> {
        let result = (|| {
            w.queue(BeginSynchronizedUpdate)?;

            if self.force_redraw {
                flush_full(&frame, w)?;
            } else {
                flush_diff(w, frame.diff(&self.previous))?;
            }

            if let Some((x, y)) = frame.terminal_cursor_position() {
                w.queue(crossterm::cursor::MoveTo(x, y))?;
            }
            w.queue(EndSynchronizedUpdate)?;
            w.flush()
        })();

        if result.is_ok() {
            frame.swap_with(&mut self.previous);
            self.force_redraw = false;
        } else {
            self.force_redraw = true;
        }
        self.current = frame;
        result
    }

    /// Render one frame. The hardware caret stays hidden for the lifetime of
    /// the app - any visible cursor is painted into the grid, so it rides the
    /// diff atomically with the rest of the frame. Its hidden position is still
    /// restored to the painted caret so terminal-managed preedit text has a
    /// stable anchor.
    pub fn render_with<W: Write, F: FnOnce(&mut Grid, &Theme)>(
        &mut self,
        theme: &Theme,
        w: &mut W,
        paint: F,
    ) -> std::io::Result<()> {
        let frame = self.paint_frame(theme, paint);
        self.flush_frame(w, frame)
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

    let mut current_style = Style::default();
    for y in 0..grid.height() {
        w.queue(MoveTo(0, y))?;
        let mut terminal_col: u16 = 0;
        let mut x = 0u16;
        while x < grid.width() {
            let cell = grid.cell(x, y);
            // A continuation is normally skipped with its leading wide glyph. If
            // one is reached defensively, paint a space rather than a literal NUL.
            let is_printable = !cell.symbol.is_continuation()
                && !cell.symbol.as_str().chars().any(char::is_control);
            let symbol = if is_printable {
                cell.symbol.as_str()
            } else {
                " "
            };
            let width = if is_printable { cell.symbol.width() } else { 1 };

            // A wide grapheme at the right edge would wrap the terminal.
            let (symbol, emit_width) = if terminal_col + width > grid.width() {
                (" ", 1u16)
            } else {
                (symbol, width)
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
            w.write_all(symbol.as_bytes())?;

            terminal_col += emit_width;
            // Skip the continuation cell so the grid cursor matches the terminal's visual width.
            x += emit_width;
        }
    }
    w.queue(SetAttribute(Attribute::Reset))?;
    w.queue(ResetColor)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Style;

    #[test]
    fn staged_paint_and_flush_matches_render_with() {
        let theme = Theme::default();
        let mut direct = Compositor::new(4, 2);
        let mut staged = Compositor::new(4, 2);
        let mut direct_out = Vec::new();
        let mut staged_out = Vec::new();

        direct
            .render_with(&theme, &mut direct_out, |grid, _| {
                grid.set(1, 0, 'x', Style::default());
                grid.set(3, 1, 'y', Style::default());
            })
            .unwrap();
        let frame = staged.paint_frame(&theme, |grid, _| {
            grid.set(1, 0, 'x', Style::default());
            grid.set(3, 1, 'y', Style::default());
        });
        staged.flush_frame(&mut staged_out, frame).unwrap();

        assert_eq!(staged_out, direct_out);
        assert_eq!(staged.previous().cell(1, 0).symbol, 'x');
        assert_eq!(staged.previous().cell(3, 1).symbol, 'y');
    }

    #[test]
    fn flush_restores_the_hidden_terminal_cursor_inside_the_synchronized_update() {
        let theme = Theme::default();
        let mut compositor = Compositor::new(4, 2);
        let mut output = Vec::new();

        compositor
            .render_with(&theme, &mut output, |grid, _| {
                grid.set(0, 0, 'x', Style::default());
                grid.set_terminal_cursor_position(2, 1);
            })
            .unwrap();

        assert!(
            output.ends_with(b"\x1b[2;3H\x1b[?2026l"),
            "cursor must be restored before the frame is displayed: {output:?}"
        );
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected flush failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("injected flush failure"))
        }
    }

    #[test]
    fn failed_flush_recycles_frame_and_forces_full_redraw() {
        let theme = Theme::default();
        let mut compositor = Compositor::new(3, 1);
        let failed = compositor.paint_frame(&theme, |grid, _| {
            grid.set(0, 0, 'x', Style::default());
        });

        assert!(compositor.flush_frame(&mut FailingWriter, failed).is_err());
        assert!(compositor.force_redraw);
        assert_eq!(compositor.previous().cell(0, 0).symbol, ' ');

        let recovered = compositor.paint_frame(&theme, |grid, _| {
            assert_eq!(grid.width(), 3);
            assert_eq!(grid.height(), 1);
            assert_eq!(grid.cell(0, 0).symbol, ' ');
            grid.set(1, 0, 'y', Style::default());
        });
        compositor.flush_frame(&mut Vec::new(), recovered).unwrap();

        assert!(!compositor.force_redraw);
        assert_eq!(compositor.previous().cell(0, 0).symbol, ' ');
        assert_eq!(compositor.previous().cell(1, 0).symbol, 'y');
    }
}
