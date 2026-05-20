//! Shared chrome for `Block::User` / `Block::Exec`: a full-width
//! `SmeltUserBg` panel with blank padding rows top and bottom. Callers
//! pass the pre-trimmed logical lines and a `paint` closure that
//! decorates each wrapped chunk. The closure receives a 0-based
//! `chunk_idx` over content chunks (blank logical lines don't count) so
//! prefix glyphs like `!` can be styled on the first chunk only.

use smelt_core::buffer::SpanMeta;
use smelt_core::content::builder::LineBuilder;
use smelt_core::content::wrap::wrap_line;
use smelt_core::style::Color;
use smelt_core::theme::intern;

use super::metrics::CHROME_INNER_PAD;

pub(super) fn render(
    out: &mut LineBuilder,
    lines: &[String],
    text_w: usize,
    mut paint: impl FnMut(&mut LineBuilder, &str, usize),
) -> u16 {
    let user_bg = intern("SmeltUserBg");
    let user_bg_color = out.theme().resolve(user_bg).bg.unwrap_or(Color::Reset);
    let pad_meta = SpanMeta {
        selectable: false,
        copy_as: None,
    };
    let blank_anchor_meta = SpanMeta {
        selectable: true,
        copy_as: Some(String::new()),
    };
    let pad: String = " ".repeat(CHROME_INNER_PAD);

    let blank_row = |out: &mut LineBuilder| {
        out.set_hl(user_bg);
        out.print_with_meta(&pad, pad_meta.clone());
        out.print_with_meta(" ", blank_anchor_meta.clone());
        out.reset_style();
        out.fill_line_bg(user_bg_color);
        out.newline();
    };

    let mut rows = 0u16;
    blank_row(out);
    rows += 1;

    let mut chunk_idx = 0usize;
    for logical in lines {
        if logical.is_empty() {
            blank_row(out);
            rows += 1;
            continue;
        }
        let chunks = wrap_line(logical, text_w);
        if chunks.len() > 1 {
            out.mark_wrapped();
        }
        for chunk in &chunks {
            out.set_hl(user_bg);
            out.print_with_meta(&pad, pad_meta.clone());
            out.set_bold();
            paint(out, chunk, chunk_idx);
            out.set_hl(user_bg);
            out.pad_row_to_layout_width(pad_meta.clone());
            out.reset_style();
            out.newline();
            rows += 1;
            chunk_idx += 1;
        }
    }

    blank_row(out);
    rows += 1;
    rows
}
