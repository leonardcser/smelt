//! Image leaf renderer. A display snapshot is fitted to the available cells as
//! full-resolution Kitty graphics or colored half-blocks, or its label when it
//! cannot be decoded. Both pictures use the same cell grid, so the transcript
//! keeps its layout when graphics support is detected or lost.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine;
use image::{
    codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder},
    imageops::{self, FilterType},
    ImageEncoder, RgbaImage,
};
use smelt_buffer::theme::intern;
use smelt_core::buffer::{AtomicObject, AtomicSpan, SpanMeta};
use smelt_core::style::{Color, Style};
use smelt_term::{kitty, RasterImage};
use smelt_transcript::content::block_layout::{GutterSpec, RetainedImageSpec};
use smelt_transcript::content::builder::LineBuilder;

const MAX_PNG_BASE64: usize = 2 * 1024 * 1024;
const BLOCK_CAPACITY: usize = 32;
const RETAINED_PNG_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_ROWS: u16 = 24;
const MAX_ROWS: u16 = 48;
/// Half-block pixels below this alpha show the terminal background.
const ALPHA_CUTOFF: u8 = 128;
/// Nominal cell size in pixels that sizes the grid for both pictures, so the
/// layout does not depend on the terminal's real cell size.
const NOMINAL_CELL: (u32, u32) = (8, 16);

#[derive(Clone, Copy, PartialEq, Eq)]
struct BlockKey {
    snapshot: u64,
    cols: u16,
    rows: u16,
    /// The cell size in pixels with Kitty graphics, `None` for half-blocks.
    graphics_cell: Option<(u32, u32)>,
}

enum ImageRow {
    Cells(Vec<(String, Style)>),
    /// The snapshot could not be decoded.
    Error(String),
}

pub(crate) struct ImageBlock {
    rows: Vec<ImageRow>,
    /// Set for Kitty graphics, whose rows are placeholders for one uploaded image.
    object: Option<Arc<AtomicObject>>,
}

impl ImageBlock {
    fn is_picture(&self) -> bool {
        matches!(self.rows.first(), Some(ImageRow::Cells(_)))
    }

    fn error(label: &str, width: u16) -> Self {
        Self {
            rows: vec![ImageRow::Error(fit_label(label, width))],
            object: None,
        }
    }
}

#[derive(Default)]
struct BlockCache {
    // Front is least recently used.
    entries: VecDeque<(BlockKey, Arc<ImageBlock>)>,
}

impl BlockCache {
    fn get(&mut self, key: &BlockKey) -> Option<Arc<ImageBlock>> {
        let index = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(index)?;
        let block = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(block)
    }

    fn insert(&mut self, key: BlockKey, block: Arc<ImageBlock>) {
        self.entries.push_back((key, block));
        while self.entries.len() > BLOCK_CAPACITY
            || (self.entries.len() > 1 && self.retained_png_bytes() > RETAINED_PNG_BYTES)
        {
            self.entries.pop_front();
        }
    }

    fn retained_png_bytes(&self) -> usize {
        self.entries
            .iter()
            .filter_map(|(_, block)| block.object.as_ref())
            .map(|object| object.image.png_base64.len())
            .sum()
    }
}

fn cache() -> std::sync::MutexGuard<'static, BlockCache> {
    static CACHE: OnceLock<Mutex<BlockCache>> = OnceLock::new();
    CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn fit_label(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if smelt_buffer::cell_width::text_width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in smelt_buffer::cell_width::graphemes(text) {
        let w = smelt_buffer::cell_width::text_width(grapheme);
        if used + w + 1 > width {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    out.push('…');
    out
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

fn graphics_block(
    pixels: &RgbaImage,
    (cols, rows): (u16, u16),
    (cell_w, cell_h): (u32, u32),
) -> Option<ImageBlock> {
    let (canvas_w, canvas_h) = (u32::from(cols) * cell_w, u32::from(rows) * cell_h);
    let scale = (f64::from(canvas_w) / f64::from(pixels.width()))
        .min(f64::from(canvas_h) / f64::from(pixels.height()));
    let target_w = ((f64::from(pixels.width()) * scale).round() as u32).clamp(1, canvas_w);
    let target_h = ((f64::from(pixels.height()) * scale).round() as u32).clamp(1, canvas_h);
    let resized = imageops::resize(pixels, target_w, target_h, FilterType::Lanczos3);
    // The terminal stretches the upload over cols x rows cells, so pad to the
    // exact cell grid instead of letting it distort the aspect ratio.
    let mut canvas = RgbaImage::new(canvas_w, canvas_h);
    imageops::overlay(&mut canvas, &resized, 0, 0);
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
    Some(ImageBlock {
        rows: (0..rows)
            .map(|row| {
                ImageRow::Cells(vec![(
                    kitty::placeholder_row(usize::from(row), cols),
                    Style::default(),
                )])
            })
            .collect(),
        object: Some(AtomicObject::new(image)),
    })
}

fn blocks_block(pixels: &RgbaImage, (cols, rows): (u16, u16)) -> ImageBlock {
    // Each cell shows two vertically stacked pixels.
    let pixels = imageops::resize(
        pixels,
        u32::from(cols),
        u32::from(rows) * 2,
        FilterType::Triangle,
    );
    ImageBlock {
        rows: (0..u32::from(rows))
            .map(|row| ImageRow::Cells(half_block_runs(&pixels, row * 2)))
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

fn build_block(key: &BlockKey, spec: &RetainedImageSpec) -> ImageBlock {
    let Some(pixels) = engine::image::decode_display_snapshot(&spec.content.snapshot()) else {
        return ImageBlock::error(&spec.label, key.cols);
    };
    let grid = cell_grid(pixels.width(), pixels.height(), key.cols, key.rows);
    if let Some(cell) = key.graphics_cell {
        if let Some(block) = graphics_block(&pixels, grid, cell) {
            return block;
        }
    }
    blocks_block(&pixels, grid)
}

/// The laid-out rows for `spec` at `width`, shared between measuring and rendering.
fn image_block(spec: &RetainedImageSpec, width: u16) -> Arc<ImageBlock> {
    if width == 0 {
        return Arc::new(ImageBlock {
            rows: Vec::new(),
            object: None,
        });
    }
    let key = BlockKey {
        snapshot: spec.content.content_hash(),
        cols: width,
        rows: spec.max_rows.unwrap_or(DEFAULT_ROWS).clamp(1, MAX_ROWS),
        graphics_cell: crate::display_layout::kitty_cell_pixels(),
    };
    if let Some(block) = cache().get(&key) {
        return block;
    }
    let block = Arc::new(build_block(&key, spec));
    cache().insert(key, Arc::clone(&block));
    block
}

pub(crate) fn measure_image_spec(spec: &RetainedImageSpec, width: u16) -> usize {
    image_block(spec, width).rows.len()
}

pub(crate) fn render_image_spec(
    out: &mut LineBuilder,
    spec: &RetainedImageSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let block = image_block(spec, width);
    let first = out.line_count();
    let mut emitted = 0u16;
    for (index, row) in block
        .rows
        .iter()
        .enumerate()
        .skip(row_start)
        .take(row_count)
    {
        if let Some(gutter) = gutter {
            out.print_gutter(&gutter.text);
        }
        match row {
            ImageRow::Cells(runs) => {
                // Cells stay selectable: copying a selection that covers one emits
                // the group's source text instead of the glyphs.
                let meta = SpanMeta {
                    atomic: block.object.as_ref().map(|object| AtomicSpan {
                        object: Arc::clone(object),
                        row: index as u16,
                    }),
                    ..SpanMeta::default()
                };
                for (text, style) in runs {
                    out.append_resolved_span(text, *style, meta.clone());
                }
            }
            ImageRow::Error(text) => {
                out.push_hl(intern("ErrorMsg"));
                out.print(text);
                out.pop_style();
            }
        }
        out.newline();
        emitted += 1;
    }
    if block.is_picture() && emitted > 0 {
        let source = if spec.source.is_empty() {
            &spec.label
        } else {
            &spec.source
        };
        out.stamp_atomic_copy_group(first, source);
    }
    emitted
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_transcript::content::builder::test_util::{render_test, TestLine};
    use smelt_transcript::transcript_content::TranscriptContent;

    fn snapshot_spec(w: u32, h: u32, rgba: [u8; 4]) -> RetainedImageSpec {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        RgbaImage::from_pixel(w, h, image::Rgba(rgba))
            .save(&path)
            .unwrap();
        let snapshot = engine::image::display_snapshot(path.to_str().unwrap()).unwrap();
        RetainedImageSpec {
            content: TranscriptContent::from(snapshot.data),
            label: format!("[image: shot.png {w}x{h}]"),
            source: "/work/shot.png".into(),
            max_rows: None,
        }
    }

    fn cells(row: &ImageRow) -> &[(String, Style)] {
        match row {
            ImageRow::Cells(runs) => runs,
            ImageRow::Error(_) => panic!("expected cells"),
        }
    }

    fn row_width(row: &ImageRow) -> usize {
        cells(row)
            .iter()
            .map(|(text, _)| smelt_buffer::cell_width::text_width(text))
            .sum()
    }

    /// Tests have no graphics support, so images draw as half-blocks.
    fn render_lines(spec: &RetainedImageSpec) -> Vec<TestLine> {
        render_test(40, |out| {
            render_image_spec(out, spec, 40, 0, usize::MAX, None);
        })
        .lines
    }

    #[test]
    fn fit_within_keeps_aspect_and_never_enlarges() {
        assert_eq!(fit_within(100, 50, 40, 40), (40, 20));
        assert_eq!(fit_within(100, 200, 40, 40), (20, 40));
        assert_eq!(fit_within(10, 10, 40, 40), (10, 10));
        assert_eq!(fit_within(1000, 1, 10, 10), (10, 1));
    }

    #[test]
    fn half_blocks_pairs_pixel_rows_into_half_blocks() {
        let spec = snapshot_spec(64, 64, [255, 0, 0, 255]);
        let block = image_block(&spec, 40);
        assert_eq!(block.rows.len(), 4);
        assert!(block.object.is_none());
        let [(text, style)] = cells(&block.rows[0]) else {
            panic!("expected one run");
        };
        assert_eq!(text, &"▀".repeat(8));
        // The opaque snapshot is JPEG, so allow small color drift.
        let (Some(Color::Rgb { r, g, b }), Some(_)) = (style.fg, style.bg) else {
            panic!("expected rgb fg and bg: {style:?}");
        };
        assert!(r > 240 && g < 16 && b < 16, "{r},{g},{b}");
    }

    #[test]
    fn half_blocks_leaves_transparent_pixels_to_the_terminal_background() {
        let spec = snapshot_spec(32, 16, [0, 0, 0, 0]);
        let block = image_block(&spec, 40);
        assert_eq!(
            cells(&block.rows[0]),
            &[("    ".to_string(), Style::default())]
        );
    }

    #[test]
    fn half_blocks_fits_the_available_width_and_row_cap() {
        let spec = snapshot_spec(400, 400, [0, 255, 0, 255]);
        let narrow = image_block(&spec, 10);
        assert_eq!((row_width(&narrow.rows[0]), narrow.rows.len()), (10, 5));
        let wide = image_block(&spec, 200);
        assert_eq!(wide.rows.len(), usize::from(DEFAULT_ROWS));
        let capped = image_block(
            &RetainedImageSpec {
                max_rows: Some(3),
                ..spec
            },
            200,
        );
        assert_eq!(capped.rows.len(), 3);
    }

    #[test]
    fn graphics_and_half_blocks_cover_the_same_cells_at_any_cell_size() {
        let pixels = RgbaImage::from_pixel(100, 50, image::Rgba([9, 9, 9, 255]));
        // 100x50 px at the nominal 8x16 px cell: 13 cols, 4 rows.
        let grid = cell_grid(100, 50, 40, DEFAULT_ROWS);
        assert_eq!(grid, (13, 4));
        let blocks = blocks_block(&pixels, grid);
        assert_eq!((row_width(&blocks.rows[0]), blocks.rows.len()), (13, 4));
        for cell in [(10, 20), (7, 15), (12, 24)] {
            let graphics = graphics_block(&pixels, grid, cell).unwrap();
            assert_eq!(graphics.rows.len(), 4);
            let object = graphics.object.as_ref().unwrap();
            assert_eq!((object.image.cols, object.image.rows), (13, 4));
            assert!(kitty::image_exists(object.image.id));
        }
    }

    #[test]
    fn tall_images_stop_at_the_row_cap() {
        assert_eq!(cell_grid(20, 4000, 80, MAX_ROWS).1, MAX_ROWS);
    }

    #[test]
    fn copying_a_picture_yields_its_source_on_every_row() {
        let spec = snapshot_spec(64, 64, [10, 20, 30, 255]);
        let lines = render_lines(&spec);
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(line.atomic_source_text.as_deref(), Some("/work/shot.png"));
            assert!(line.spans.iter().all(|span| span.meta.selectable));
        }
        assert!(!lines[0].copy_continuation);
        assert!(lines[1..].iter().all(|line| line.copy_continuation));
    }

    #[test]
    fn unreadable_snapshot_shows_the_label() {
        let spec = RetainedImageSpec {
            content: TranscriptContent::from("not base64 image data".to_string()),
            label: "[image: gone.png]".into(),
            source: String::new(),
            max_rows: None,
        };
        let block = image_block(&spec, 80);
        assert!(matches!(
            &block.rows[..],
            [ImageRow::Error(text)] if text == "[image: gone.png]"
        ));
    }

    #[test]
    fn labels_truncate_to_the_available_width() {
        assert_eq!(fit_label("[image: a.png]", 40), "[image: a.png]");
        assert_eq!(fit_label("[image: abcdef.png]", 8), "[image:…");
    }
}
