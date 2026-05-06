//! `Block::User` renderer + the geometry helper used both here and by
//! the prompt buffer to size the tinted user-bubble.

use smelt_core::buffer::SpanMeta;
use smelt_core::content::builder::{display_width, LineBuilder};
use smelt_core::content::wrap::wrap_line;
use smelt_core::theme::role_hl;
use smelt_core::transcript_present::is_command_like;

/// Preprocessed user message layout: tab-expanded, blank-trimmed lines
/// with a computed `block_w` for multiline bubble rendering.
pub struct UserBlockGeometry {
    pub lines: Vec<String>,
    pub block_w: usize,
}

impl UserBlockGeometry {
    pub fn new(text: &str, text_w: usize) -> Self {
        let all_lines: Vec<String> = text.lines().map(|l| l.replace('\t', "    ")).collect();
        let start = all_lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
        let end = all_lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        let lines: Vec<String> = all_lines[start..end]
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        let wraps = lines.iter().any(|l| display_width(l) > text_w);
        let multiline = lines.len() > 1 || wraps;
        let block_w = if multiline {
            if wraps {
                text_w + 1
            } else {
                lines.iter().map(|l| display_width(l)).max().unwrap_or(0) + 1
            }
        } else {
            0
        };
        Self { lines, block_w }
    }
}

pub(super) fn render(
    out: &mut LineBuilder,
    text: &str,
    image_labels: &[String],
    width: usize,
) -> u16 {
    let is_command = is_command_like(text.trim());
    let text_w = width.saturating_sub(1).max(1);
    let geom = UserBlockGeometry::new(text, text_w);
    let user_bg = role_hl("UserBg");
    let mut rows = 0u16;
    let pad_meta = SpanMeta {
        selectable: false,
        copy_as: None,
    };
    for logical_line in &geom.lines {
        if logical_line.is_empty() {
            let fill = if geom.block_w > 0 { geom.block_w } else { 1 };
            out.set_hl(user_bg);
            out.print_with_meta(&" ".repeat(fill), pad_meta.clone());
            out.reset_style();
            out.set_gutter_bg_group(user_bg);
            out.newline();
            rows += 1;
            continue;
        }
        let chunks = wrap_line(logical_line, text_w);
        if chunks.len() > 1 {
            out.mark_wrapped();
        }
        for chunk in &chunks {
            let chunk_w = display_width(chunk);
            let trailing = if geom.block_w > 0 {
                geom.block_w.saturating_sub(chunk_w)
            } else {
                1
            };
            out.set_hl(user_bg);
            out.set_bold();
            print_highlights(out, chunk, image_labels, is_command);
            out.print_with_meta(&" ".repeat(trailing), pad_meta.clone());
            out.reset_style();
            out.set_gutter_bg_group(user_bg);
            out.newline();
            rows += 1;
        }
    }
    rows
}

/// Print user message text with accent highlighting for valid `@path` refs,
/// `/command` lines, and `[image]` attachment labels.
fn print_highlights(out: &mut LineBuilder, text: &str, image_labels: &[String], is_command: bool) {
    let accent_role = role_hl("Accent");

    if is_command {
        out.push_hl(accent_role);
        out.print(text);
        out.pop_style();
        return;
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut plain = String::new();

    let flush = |out: &mut LineBuilder, plain: &mut String| {
        if !plain.is_empty() {
            let s = std::mem::take(plain);
            out.print(&s);
        }
    };

    let accent = |out: &mut LineBuilder, token: String| {
        out.push_hl(accent_role);
        out.print(&token);
        out.pop_style();
    };

    while i < len {
        if chars[i] == '[' {
            let remaining: String = chars[i..].iter().collect();
            if let Some(label) = image_labels
                .iter()
                .find(|l| remaining.starts_with(l.as_str()))
            {
                flush(out, &mut plain);
                accent(out, label.clone());
                i += label.chars().count();
                continue;
            }
        }

        if let Some((token, end)) = smelt_core::content::selection::try_at_ref(&chars, i) {
            flush(out, &mut plain);
            accent(out, token);
            i = end;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    flush(out, &mut plain);
}
