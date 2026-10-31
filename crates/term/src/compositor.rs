use super::flush::flush_diff;
use super::grid::Grid;
use super::kitty;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::QueueableCommand;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

/// Double-buffered terminal renderer. Diffs `current` against `previous`
/// and flushes only changed cells; `force_redraw` triggers a full repaint.
#[derive(Clone)]
pub struct Compositor {
    current: Grid,
    previous: Grid,
    width: u16,
    height: u16,
    force_redraw: bool,
    uploaded_images: HashSet<u32>,
}

impl Compositor {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            current: Grid::new(width, height),
            previous: Grid::new(width, height),
            width,
            height,
            force_redraw: true,
            uploaded_images: HashSet::new(),
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.current.resize(width, height);
        self.previous.resize(width, height);
        self.force_redraw = true;
    }

    /// Hand out a cleared grid at the current size for the next frame.
    ///
    /// The caller owns the grid while painting, so paint phases can release and
    /// reacquire other borrows freely. Pass it back to [`Self::flush_frame`].
    pub fn begin_frame(&mut self) -> Grid {
        let mut frame = std::mem::replace(&mut self.current, Grid::new(0, 0));
        if frame.width() != self.width || frame.height() != self.height {
            frame.resize(self.width, self.height);
        } else {
            frame.clear_all();
        }
        frame
    }

    /// Flush a frame from [`Self::begin_frame`] and recycle its grid.
    pub fn flush_frame<W: Write>(&mut self, w: &mut W, mut frame: Grid) -> std::io::Result<()> {
        let mut images = HashMap::new();
        for y in 0..frame.height() {
            for x in 0..frame.width() {
                if let Some(image) = &frame.cell(x, y).image {
                    images.entry(image.id).or_insert_with(|| Arc::clone(image));
                }
            }
        }
        let visible_images: HashSet<u32> = images.keys().copied().collect();
        let uploads: Vec<u32> = visible_images
            .iter()
            .copied()
            .filter(|id| self.force_redraw || !self.uploaded_images.contains(id))
            .collect();
        let removed: Vec<u32> = self
            .uploaded_images
            .difference(&visible_images)
            .copied()
            .collect();
        let result = (|| {
            w.queue(BeginSynchronizedUpdate)?;
            for id in &removed {
                // Offscreen images are retransmitted when needed, so release their terminal data.
                kitty::delete_image(w, *id)?;
            }
            for id in &uploads {
                kitty::transmit_image(w, *id, &images[id])?;
            }

            if self.force_redraw || !uploads.is_empty() || !removed.is_empty() {
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
            self.uploaded_images = visible_images;
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
    pub fn render_with<W: Write, F: FnOnce(&mut Grid)>(
        &mut self,
        w: &mut W,
        paint: F,
    ) -> std::io::Result<()> {
        let mut frame = self.begin_frame();
        paint(&mut frame);
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
    use super::color_depth::color_depth;
    use super::grid::Style;
    use crossterm::cursor::MoveTo;
    use crossterm::style::{
        Attribute, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    };

    let depth = color_depth();
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
                    w.queue(SetForegroundColor(super::grid::to_crossterm_color(
                        fg, depth,
                    )))?;
                }
                if let Some(bg) = cell.style.bg {
                    w.queue(SetBackgroundColor(super::grid::to_crossterm_color(
                        bg, depth,
                    )))?;
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
    use std::sync::{Mutex, OnceLock};

    fn registry_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn unowned_placeholder_is_not_mistaken_for_a_registered_image() {
        let mut compositor = Compositor::new(2, 1);
        let mut output = Vec::new();
        compositor
            .render_with(&mut output, |grid| {
                grid.as_mut_slice().set_symbol(
                    0,
                    0,
                    "\u{10eeee}\u{0305}\u{0305}",
                    Style::new().fg(crate::Color::Rgb { r: 1, g: 2, b: 3 }),
                );
            })
            .unwrap();
        assert!(!output.windows(4).any(|window| window == b"a=T,"));
    }

    #[test]
    fn kitty_image_reappears_after_scrolling_out_of_view() {
        let _lock = registry_test_lock();
        let id = 0x010204;
        let image = kitty::register_image(id, "QUJDRA==".into(), 2, 2).unwrap();
        let style = Style::new().fg(crate::Color::Rgb { r: 1, g: 2, b: 4 });
        let mut compositor = Compositor::new(4, 2);
        let mut first = Vec::new();
        compositor
            .render_with(&mut first, |grid| {
                grid.as_mut_slice().set_image_symbol(
                    0,
                    0,
                    "\u{10eeee}\u{030d}\u{0305}",
                    style,
                    Arc::clone(&image),
                );
            })
            .unwrap();
        assert!(first.windows(4).any(|window| window == b"a=T,"));

        compositor.render_with(&mut Vec::new(), |_| {}).unwrap();
        let mut returned = Vec::new();
        compositor
            .render_with(&mut returned, |grid| {
                grid.as_mut_slice().set_image_symbol(
                    0,
                    0,
                    "\u{10eeee}\u{030d}\u{0305}",
                    style,
                    Arc::clone(&image),
                );
            })
            .unwrap();
        assert!(returned.windows(4).any(|window| window == b"a=T,"));
    }

    #[test]
    fn kitty_image_moves_with_virtual_viewport_and_reuploads_on_redraw() {
        let _lock = registry_test_lock();
        let id = 0x010203;
        let image = kitty::register_image(id, "QUJDRA==".into(), 2, 2).unwrap();
        let style = Style::new().fg(crate::Color::Rgb { r: 1, g: 2, b: 3 });
        let mut compositor = Compositor::new(4, 2);
        let mut first = Vec::new();
        compositor
            .render_with(&mut first, |grid| {
                grid.as_mut_slice().set_image_symbol(
                    0,
                    1,
                    "\u{10eeee}\u{030d}\u{0305}",
                    style,
                    Arc::clone(&image),
                );
            })
            .unwrap();
        assert!(first
            .windows(b"a=T,f=100,q=2,U=1,i=66051,c=2,r=2,m=0;QUJDRA==".len())
            .any(|window| window == b"a=T,f=100,q=2,U=1,i=66051,c=2,r=2,m=0;QUJDRA=="));

        let mut scrolled = Vec::new();
        compositor
            .render_with(&mut scrolled, |grid| {
                grid.as_mut_slice().set_image_symbol(
                    0,
                    0,
                    "\u{10eeee}\u{030d}\u{0305}",
                    style,
                    Arc::clone(&image),
                );
            })
            .unwrap();
        assert!(!scrolled.windows(4).any(|window| window == b"a=T,"));
        assert_eq!(
            compositor.previous().cell(0, 0).symbol.as_str(),
            "\u{10eeee}\u{030d}\u{0305}"
        );
        assert_eq!(compositor.previous().cell(0, 1).symbol, ' ');

        compositor.force_redraw();
        let mut restored = Vec::new();
        compositor
            .render_with(&mut restored, |grid| {
                grid.as_mut_slice().set_image_symbol(
                    0,
                    0,
                    "\u{10eeee}\u{030d}\u{0305}",
                    style,
                    Arc::clone(&image),
                );
            })
            .unwrap();
        assert!(restored.windows(4).any(|window| window == b"a=T,"));
    }

    #[test]
    fn offscreen_image_frees_terminal_data_but_retains_raster_handle() {
        let _lock = registry_test_lock();
        let id = 0x0a0b0c;
        let image = kitty::register_image(id, "QUJDRA==".into(), 1, 1).unwrap();
        let style = Style::new().fg(crate::Color::Rgb {
            r: 10,
            g: 11,
            b: 12,
        });
        let mut compositor = Compositor::new(2, 1);
        compositor
            .render_with(&mut Vec::new(), |grid| {
                grid.as_mut_slice().set_image_symbol(
                    0,
                    0,
                    "\u{10eeee}\u{0305}\u{0305}",
                    style,
                    Arc::clone(&image),
                );
            })
            .unwrap();
        assert!(kitty::image_exists(id));
        let mut cleared = Vec::new();
        compositor.render_with(&mut cleared, |_| {}).unwrap();
        assert!(kitty::image_exists(id));
        assert!(cleared
            .windows(b"a=d,d=I,i=658188,q=2;".len())
            .any(|w| w == b"a=d,d=I,i=658188,q=2;"));
        assert!(compositor.uploaded_images.is_empty());
    }

    #[test]
    fn staged_paint_and_flush_matches_render_with() {
        let _lock = registry_test_lock();
        let mut direct = Compositor::new(4, 2);
        let mut staged = Compositor::new(4, 2);
        let mut direct_out = Vec::new();
        let mut staged_out = Vec::new();

        direct
            .render_with(&mut direct_out, |grid| {
                grid.as_mut_slice().set(1, 0, 'x', Style::default());
                grid.as_mut_slice().set(3, 1, 'y', Style::default());
            })
            .unwrap();
        let mut frame = staged.begin_frame();
        frame.as_mut_slice().set(1, 0, 'x', Style::default());
        frame.as_mut_slice().set(3, 1, 'y', Style::default());
        staged.flush_frame(&mut staged_out, frame).unwrap();

        assert_eq!(staged_out, direct_out);
        assert_eq!(staged.previous().cell(1, 0).symbol, 'x');
        assert_eq!(staged.previous().cell(3, 1).symbol, 'y');
    }

    #[test]
    fn flush_restores_the_hidden_terminal_cursor_inside_the_synchronized_update() {
        let _lock = registry_test_lock();
        let mut compositor = Compositor::new(4, 2);
        let mut output = Vec::new();

        compositor
            .render_with(&mut output, |grid| {
                grid.as_mut_slice().set(0, 0, 'x', Style::default());
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
        let _lock = registry_test_lock();
        let mut compositor = Compositor::new(3, 1);
        let mut failed = compositor.begin_frame();
        failed.as_mut_slice().set(0, 0, 'x', Style::default());

        assert!(compositor.flush_frame(&mut FailingWriter, failed).is_err());
        assert!(compositor.force_redraw);
        assert_eq!(compositor.previous().cell(0, 0).symbol, ' ');

        let mut recovered = compositor.begin_frame();
        assert_eq!(recovered.width(), 3);
        assert_eq!(recovered.height(), 1);
        assert_eq!(recovered.cell(0, 0).symbol, ' ');
        recovered.as_mut_slice().set(1, 0, 'y', Style::default());
        compositor.flush_frame(&mut Vec::new(), recovered).unwrap();

        assert!(!compositor.force_redraw);
        assert_eq!(compositor.previous().cell(0, 0).symbol, ' ');
        assert_eq!(compositor.previous().cell(1, 0).symbol, 'y');
    }
}
