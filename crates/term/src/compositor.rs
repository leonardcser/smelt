use super::flush::flush_diff;
use super::grid::Grid;
use super::Theme;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::QueueableCommand;
use smelt_style::image::RasterImage;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Default)]
struct ImageRegistry {
    images: HashMap<u32, Weak<RasterImage>>,
    generation: u64,
}

impl ImageRegistry {
    fn register(
        &mut self,
        id: u32,
        png_base64: Arc<str>,
        cols: u16,
        rows: u16,
    ) -> Option<Arc<RasterImage>> {
        if id == 0
            || id > 0x00ff_ffff
            || cols == 0
            || rows == 0
            || png_base64.len() > 2 * 1024 * 1024
        {
            return None;
        }
        if let Some(image) = self.images.get(&id).and_then(Weak::upgrade) {
            return Some(image);
        }
        let bytes = self.live_bytes();
        if self.images.len() >= 256 || bytes.saturating_add(png_base64.len()) > 32 * 1024 * 1024 {
            return None;
        }
        let image = Arc::new(RasterImage {
            id,
            png_base64,
            cols,
            rows,
        });
        self.images.insert(id, Arc::downgrade(&image));
        self.generation = self.generation.wrapping_add(1);
        Some(image)
    }

    fn live_bytes(&mut self) -> usize {
        let count = self.images.len();
        let mut bytes = 0;
        self.images.retain(|_, weak| {
            if let Some(image) = weak.upgrade() {
                bytes += image.png_base64.len();
                true
            } else {
                false
            }
        });
        if self.images.len() != count {
            self.generation = self.generation.wrapping_add(1);
        }
        bytes
    }
}

fn kitty_images() -> &'static Mutex<ImageRegistry> {
    static IMAGES: OnceLock<Mutex<ImageRegistry>> = OnceLock::new();
    IMAGES.get_or_init(|| Mutex::new(ImageRegistry::default()))
}

/// Changes when live image ownership changes, including after released handles are reclaimed.
pub fn kitty_image_generation() -> u64 {
    let mut registry = kitty_images().lock().unwrap_or_else(|e| e.into_inner());
    registry.live_bytes();
    registry.generation
}

pub fn kitty_image_exists(id: u32) -> bool {
    kitty_images()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .images
        .get(&id)
        .and_then(Weak::upgrade)
        .is_some()
}

/// Admit an image into the live-asset budget. The registry holds only weak
/// references; cached rows and painted cells retain the actual image data.
pub fn register_kitty_image(
    id: u32,
    png_base64: Arc<str>,
    cols: u16,
    rows: u16,
) -> Option<Arc<RasterImage>> {
    kitty_images()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(id, png_base64, cols, rows)
}

pub const KITTY_IMAGE_ROWS: usize = 16;

/// Kitty's first sixteen row diacritics, plus the zero-column diacritic.
pub fn kitty_placeholder_row(row: usize, cols: u16) -> String {
    const MARKS: [char; KITTY_IMAGE_ROWS] = [
        '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}',
        '\u{033f}', '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}',
        '\u{0352}', '\u{0357}',
    ];
    let Some(mark) = MARKS.get(row) else {
        return String::new();
    };
    if cols == 0 {
        return String::new();
    }
    let mut text = String::with_capacity(cols as usize * 4 + 8);
    text.push('\u{10eeee}');
    text.push(*mark);
    text.push(MARKS[0]);
    for _ in 1..cols {
        text.push('\u{10eeee}');
    }
    text
}

/// Wrap an escape sequence for tmux to forward it to the outer terminal.
/// tmux requires every ESC inside its DCS passthrough payload to be doubled.
pub fn kitty_control_sequence(sequence: &[u8]) -> Vec<u8> {
    if std::env::var_os("TMUX").is_none() {
        return sequence.to_vec();
    }
    tmux_passthrough(sequence)
}

fn tmux_passthrough(sequence: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(sequence.len() + 16);
    wrapped.extend_from_slice(b"\x1bPtmux;");
    for &byte in sequence {
        if byte == 0x1b {
            wrapped.push(0x1b);
        }
        wrapped.push(byte);
    }
    wrapped.extend_from_slice(b"\x1b\\");
    wrapped
}

fn transmit_kitty_image<W: Write>(w: &mut W, id: u32, image: &RasterImage) -> std::io::Result<()> {
    let mut chunks = image.png_base64.as_bytes().chunks(4096).peekable();
    if let Some(first) = chunks.next() {
        let mut command = Vec::with_capacity(first.len() + 90);
        write!(
            command,
            "\x1b_Ga=T,f=100,q=2,U=1,i={id},c={},r={},m={};",
            image.cols,
            image.rows,
            u8::from(chunks.peek().is_some())
        )?;
        command.extend_from_slice(first);
        command.extend_from_slice(b"\x1b\\");
        w.write_all(&kitty_control_sequence(&command))?;
    }
    while let Some(chunk) = chunks.next() {
        let mut command = Vec::with_capacity(chunk.len() + 32);
        write!(
            command,
            "\x1b_Gm={},q=2;",
            u8::from(chunks.peek().is_some())
        )?;
        command.extend_from_slice(chunk);
        command.extend_from_slice(b"\x1b\\");
        w.write_all(&kitty_control_sequence(&command))?;
    }
    Ok(())
}

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
                let command = format!("\x1b_Ga=d,d=I,i={id},q=2;\x1b\\");
                w.write_all(&kitty_control_sequence(command.as_bytes()))?;
            }
            for id in &uploads {
                transmit_kitty_image(w, *id, &images[id])?;
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

    fn registry_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn image_budget_reclaims_dropped_document_and_frame_handles() {
        let mut registry = ImageRegistry::default();
        let retained: Vec<_> = (1..=256)
            .map(|id| registry.register(id, "QUJDRA==".into(), 1, 1).unwrap())
            .collect();
        assert_eq!(registry.generation, 256);
        assert!(registry.register(257, "QUJDRA==".into(), 1, 1).is_none());
        drop(retained);
        let replacement = registry.register(257, "QUJDRA==".into(), 1, 1).unwrap();
        assert_eq!(registry.generation, 258);
        assert_eq!(registry.images.len(), 1);
        assert!(registry.images.get(&257).and_then(Weak::upgrade).is_some());
        drop(replacement);
        assert!(registry.images.get(&257).and_then(Weak::upgrade).is_none());
    }

    #[test]
    fn unowned_placeholder_is_not_mistaken_for_a_registered_image() {
        let mut compositor = Compositor::new(2, 1);
        let mut output = Vec::new();
        compositor
            .render_with(&Theme::default(), &mut output, |grid, _| {
                grid.set_symbol(
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
    fn tmux_passthrough_doubles_every_escape_in_graphics_commands() {
        assert_eq!(
            tmux_passthrough(b"\x1b_Gi=31,a=q;AAAA\x1b\\"),
            b"\x1bPtmux;\x1b\x1b_Gi=31,a=q;AAAA\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn kitty_image_reappears_after_scrolling_out_of_view() {
        let _lock = registry_test_lock();
        let id = 0x010204;
        let image = register_kitty_image(id, "QUJDRA==".into(), 2, 2).unwrap();
        let style = Style::new().fg(crate::Color::Rgb { r: 1, g: 2, b: 4 });
        let theme = Theme::default();
        let mut compositor = Compositor::new(4, 2);
        let mut first = Vec::new();
        compositor
            .render_with(&theme, &mut first, |grid, _| {
                grid.set_image_symbol(
                    0,
                    0,
                    "\u{10eeee}\u{030d}\u{0305}",
                    style,
                    Arc::clone(&image),
                );
            })
            .unwrap();
        assert!(first.windows(4).any(|window| window == b"a=T,"));

        compositor
            .render_with(&theme, &mut Vec::new(), |_, _| {})
            .unwrap();
        let mut returned = Vec::new();
        compositor
            .render_with(&theme, &mut returned, |grid, _| {
                grid.set_image_symbol(
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
        let image = register_kitty_image(id, "QUJDRA==".into(), 2, 2).unwrap();
        let style = Style::new().fg(crate::Color::Rgb { r: 1, g: 2, b: 3 });
        let theme = Theme::default();
        let mut compositor = Compositor::new(4, 2);
        let mut first = Vec::new();
        compositor
            .render_with(&theme, &mut first, |grid, _| {
                grid.set_image_symbol(
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
            .render_with(&theme, &mut scrolled, |grid, _| {
                grid.set_image_symbol(
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
            .render_with(&theme, &mut restored, |grid, _| {
                grid.set_image_symbol(
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
        let image = register_kitty_image(id, "QUJDRA==".into(), 1, 1).unwrap();
        let style = Style::new().fg(crate::Color::Rgb {
            r: 10,
            g: 11,
            b: 12,
        });
        let theme = Theme::default();
        let mut compositor = Compositor::new(2, 1);
        compositor
            .render_with(&theme, &mut Vec::new(), |grid, _| {
                grid.set_image_symbol(
                    0,
                    0,
                    "\u{10eeee}\u{0305}\u{0305}",
                    style,
                    Arc::clone(&image),
                );
            })
            .unwrap();
        assert!(kitty_image_exists(id));
        let mut cleared = Vec::new();
        compositor
            .render_with(&theme, &mut cleared, |_, _| {})
            .unwrap();
        assert!(kitty_image_exists(id));
        assert!(cleared
            .windows(b"a=d,d=I,i=658188,q=2;".len())
            .any(|w| w == b"a=d,d=I,i=658188,q=2;"));
        assert!(compositor.uploaded_images.is_empty());
    }

    #[test]
    fn staged_paint_and_flush_matches_render_with() {
        let _lock = registry_test_lock();
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
        let _lock = registry_test_lock();
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
        let _lock = registry_test_lock();
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
