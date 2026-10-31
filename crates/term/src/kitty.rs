//! Kitty graphics protocol: a process-wide registry of live raster images,
//! Unicode placeholder rows, and tmux passthrough for control sequences.

use smelt_style::image::RasterImage;
use std::collections::HashMap;
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

fn registry() -> &'static Mutex<ImageRegistry> {
    static IMAGES: OnceLock<Mutex<ImageRegistry>> = OnceLock::new();
    IMAGES.get_or_init(|| Mutex::new(ImageRegistry::default()))
}

/// Changes when live image ownership changes, including after released handles are reclaimed.
pub fn image_generation() -> u64 {
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    registry.live_bytes();
    registry.generation
}

pub fn image_exists(id: u32) -> bool {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .images
        .get(&id)
        .and_then(Weak::upgrade)
        .is_some()
}

/// Admit an image into the live-asset budget. The registry holds only weak
/// references; cached rows and painted cells retain the actual image data.
pub fn register_image(
    id: u32,
    png_base64: Arc<str>,
    cols: u16,
    rows: u16,
) -> Option<Arc<RasterImage>> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(id, png_base64, cols, rows)
}

pub const IMAGE_ROWS: usize = 16;

/// Kitty's first sixteen row diacritics, plus the zero-column diacritic.
pub fn placeholder_row(row: usize, cols: u16) -> String {
    const MARKS: [char; IMAGE_ROWS] = [
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
pub fn control_sequence(sequence: &[u8]) -> Vec<u8> {
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

pub(crate) fn transmit_image<W: Write>(
    w: &mut W,
    id: u32,
    image: &RasterImage,
) -> std::io::Result<()> {
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
        w.write_all(&control_sequence(&command))?;
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
        w.write_all(&control_sequence(&command))?;
    }
    Ok(())
}

/// Release an uploaded image's terminal-side data.
pub(crate) fn delete_image<W: Write>(w: &mut W, id: u32) -> std::io::Result<()> {
    let command = format!("\x1b_Ga=d,d=I,i={id},q=2;\x1b\\");
    w.write_all(&control_sequence(command.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tmux_passthrough_doubles_every_escape_in_graphics_commands() {
        assert_eq!(
            tmux_passthrough(b"\x1b_Gi=31,a=q;AAAA\x1b\\"),
            b"\x1bPtmux;\x1b\x1b_Gi=31,a=q;AAAA\x1b\x1b\\\x1b\\"
        );
    }
}
