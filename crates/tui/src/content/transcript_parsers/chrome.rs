//! Shared chrome for `Block::User` / `Block::Exec`: a full-width
//! `SmeltUserBg` panel with blank padding rows top and bottom. Callers
//! pass the pre-trimmed logical lines and a `paint` closure that
//! decorates each wrapped chunk. The closure receives the chunk's
//! position in the chrome block so prefix styling can span wrapped rows.

use smelt_core::buffer::SpanMeta;
use smelt_core::content::builder::LineBuilder;
use smelt_core::content::wrap::wrap_line;
use smelt_core::theme::intern;

use super::metrics::CHROME_INNER_PAD;

pub(super) struct ChunkPos {
    pub chunk_idx: usize,
    pub logical_line: usize,
    pub char_start: usize,
}

pub(super) fn render(
    out: &mut LineBuilder,
    lines: &[String],
    text_w: usize,
    mut paint: impl FnMut(&mut LineBuilder, &str, ChunkPos),
) -> u16 {
    let user_bg = intern("SmeltUserBg");
    let bg = out
        .theme()
        .resolve(user_bg)
        .bg
        .unwrap_or(smelt_core::style::Color::Reset);
    let pad_meta = SpanMeta {
        selectable: false,
        copy_as: None,
    };
    let pad: String = " ".repeat(CHROME_INNER_PAD);
    let blank_row = |out: &mut LineBuilder| {
        out.set_hl(user_bg);
        out.print_with_meta(&pad, pad_meta.clone());
        out.fill_line_bg(bg);
        out.reset_style();
        out.newline();
    };

    let mut rows = 0u16;
    blank_row(out);
    rows += 1;

    let mut chunk_idx = 0usize;
    for (logical_line, logical) in lines.iter().enumerate() {
        if logical.is_empty() {
            blank_row(out);
            rows += 1;
            continue;
        }
        let chunks = wrap_line(logical, text_w);
        if chunks.len() > 1 {
            out.mark_wrapped();
        }
        let mut char_start = 0usize;
        for chunk in &chunks {
            out.set_hl(user_bg);
            out.print_with_meta(&pad, pad_meta.clone());
            out.set_bold();
            paint(
                out,
                chunk,
                ChunkPos {
                    chunk_idx,
                    logical_line,
                    char_start,
                },
            );
            out.set_hl(user_bg);
            out.fill_line_bg(bg);
            out.reset_style();
            out.newline();
            rows += 1;
            chunk_idx += 1;
            char_start += chunk.chars().count();
        }
    }

    blank_row(out);
    rows += 1;
    rows
}
