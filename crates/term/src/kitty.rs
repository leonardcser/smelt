//! Kitty graphics protocol: a process-wide registry of live raster images,
//! Unicode placeholder rows, and tmux passthrough for control sequences.

use smelt_style::image::RasterImage;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
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

/// Allocate an id for a new image. Ids are 24-bit because the placeholder
/// foreground color carries them, and one counter serves every image source so
/// registry entries never collide.
pub fn next_image_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
        Some(if id >= 0x00ff_ffff { 1 } else { id + 1 })
    })
    .unwrap_or(1)
}

/// Kitty's row and column diacritics (`gen/rowcolumn-diacritics.txt` in the
/// kitty repository). The n-th mark encodes row or column n, so an image can
/// span at most this many placeholder rows.
const DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030d}',
    '\u{030e}',
    '\u{0310}',
    '\u{0312}',
    '\u{033d}',
    '\u{033e}',
    '\u{033f}',
    '\u{0346}',
    '\u{034a}',
    '\u{034b}',
    '\u{034c}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035b}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036a}',
    '\u{036b}',
    '\u{036c}',
    '\u{036d}',
    '\u{036e}',
    '\u{036f}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059c}',
    '\u{059d}',
    '\u{059e}',
    '\u{059f}',
    '\u{05a0}',
    '\u{05a1}',
    '\u{05a8}',
    '\u{05a9}',
    '\u{05ab}',
    '\u{05ac}',
    '\u{05af}',
    '\u{05c4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065a}',
    '\u{065b}',
    '\u{065d}',
    '\u{065e}',
    '\u{06d6}',
    '\u{06d7}',
    '\u{06d8}',
    '\u{06d9}',
    '\u{06da}',
    '\u{06db}',
    '\u{06dc}',
    '\u{06df}',
    '\u{06e0}',
    '\u{06e1}',
    '\u{06e2}',
    '\u{06e4}',
    '\u{06e7}',
    '\u{06e8}',
    '\u{06eb}',
    '\u{06ec}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073a}',
    '\u{073d}',
    '\u{073f}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074a}',
    '\u{07eb}',
    '\u{07ec}',
    '\u{07ed}',
    '\u{07ee}',
    '\u{07ef}',
    '\u{07f0}',
    '\u{07f1}',
    '\u{07f3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081b}',
    '\u{081c}',
    '\u{081d}',
    '\u{081e}',
    '\u{081f}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082a}',
    '\u{082b}',
    '\u{082c}',
    '\u{082d}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0f82}',
    '\u{0f83}',
    '\u{0f86}',
    '\u{0f87}',
    '\u{135d}',
    '\u{135e}',
    '\u{135f}',
    '\u{17dd}',
    '\u{193a}',
    '\u{1a17}',
    '\u{1a75}',
    '\u{1a76}',
    '\u{1a77}',
    '\u{1a78}',
    '\u{1a79}',
    '\u{1a7a}',
    '\u{1a7b}',
    '\u{1a7c}',
    '\u{1b6b}',
    '\u{1b6d}',
    '\u{1b6e}',
    '\u{1b6f}',
    '\u{1b70}',
    '\u{1b71}',
    '\u{1b72}',
    '\u{1b73}',
    '\u{1cd0}',
    '\u{1cd1}',
    '\u{1cd2}',
    '\u{1cda}',
    '\u{1cdb}',
    '\u{1ce0}',
    '\u{1dc0}',
    '\u{1dc1}',
    '\u{1dc3}',
    '\u{1dc4}',
    '\u{1dc5}',
    '\u{1dc6}',
    '\u{1dc7}',
    '\u{1dc8}',
    '\u{1dc9}',
    '\u{1dcb}',
    '\u{1dcc}',
    '\u{1dd1}',
    '\u{1dd2}',
    '\u{1dd3}',
    '\u{1dd4}',
    '\u{1dd5}',
    '\u{1dd6}',
    '\u{1dd7}',
    '\u{1dd8}',
    '\u{1dd9}',
    '\u{1dda}',
    '\u{1ddb}',
    '\u{1ddc}',
    '\u{1ddd}',
    '\u{1dde}',
    '\u{1ddf}',
    '\u{1de0}',
    '\u{1de1}',
    '\u{1de2}',
    '\u{1de3}',
    '\u{1de4}',
    '\u{1de5}',
    '\u{1de6}',
    '\u{1dfe}',
    '\u{20d0}',
    '\u{20d1}',
    '\u{20d4}',
    '\u{20d5}',
    '\u{20d6}',
    '\u{20d7}',
    '\u{20db}',
    '\u{20dc}',
    '\u{20e1}',
    '\u{20e7}',
    '\u{20e9}',
    '\u{20f0}',
    '\u{2cef}',
    '\u{2cf0}',
    '\u{2cf1}',
    '\u{2de0}',
    '\u{2de1}',
    '\u{2de2}',
    '\u{2de3}',
    '\u{2de4}',
    '\u{2de5}',
    '\u{2de6}',
    '\u{2de7}',
    '\u{2de8}',
    '\u{2de9}',
    '\u{2dea}',
    '\u{2deb}',
    '\u{2dec}',
    '\u{2ded}',
    '\u{2dee}',
    '\u{2def}',
    '\u{2df0}',
    '\u{2df1}',
    '\u{2df2}',
    '\u{2df3}',
    '\u{2df4}',
    '\u{2df5}',
    '\u{2df6}',
    '\u{2df7}',
    '\u{2df8}',
    '\u{2df9}',
    '\u{2dfa}',
    '\u{2dfb}',
    '\u{2dfc}',
    '\u{2dfd}',
    '\u{2dfe}',
    '\u{2dff}',
    '\u{a66f}',
    '\u{a67c}',
    '\u{a67d}',
    '\u{a6f0}',
    '\u{a6f1}',
    '\u{a8e0}',
    '\u{a8e1}',
    '\u{a8e2}',
    '\u{a8e3}',
    '\u{a8e4}',
    '\u{a8e5}',
    '\u{a8e6}',
    '\u{a8e7}',
    '\u{a8e8}',
    '\u{a8e9}',
    '\u{a8ea}',
    '\u{a8eb}',
    '\u{a8ec}',
    '\u{a8ed}',
    '\u{a8ee}',
    '\u{a8ef}',
    '\u{a8f0}',
    '\u{a8f1}',
    '\u{aab0}',
    '\u{aab2}',
    '\u{aab3}',
    '\u{aab7}',
    '\u{aab8}',
    '\u{aabe}',
    '\u{aabf}',
    '\u{aac1}',
    '\u{fe20}',
    '\u{fe21}',
    '\u{fe22}',
    '\u{fe23}',
    '\u{fe24}',
    '\u{fe25}',
    '\u{fe26}',
    '\u{10a0f}',
    '\u{10a38}',
    '\u{1d185}',
    '\u{1d186}',
    '\u{1d187}',
    '\u{1d188}',
    '\u{1d189}',
    '\u{1d1aa}',
    '\u{1d1ab}',
    '\u{1d1ac}',
    '\u{1d1ad}',
    '\u{1d242}',
    '\u{1d243}',
    '\u{1d244}',
];

pub const IMAGE_ROWS: usize = DIACRITICS.len();

/// One placeholder row: the row diacritic, the zero-column diacritic, then
/// bare placeholders whose columns the terminal infers left to right.
pub fn placeholder_row(row: usize, cols: u16) -> String {
    let Some(mark) = DIACRITICS.get(row) else {
        return String::new();
    };
    if cols == 0 {
        return String::new();
    }
    let mut text = String::with_capacity(cols as usize * 4 + 8);
    text.push('\u{10eeee}');
    text.push(*mark);
    text.push(DIACRITICS[0]);
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
    fn image_ids_stay_within_the_placeholder_color_range() {
        let first = next_image_id();
        assert!((1..=0x00ff_ffff).contains(&first));
        assert_ne!(first, next_image_id());
    }

    #[test]
    fn tmux_passthrough_doubles_every_escape_in_graphics_commands() {
        assert_eq!(
            tmux_passthrough(b"\x1b_Gi=31,a=q;AAAA\x1b\\"),
            b"\x1bPtmux;\x1b\x1b_Gi=31,a=q;AAAA\x1b\x1b\\\x1b\\"
        );
    }
}
