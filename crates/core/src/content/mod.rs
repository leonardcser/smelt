pub mod ansi;
pub mod block_layout;
pub mod builder;
pub mod code_block;
pub(crate) mod context;
pub mod file_icons;
pub mod highlight;
pub mod selection;
pub mod stream_parser;
pub mod tool_draft;
pub mod transcript;
pub mod width;

pub use smelt_buffer::inline_line;
pub mod markdown_ir;
pub mod markdown_stream;
pub use markdown_stream::{
    markdown_closes_fence, markdown_opening_fence, FenceMarker, MarkdownFence,
};
pub use smelt_buffer::wrap;

pub use crate::buffer::SpanMeta;
pub use context::LayoutContext;

pub fn display_safe_char(ch: char) -> char {
    if ch != '\n' && ch.is_control() {
        '\u{FFFD}'
    } else {
        ch
    }
}

pub fn display_safe_text(text: &str) -> String {
    text.chars().map(display_safe_char).collect()
}

use crate::theme::HlGroup;

/// Context for rendering content inside a bordered box.
pub struct BoxContext {
    /// Left border string printed before each line (e.g. "   │ ").
    pub left: &'static str,
    /// Right border string printed after padding (e.g. " │").
    pub right: &'static str,
    pub group: HlGroup,
    /// Inner content width (between left and right borders).
    pub inner_w: usize,
}

impl BoxContext {
    pub fn print_left(&self, out: &mut builder::LineBuilder) {
        out.push_hl(self.group);
        out.print_gutter(self.left);
        out.pop_style();
    }

    /// Pad to `inner_w` and print the right border.
    pub fn print_right(&self, out: &mut builder::LineBuilder, cols: usize) {
        let pad = self.inner_w.saturating_sub(cols);
        if pad > 0 {
            out.print_gutter(&" ".repeat(pad));
        }
        out.push_hl(self.group);
        out.print_gutter(self.right);
        out.pop_style();
    }
}

pub(crate) const SPINNER_FRAMES: &[&str] = &["✿", "❀", "✾", "❁"];
/// Frame duration shared by all spinners; read by every animated call site to stay in lockstep.
pub const SPINNER_FRAME_MS: u64 = 150;

pub fn spinner_frame_index(elapsed: std::time::Duration) -> usize {
    ((elapsed.as_millis() / SPINNER_FRAME_MS as u128) as usize) % SPINNER_FRAMES.len()
}

/// Fallback column budget when no explicit width is provided.
pub(crate) fn default_width() -> usize {
    80
}

/// Split a single markdown list-item line into its marker prefix and body.
pub fn split_markdown_list_prefix(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1].is_ascii_whitespace()
    {
        return (&line[..2], &line[2..]);
    }

    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if (1..=9).contains(&i)
        && i + 1 < bytes.len()
        && matches!(bytes[i], b'.' | b')')
        && bytes[i + 1].is_ascii_whitespace()
    {
        return (&line[..i + 2], &line[i + 2..]);
    }

    ("", line)
}

pub fn is_markdown_list_item(line: &str) -> bool {
    !split_markdown_list_prefix(line.trim_start()).0.is_empty()
}

/// Per-column alignment, parsed from a markdown table separator line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColumnAlignment {
    #[default]
    Left,
    Center,
    Right,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_list_prefix_matches_common_mark_markers() {
        assert_eq!(split_markdown_list_prefix("- item"), ("- ", "item"));
        assert_eq!(split_markdown_list_prefix("+ item"), ("+ ", "item"));
        assert_eq!(split_markdown_list_prefix("12) item"), ("12) ", "item"));
        assert_eq!(split_markdown_list_prefix("1.item"), ("", "1.item"));
        assert!(is_markdown_list_item("  * item"));
    }

    #[test]
    fn markdown_opening_fence_keeps_fence_length_and_info() {
        let fence = markdown_opening_fence("  ````markdown").unwrap();
        assert_eq!(fence.len, 4);
        assert_eq!(fence.info, "markdown");
    }
}
