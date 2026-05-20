pub mod block_layout;
pub mod builder;
pub(crate) mod context;
pub mod highlight;
pub mod selection;
pub mod stream_parser;
pub mod transcript;

pub use smelt_buffer::wrap;

pub use crate::buffer::SpanMeta;
pub use context::LayoutContext;

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

/// Current glyph from a process-start epoch; all callers converge on the same frame.
pub fn spinner_glyph() -> &'static str {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    SPINNER_FRAMES[spinner_frame_index(epoch.elapsed())]
}

/// Glyph for a caller-provided elapsed duration. Use this when the
/// frame must derive from a virtual or paused-aware clock so snapshot
/// tests stay deterministic. Callers that want process-wide animation
/// use `spinner_glyph` instead.
pub fn glyph_for(elapsed: std::time::Duration) -> &'static str {
    SPINNER_FRAMES[spinner_frame_index(elapsed)]
}

/// Fallback column budget when no explicit width is provided.
pub(crate) fn default_width() -> usize {
    80
}

/// Returns true for markdown table separator lines (e.g. `|---|---|`).
pub fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
}

/// Per-column alignment, parsed from a markdown table separator line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColumnAlignment {
    #[default]
    Left,
    Center,
    Right,
}

/// Parse column alignments from a markdown table separator like `|:---|:---:|---:|`.
/// Returns one entry per column: `:---` left, `:---:` center, `---:` right,
/// otherwise [`ColumnAlignment::Left`]. Returns empty when `line` isn't a separator.
pub fn parse_table_alignments(line: &str) -> Vec<ColumnAlignment> {
    if !is_table_separator(line) {
        return Vec::new();
    }
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| {
            let c = cell.trim();
            match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => ColumnAlignment::Center,
                (false, true) => ColumnAlignment::Right,
                _ => ColumnAlignment::Left,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_table_alignments_recognizes_all_three_markers() {
        let got = parse_table_alignments("|:---|:---:|---:|---|");
        assert_eq!(
            got,
            vec![
                ColumnAlignment::Left,
                ColumnAlignment::Center,
                ColumnAlignment::Right,
                ColumnAlignment::Left,
            ]
        );
    }

    #[test]
    fn parse_table_alignments_returns_empty_for_non_separator() {
        assert!(parse_table_alignments("| a | b |").is_empty());
    }
}
