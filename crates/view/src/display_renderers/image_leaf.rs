//! Transcript image leaf. A display snapshot is drawn as a [`Picture`] fitted
//! to the available width, or as its label when it cannot be decoded.

use std::sync::Arc;

use smelt_buffer::theme::intern;
use smelt_transcript::content::block_layout::{GutterSpec, RetainedImageSpec};
use smelt_transcript::content::builder::LineBuilder;

use crate::picture::{self, Picture};

const DEFAULT_ROWS: u16 = 24;
const MAX_ROWS: u16 = 48;

enum ImageBlock {
    Picture(Arc<Picture>),
    /// The snapshot could not be decoded.
    Error(String),
}

impl ImageBlock {
    fn row_count(&self) -> usize {
        match self {
            ImageBlock::Picture(picture) => picture.rows().len(),
            ImageBlock::Error(_) => 1,
        }
    }
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

/// The laid-out rows for `spec` at `width`, shared between measuring and rendering.
fn image_block(spec: &RetainedImageSpec, width: u16) -> Option<ImageBlock> {
    if width == 0 {
        return None;
    }
    let rows = spec.max_rows.unwrap_or(DEFAULT_ROWS).clamp(1, MAX_ROWS);
    let picture = picture::fitted(
        spec.content.content_hash(),
        width,
        rows,
        picture::Align::Top,
        || engine::image::decode_base64_image(&spec.content.snapshot()),
    );
    Some(match picture {
        Some(picture) => ImageBlock::Picture(picture),
        None => ImageBlock::Error(fit_label(&spec.label, width)),
    })
}

pub(crate) fn measure_image_spec(spec: &RetainedImageSpec, width: u16) -> usize {
    image_block(spec, width).map_or(0, |block| block.row_count())
}

pub(crate) fn render_image_spec(
    out: &mut LineBuilder,
    spec: &RetainedImageSpec,
    width: u16,
    row_start: usize,
    row_count: usize,
    gutter: Option<&GutterSpec>,
) -> u16 {
    let Some(block) = image_block(spec, width) else {
        return 0;
    };
    let first = out.line_count();
    let mut emitted = 0u16;
    for index in (row_start..block.row_count()).take(row_count) {
        if let Some(gutter) = gutter {
            out.print_gutter(&gutter.text);
        }
        match &block {
            ImageBlock::Picture(picture) => {
                // Cells stay selectable: copying a selection that covers one emits
                // the group's source text instead of the glyphs.
                let meta = picture.row_meta(index);
                for (text, style) in &picture.rows()[index] {
                    out.append_resolved_span(text, *style, meta.clone());
                }
            }
            ImageBlock::Error(text) => {
                out.push_hl(intern("ErrorMsg"));
                out.print(text);
                out.pop_style();
            }
        }
        out.newline();
        emitted += 1;
    }
    if matches!(block, ImageBlock::Picture(_)) && emitted > 0 {
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
    use image::RgbaImage;
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

    /// Tests have no graphics support, so images draw as half-blocks.
    fn render_lines(spec: &RetainedImageSpec) -> Vec<TestLine> {
        render_test(40, |out| {
            render_image_spec(out, spec, 40, 0, usize::MAX, None);
        })
        .lines
    }

    #[test]
    fn rows_default_to_a_cap_that_max_rows_overrides() {
        let spec = snapshot_spec(400, 400, [0, 255, 0, 255]);
        assert_eq!(measure_image_spec(&spec, 10), 5);
        assert_eq!(measure_image_spec(&spec, 200), usize::from(DEFAULT_ROWS));
        let capped = RetainedImageSpec {
            max_rows: Some(3),
            ..spec
        };
        assert_eq!(measure_image_spec(&capped, 200), 3);
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
        let lines = render_lines(&spec);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "[image: gone.png]");
    }

    #[test]
    fn labels_truncate_to_the_available_width() {
        assert_eq!(fit_label("[image: a.png]", 40), "[image: a.png]");
        assert_eq!(fit_label("[image: abcdef.png]", 8), "[image:…");
    }
}
