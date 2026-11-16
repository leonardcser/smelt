//! Pictures fitted to a cell grid, as full-resolution Kitty graphics or
//! colored half-blocks. Both cover the same cells, so a layout does not change
//! when graphics support is detected or lost.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine;
use image::{
    codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder},
    imageops::{self, FilterType},
    ImageEncoder, RgbaImage,
};
use smelt_buffer::buffer::{AtomicObject, AtomicSpan, Buffer, SpanMeta};
use smelt_core::style::{Color, Style};
use smelt_term::{kitty, RasterImage};

const MAX_PNG_BASE64: usize = 2 * 1024 * 1024;
const CACHE_CAPACITY: usize = 32;
const RETAINED_PNG_BYTES: usize = 16 * 1024 * 1024;
/// Half-block pixels below this alpha show the terminal background.
const ALPHA_CUTOFF: u8 = 128;
/// Nominal cell size in pixels that sizes the grid for both pictures, so the
/// layout does not depend on the terminal's real cell size.
const NOMINAL_CELL: (u32, u32) = (8, 16);

/// An image laid out as rows of styled cell runs. With Kitty graphics the rows
/// are placeholders for one uploaded image, which `object` owns.
pub struct Picture {
    rows: Vec<Vec<(String, Style)>>,
    object: Option<Arc<AtomicObject>>,
}

impl Picture {
    pub fn rows(&self) -> &[Vec<(String, Style)>] {
        &self.rows
    }

    /// Width in cells.
    pub fn cols(&self) -> u16 {
        self.rows.first().map_or(0, |runs| {
            runs.iter()
                .map(|(text, _)| smelt_buffer::cell_width::text_width_u16(text))
                .sum()
        })
    }

    /// The span metadata for picture row `row`: Kitty rows carry the image.
    pub fn row_meta(&self, row: usize) -> SpanMeta {
        SpanMeta {
            atomic: self.object.as_ref().map(|object| AtomicSpan {
                object: Arc::clone(object),
                row: row as u16,
            }),
            ..SpanMeta::default()
        }
    }

    /// Replace `buf`'s lines with the picture.
    pub fn write_to(&self, buf: &mut Buffer) {
        buf.set_all_lines(
            self.rows
                .iter()
                .map(|runs| runs.iter().map(|(text, _)| text.as_str()).collect())
                .collect(),
        );
        for (row, runs) in self.rows.iter().enumerate() {
            let meta = self.row_meta(row);
            let mut col = 0u16;
            for (text, style) in runs {
                let end = col.saturating_add(smelt_buffer::cell_width::text_width_u16(text));
                buf.add_highlight_with_meta(row, col, end, *style, meta.clone());
                col = end;
            }
        }
    }
}

/// Where an image rests in its cells when it does not fill the last row. A
/// picture pinned by its bottom edge rests on the bottom, so no gap shows
/// under it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Top,
    Bottom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    source: u64,
    cols: u16,
    rows: u16,
    align: Align,
    /// The cell size in pixels with Kitty graphics, `None` for half-blocks.
    graphics_cell: Option<(u32, u32)>,
}

/// A cached fit, filled once by whichever caller builds it first. Later
/// callers wait for that build instead of repeating it.
type Slot = Arc<OnceLock<Option<Arc<Picture>>>>;

/// Recently fitted pictures, including sources that failed to decode so they
/// are not decoded again every frame.
#[derive(Default)]
struct Cache {
    // Front is least recently used.
    entries: VecDeque<(Key, Slot)>,
}

impl Cache {
    fn slot(&mut self, key: Key) -> Slot {
        if let Some(index) = self.entries.iter().position(|(cached, _)| *cached == key) {
            let entry = self.entries.remove(index).expect("cached entry");
            let slot = Arc::clone(&entry.1);
            self.entries.push_back(entry);
            return slot;
        }
        let slot = Slot::default();
        self.entries.push_back((key, Arc::clone(&slot)));
        while self.entries.len() > CACHE_CAPACITY
            || (self.entries.len() > 1 && self.retained_png_bytes() > RETAINED_PNG_BYTES)
        {
            self.entries.pop_front();
        }
        slot
    }

    fn retained_png_bytes(&self) -> usize {
        self.entries
            .iter()
            .filter_map(|(_, slot)| slot.get()?.as_ref()?.object.as_ref())
            .map(|object| object.image.png_base64.len())
            .sum()
    }
}

/// The picture of the image identified by `source`, fitted within
/// `max_cols` x `max_rows` cells without enlarging it and resting in them
/// by `align`. `decode` runs only when
/// no fitted copy is cached or being built. `None` when the image cannot be
/// decoded.
pub fn fitted(
    source: u64,
    max_cols: u16,
    max_rows: u16,
    align: Align,
    decode: impl FnOnce() -> Option<RgbaImage>,
) -> Option<Arc<Picture>> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    if max_cols == 0 || max_rows == 0 {
        return None;
    }
    let key = Key {
        source,
        cols: max_cols,
        rows: max_rows,
        align,
        graphics_cell: crate::display_layout::kitty_cell_pixels(),
    };
    // Built outside the cache lock, so other images stay available meanwhile.
    let slot = CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .slot(key);
    slot.get_or_init(|| decode().map(|pixels| Arc::new(build(&pixels, &key))))
        .clone()
}

fn build(pixels: &RgbaImage, key: &Key) -> Picture {
    let grid = cell_grid(pixels.width(), pixels.height(), key.cols, key.rows);
    key.graphics_cell
        .and_then(|cell| graphics(pixels, grid, cell, key.align))
        .unwrap_or_else(|| half_blocks(pixels, grid))
}

/// Largest size within `max_w` x `max_h` that keeps the aspect ratio and never
/// enlarges the image.
fn fit_within(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (f64::from(max_w) / f64::from(w))
        .min(f64::from(max_h) / f64::from(h))
        .min(1.0);
    (
        ((f64::from(w) * scale).round() as u32).max(1),
        ((f64::from(h) * scale).round() as u32).max(1),
    )
}

/// The cells an image of `w` x `h` pixels covers at the nominal cell size,
/// within `max_cols` x `max_rows`.
fn cell_grid(w: u32, h: u32, max_cols: u16, max_rows: u16) -> (u16, u16) {
    let (cell_w, cell_h) = NOMINAL_CELL;
    let (fit_w, fit_h) = fit_within(
        w,
        h,
        u32::from(max_cols) * cell_w,
        u32::from(max_rows) * cell_h,
    );
    (fit_w.div_ceil(cell_w) as u16, fit_h.div_ceil(cell_h) as u16)
}

fn graphics(
    pixels: &RgbaImage,
    (cols, rows): (u16, u16),
    (cell_w, cell_h): (u32, u32),
    align: Align,
) -> Option<Picture> {
    let (canvas_w, canvas_h) = (u32::from(cols) * cell_w, u32::from(rows) * cell_h);
    let scale = (f64::from(canvas_w) / f64::from(pixels.width()))
        .min(f64::from(canvas_h) / f64::from(pixels.height()));
    let target_w = ((f64::from(pixels.width()) * scale).round() as u32).clamp(1, canvas_w);
    let target_h = ((f64::from(pixels.height()) * scale).round() as u32).clamp(1, canvas_h);
    let resized = resized(pixels, target_w, target_h);
    // The terminal stretches the upload over cols x rows cells, so pad to the
    // exact cell grid instead of letting it distort the aspect ratio.
    let mut canvas = RgbaImage::new(canvas_w, canvas_h);
    let y = match align {
        Align::Top => 0,
        Align::Bottom => canvas_h - target_h,
    };
    imageops::overlay(&mut canvas, &resized, 0, i64::from(y));
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, PngFilter::Adaptive)
        .write_image(
            canvas.as_raw(),
            canvas_w,
            canvas_h,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    let png: Arc<str> = base64::engine::general_purpose::STANDARD.encode(png).into();
    if png.len() > MAX_PNG_BASE64 {
        return None;
    }
    let image: Arc<RasterImage> = kitty::register_image(kitty::next_image_id(), png, cols, rows)?;
    Some(Picture {
        rows: (0..rows)
            .map(|row| {
                vec![(
                    kitty::placeholder_row(usize::from(row), cols),
                    Style::default(),
                )]
            })
            .collect(),
        object: Some(AtomicObject::new(image)),
    })
}

/// `pixels` resized to exactly `w` x `h`. Shrinking uses a box filter, which
/// is several times faster than Lanczos at the large ratios previews need.
fn resized(pixels: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    if w <= pixels.width() && h <= pixels.height() {
        imageops::thumbnail(pixels, w, h)
    } else {
        imageops::resize(pixels, w, h, FilterType::Triangle)
    }
}

fn half_blocks(pixels: &RgbaImage, (cols, rows): (u16, u16)) -> Picture {
    // Each cell shows two vertically stacked pixels.
    let pixels = resized(pixels, u32::from(cols), u32::from(rows) * 2);
    Picture {
        rows: (0..u32::from(rows))
            .map(|row| half_block_runs(&pixels, row * 2))
            .collect(),
        object: None,
    }
}

/// One terminal row of `▀` and `▄` cells covering pixel rows `y` and `y + 1`.
/// Transparent pixels leave the terminal background showing.
fn half_block_runs(pixels: &RgbaImage, y: u32) -> Vec<(String, Style)> {
    let color = |x: u32, y: u32| {
        let [r, g, b, a] = pixels.get_pixel(x, y).0;
        (a >= ALPHA_CUTOFF).then_some(Color::Rgb { r, g, b })
    };
    let mut runs: Vec<(String, Style)> = Vec::new();
    for x in 0..pixels.width() {
        let (glyph, fg, bg) = match (color(x, y), color(x, y + 1)) {
            (Some(top), bottom) => ('▀', Some(top), bottom),
            (None, Some(bottom)) => ('▄', Some(bottom), None),
            (None, None) => (' ', None, None),
        };
        let style = Style {
            fg,
            bg,
            ..Style::default()
        };
        match runs.last_mut() {
            Some((text, last)) if *last == style => text.push(glyph),
            _ => runs.push((glyph.to_string(), style)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba(rgba))
    }

    fn row_width(runs: &[(String, Style)]) -> usize {
        runs.iter()
            .map(|(text, _)| smelt_buffer::cell_width::text_width(text))
            .sum()
    }

    #[test]
    fn fit_within_keeps_aspect_and_never_enlarges() {
        assert_eq!(fit_within(100, 50, 40, 40), (40, 20));
        assert_eq!(fit_within(100, 200, 40, 40), (20, 40));
        assert_eq!(fit_within(10, 10, 40, 40), (10, 10));
        assert_eq!(fit_within(1000, 1, 10, 10), (10, 1));
    }

    #[test]
    fn half_blocks_pair_pixel_rows_into_half_blocks() {
        let picture = half_blocks(&solid(64, 64, [255, 0, 0, 255]), (8, 4));
        assert_eq!(picture.rows.len(), 4);
        assert!(picture.object.is_none());
        let [(text, style)] = &picture.rows[0][..] else {
            panic!("expected one run");
        };
        assert_eq!(text, &"▀".repeat(8));
        let red = Some(Color::Rgb { r: 255, g: 0, b: 0 });
        assert_eq!((style.fg, style.bg), (red, red));
    }

    #[test]
    fn half_blocks_leave_transparent_pixels_to_the_terminal_background() {
        let picture = half_blocks(&solid(32, 16, [0, 0, 0, 0]), (4, 1));
        assert_eq!(picture.rows[0], [("    ".to_string(), Style::default())]);
    }

    #[test]
    fn fitted_pictures_respect_the_cell_box() {
        let pixels = solid(400, 400, [0, 255, 0, 255]);
        let narrow = build(
            &pixels,
            &Key {
                source: 0,
                cols: 10,
                rows: 24,
                align: Align::Top,
                graphics_cell: None,
            },
        );
        assert_eq!((row_width(&narrow.rows[0]), narrow.rows.len()), (10, 5));
        let capped = build(
            &pixels,
            &Key {
                source: 0,
                cols: 200,
                rows: 3,
                align: Align::Top,
                graphics_cell: None,
            },
        );
        assert_eq!(capped.rows.len(), 3);
        assert_eq!(capped.cols(), 6);
    }

    #[test]
    fn graphics_and_half_blocks_cover_the_same_cells_at_any_cell_size() {
        let pixels = solid(100, 50, [9, 9, 9, 255]);
        // 100x50 px at the nominal 8x16 px cell: 13 cols, 4 rows.
        let grid = cell_grid(100, 50, 40, 24);
        assert_eq!(grid, (13, 4));
        let blocks = half_blocks(&pixels, grid);
        assert_eq!((row_width(&blocks.rows[0]), blocks.rows.len()), (13, 4));
        for cell in [(10, 20), (7, 15), (12, 24)] {
            let graphics = graphics(&pixels, grid, cell, Align::Top).unwrap();
            assert_eq!(graphics.rows.len(), 4);
            let object = graphics.object.as_ref().unwrap();
            assert_eq!((object.image.cols, object.image.rows), (13, 4));
            assert!(kitty::image_exists(object.image.id));
        }
    }

    #[test]
    fn graphics_rest_on_the_edge_they_align_to() {
        // 100x50 px fills 130x65 of a 13x4 grid of 10x20 px cells: 15 px spare.
        let pixels = solid(100, 50, [9, 9, 9, 255]);
        let opaque_rows = |align| {
            let picture = graphics(&pixels, (13, 4), (10, 20), align).unwrap();
            let png = base64::engine::general_purpose::STANDARD
                .decode(picture.object.unwrap().image.png_base64.as_bytes())
                .unwrap();
            let canvas = image::load_from_memory(&png).unwrap().into_rgba8();
            let opaque: Vec<u32> = (0..canvas.height())
                .filter(|&y| canvas.get_pixel(0, y)[3] > 0)
                .collect();
            (opaque[0], opaque[opaque.len() - 1])
        };
        assert_eq!(opaque_rows(Align::Top), (0, 64));
        assert_eq!(opaque_rows(Align::Bottom), (15, 79));
    }

    #[test]
    fn tall_images_stop_at_the_row_cap() {
        assert_eq!(cell_grid(20, 4000, 80, 48).1, 48);
    }

    #[test]
    fn concurrent_callers_share_one_decode_per_source_and_box() {
        let source = 0x5eed_0001;
        let decodes = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let picture = fitted(source, 10, 4, Align::Top, || {
                        decodes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Keep the build in flight while the other callers arrive.
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Some(solid(16, 16, [1, 2, 3, 255]))
                    });
                    assert!(picture.is_some());
                });
            }
        });
        assert_eq!(decodes.into_inner(), 1);
        assert!(fitted(source + 1, 10, 4, Align::Top, || None).is_none());
        assert!(fitted(source + 1, 10, 4, Align::Top, || panic!(
            "failure is cached"
        ))
        .is_none());
    }

    #[test]
    fn write_to_fills_a_buffer_with_the_picture() {
        let picture = half_blocks(&solid(16, 32, [0, 0, 255, 255]), (2, 2));
        let mut buf = Buffer::new(
            smelt_buffer::buffer::BufId(0),
            smelt_buffer::buffer::BufCreateOpts::default(),
        );
        picture.write_to(&mut buf);
        assert_eq!(buf.lines(), ["▀▀", "▀▀"]);
        let spans = buf.highlights_at(1);
        assert_eq!(
            (spans.len(), spans[0].col_start, spans[0].col_end),
            (1, 0, 2)
        );
    }
}
