use super::flush::flush_diff;
use super::grid::Grid;
use super::kitty;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::QueueableCommand;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

/// Most grids a frame holds at once: the composed frame and its presented copy.
const MAX_SPARE_GRIDS: usize = 2;

/// Double-buffered terminal renderer. Diffs each frame against `previous`
/// and flushes only changed cells, scrolling the terminal first when whole
/// rows moved; `force_redraw` triggers a full repaint.
#[derive(Clone)]
pub struct Compositor {
    /// Grids handed back after a frame, reused so frames do not allocate.
    spare: Vec<Grid>,
    previous: Grid,
    width: u16,
    height: u16,
    force_redraw: bool,
    uploaded_images: HashSet<u32>,
}

impl Compositor {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            spare: Vec::new(),
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
        self.previous.resize(width, height);
        self.force_redraw = true;
    }

    /// Hand out a cleared grid at the current size for the next frame.
    ///
    /// The caller owns the grid while painting, so paint phases can release and
    /// reacquire other borrows freely. Pass it back to [`Self::flush_frame`].
    pub fn begin_frame(&mut self) -> Grid {
        let mut frame = self.take_spare();
        if frame.width() != self.width || frame.height() != self.height {
            frame.resize(self.width, self.height);
        } else {
            frame.clear_all();
        }
        frame
    }

    /// Hand out a copy of `source` for the next frame, in a reused grid.
    pub fn begin_copy(&mut self, source: &Grid) -> Grid {
        let mut frame = self.take_spare();
        frame.copy_from(source);
        frame
    }

    /// Return a grid from [`Self::begin_frame`] that will not be flushed.
    pub fn recycle(&mut self, grid: Grid) {
        if self.spare.len() < MAX_SPARE_GRIDS {
            self.spare.push(grid);
        }
    }

    fn take_spare(&mut self) -> Grid {
        self.spare.pop().unwrap_or_else(|| Grid::new(0, 0))
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
                // Images do not move with scrolled text in every terminal.
                if let Some(scroll) = images
                    .is_empty()
                    .then(|| find_scroll(&frame, &self.previous))
                    .flatten()
                {
                    scroll.queue(w)?;
                    self.previous
                        .scroll_rows(scroll.top, scroll.bottom, scroll.dy);
                }
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
        self.recycle(frame);
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

/// Rows `top..bottom` whose content moved up by `dy` rows, down when negative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scroll {
    top: u16,
    bottom: u16,
    dy: i16,
}

impl Scroll {
    /// Scrolls the terminal's copy of the rows. Rows that move in take the
    /// current background, so the style is reset first. Resetting the margins
    /// also homes the cursor, so the diff that follows positions it again.
    fn queue<W: Write>(self, w: &mut W) -> std::io::Result<()> {
        let rows = self.dy.unsigned_abs();
        let direction = if self.dy > 0 { 'S' } else { 'T' };
        write!(
            w,
            "\x1b[0m\x1b[{};{}r\x1b[{rows}{direction}\x1b[r",
            self.top + 1,
            self.bottom
        )
    }
}

/// The scroll that spares the diff the most changed cells, when that is at
/// least a row's worth. A candidate is a run of rows that equal the rows `dy`
/// away in `previous`, as when the transcript moves by whole lines. Rows are
/// matched by hash: a collision costs cells, not correctness, because the diff
/// that follows repairs every cell the scroll got wrong.
fn find_scroll(frame: &Grid, previous: &Grid) -> Option<Scroll> {
    let (width, height) = (frame.width(), frame.height());
    if width != previous.width() || height != previous.height() {
        return None;
    }
    let changed: Vec<usize> = (0..height)
        .map(|y| {
            frame
                .row(y)
                .iter()
                .zip(previous.row(y))
                .filter(|(a, b)| a != b)
                .count()
        })
        .collect();
    let mut best_saved = usize::from(width);
    if changed.iter().sum::<usize>() <= best_saved {
        return None;
    }
    let hashes = |grid: &Grid| (0..height).map(|y| grid.row_hash(y)).collect::<Vec<_>>();
    let (now, before) = (hashes(frame), hashes(previous));
    let mut best = None;
    for distance in 1..height as i16 {
        for dy in [distance, -distance] {
            let mut y = 0;
            while y < height {
                let moved = |y: u16| {
                    let source = y as i16 + dy;
                    (0..height as i16).contains(&source)
                        && now[y as usize] == before[source as usize]
                };
                if !moved(y) {
                    y += 1;
                    continue;
                }
                let start = y;
                while y < height && moved(y) {
                    y += 1;
                }
                let saved: usize = changed[start as usize..y as usize].iter().sum();
                if saved > best_saved {
                    best_saved = saved;
                    best = Some(if dy > 0 {
                        Scroll {
                            top: start,
                            bottom: y + dy as u16,
                            dy,
                        }
                    } else {
                        Scroll {
                            top: start - dy.unsigned_abs(),
                            bottom: y,
                            dy,
                        }
                    });
                }
            }
        }
    }
    best
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

    /// Renders each frame of `rows` per line through a compositor into a
    /// terminal emulator; returns what each frame wrote and the final screen.
    fn present(frames: &[Vec<&str>]) -> (Vec<Vec<u8>>, Vec<String>) {
        let (width, height) = (12, frames[0].len() as u16);
        let mut compositor = Compositor::new(width, height);
        let size = |n: u16| std::num::NonZeroU16::new(n).unwrap();
        let mut terminal = vt100::Parser::new(size(height), size(width), 0);
        let mut writes = Vec::new();
        for rows in frames {
            let mut output = Vec::new();
            compositor
                .render_with(&mut output, |grid| {
                    for (y, text) in rows.iter().enumerate() {
                        grid.as_mut_slice()
                            .put_str(0, y as u16, text, Style::default());
                    }
                })
                .unwrap();
            terminal.process(&output);
            writes.push(output);
        }
        let screen = terminal
            .screen()
            .rows(0, width)
            .map(|row| row.trim_end().to_owned())
            .collect();
        (writes, screen)
    }

    #[test]
    fn text_moving_by_whole_rows_scrolls_and_draws_only_new_rows() {
        let _lock = registry_test_lock();
        let before = vec!["first", "second", "third", "fourth", "fifth", "> prompt"];
        let after = vec!["third", "fourth", "fifth", "sixth", "seventh", "> prompt"];
        let (writes, screen) = present(&[before, after.clone()]);
        assert_eq!(screen, after);
        let scrolled = String::from_utf8_lossy(&writes[1]);
        assert!(scrolled.contains("\x1b[1;5r\x1b[2S\x1b[r"), "{scrolled:?}");
        assert!(!scrolled.contains("third") && !scrolled.contains("prompt"));
    }

    #[test]
    fn text_moving_down_scrolls_back() {
        let _lock = registry_test_lock();
        let before = vec!["third", "fourth", "fifth", "sixth", "seventh", "> prompt"];
        let after = vec!["first", "second", "third", "fourth", "fifth", "> prompt"];
        let (writes, screen) = present(&[before, after.clone()]);
        assert_eq!(screen, after);
        assert!(String::from_utf8_lossy(&writes[1]).contains("\x1b[1;5r\x1b[2T\x1b[r"));
    }

    #[test]
    fn unrelated_rows_do_not_scroll() {
        let _lock = registry_test_lock();
        let before = vec!["alpha", "beta", "gamma", "delta"];
        let after = vec!["one", "two", "three", "four"];
        let (writes, screen) = present(&[before, after.clone()]);
        assert_eq!(screen, after);
        assert!(!String::from_utf8_lossy(&writes[1]).contains("r\x1b["));
    }

    #[test]
    fn scrolled_frames_show_what_a_full_redraw_shows() {
        let _lock = registry_test_lock();
        let (width, height) = (10u16, 8u16);
        let size = |n: u16| std::num::NonZeroU16::new(n).unwrap();
        let cells = |parser: &vt100::Parser| {
            let screen = parser.screen();
            (0..height)
                .flat_map(|y| (0..width).map(move |x| (y, x)))
                .map(|(y, x)| {
                    let cell = screen.cell(y, x).unwrap();
                    // A scrolled-in cell is empty; a redrawn one holds a space.
                    let text = Some(cell.contents()).filter(|t| !t.is_empty());
                    (text.unwrap_or(" ").to_owned(), cell.bgcolor(), cell.bold())
                })
                .collect::<Vec<_>>()
        };
        let mut seed = 7u64;
        let mut next = |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut rows: Vec<u64> = (0..height as u64).collect();
        let mut compositor = Compositor::new(width, height);
        let mut terminal = vt100::Parser::new(size(height), size(width), 0);
        let mut scrolled = 0;
        for frame in 0..400 {
            let top = next(height as u64 - 1) as usize;
            let bottom = top + 1 + next((height as usize - top) as u64) as usize;
            let dy = 1 + next((bottom - top) as u64) as usize;
            match next(4) {
                0 => rows[top..bottom].rotate_left(dy.min(bottom - top)),
                1 => rows[top..bottom].rotate_right(dy.min(bottom - top)),
                _ => {}
            }
            rows[next(height as u64) as usize] = 100 + frame;
            let paint = |grid: &mut Grid| {
                for (y, row) in rows.iter().enumerate() {
                    let style = match row % 3 {
                        0 => Style::new().bg(crate::Color::AnsiValue(4)),
                        1 => Style::new().bold(),
                        _ => Style::default(),
                    };
                    grid.as_mut_slice().put_str(
                        0,
                        y as u16,
                        &format!("r{row} {}", row * 7919),
                        style,
                    );
                }
            };
            let mut output = Vec::new();
            compositor.render_with(&mut output, paint).unwrap();
            terminal.process(&output);
            scrolled += output.windows(3).any(|w| w == b"\x1b[r") as usize;
            let mut redraw = vt100::Parser::new(size(height), size(width), 0);
            let mut full = Vec::new();
            Compositor::new(width, height)
                .render_with(&mut full, paint)
                .unwrap();
            redraw.process(&full);
            assert_eq!(cells(&terminal), cells(&redraw), "frame {frame}");
        }
        assert!(scrolled > 50, "only {scrolled} frames scrolled");
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
