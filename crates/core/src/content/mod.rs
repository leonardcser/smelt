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
