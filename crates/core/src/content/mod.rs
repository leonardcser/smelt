pub mod block_layout;
pub mod builder;
pub(crate) mod context;
pub mod highlight;
pub mod selection;
pub mod stream_parser;
pub mod transcript;
pub mod wrap;

pub use crate::buffer::SpanMeta;
pub use context::LayoutContext;

use crate::theme::HlGroup;

/// Context for rendering content inside a bordered box.
/// When passed to `render_markdown` and its sub-renderers, each output line
/// gets a colored left border prefix and a right border suffix with padding.
pub struct BoxContext {
    /// Left border string printed before each line (e.g. "   │ ").
    pub left: &'static str,
    /// Right border string printed after padding (e.g. " │").
    pub right: &'static str,
    /// Theme group whose fg colors the border characters.
    pub group: HlGroup,
    /// Inner content width (between left and right borders).
    pub inner_w: usize,
}

impl BoxContext {
    /// Print the left border with color.
    pub fn print_left(&self, out: &mut builder::LineBuilder) {
        out.push_hl(self.group);
        out.print_gutter(self.left);
        out.pop_style();
    }

    /// Print right-side padding and border for a line that used `cols` content columns.
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
/// Frame duration for every animated spinner in the app. Callers in
/// the status bar, transcript tool previews, and the Lua `smelt.ui.
/// spinner` API all read this so every on-screen spinner stays in
/// lockstep.
pub const SPINNER_FRAME_MS: u64 = 150;

/// Current spinner frame glyph, sampled from a process-wide monotonic
/// clock. All call sites that animate the same pill should use this
/// helper rather than reimplementing the `elapsed / SPINNER_FRAME_MS
/// % len` modulo so frames stay coherent across the status bar,
/// transcript, and Lua-driven dialogs.
pub fn spinner_frame_index(elapsed: std::time::Duration) -> usize {
    ((elapsed.as_millis() / SPINNER_FRAME_MS as u128) as usize) % SPINNER_FRAMES.len()
}

/// Time-based current glyph. Uses a process-start epoch so every
/// caller converges on the same frame without threading an explicit
/// `Instant` around.
pub fn spinner_glyph() -> &'static str {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    SPINNER_FRAMES[spinner_frame_index(epoch.elapsed())]
}

/// Default layout width when no explicit width is provided.
/// Used by renderers that need a fallback column budget.
pub(crate) fn default_width() -> usize {
    80
}

/// A markdown table separator line (e.g. `|---|---|`).
pub fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
}
