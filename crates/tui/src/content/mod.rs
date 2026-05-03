pub(crate) mod highlight;
pub(crate) mod layout;
pub(crate) mod layout_out;
pub(crate) mod prompt_data;
pub(crate) mod prompt_wrap;
pub(crate) mod selection;
pub(crate) mod status;
pub(crate) mod to_buffer;
pub(crate) mod transcript_buf;

use crossterm::{style::Color, terminal};
use crate::core::content::display::ColorValue;

/// Context for rendering content inside a bordered box.
/// When passed to `render_markdown` and its sub-renderers, each output line
/// gets a colored left border prefix and a right border suffix with padding.
pub(crate) struct BoxContext {
    /// Left border string printed before each line (e.g. "   │ ").
    pub(crate) left: &'static str,
    /// Right border string printed after padding (e.g. " │").
    pub(crate) right: &'static str,
    /// Color for the border characters.
    pub(crate) color: ColorValue,
    /// Inner content width (between left and right borders).
    pub(crate) inner_w: usize,
}

impl BoxContext {
    /// Print the left border with color.
    pub(crate) fn print_left(&self, out: &mut layout_out::SpanCollector) {
        out.push_fg(self.color);
        out.print_gutter(self.left);
        out.pop_style();
    }

    /// Print right-side padding and border for a line that used `cols` content columns.
    pub(crate) fn print_right(&self, out: &mut layout_out::SpanCollector, cols: usize) {
        let pad = self.inner_w.saturating_sub(cols);
        if pad > 0 {
            out.print_gutter(&" ".repeat(pad));
        }
        out.push_fg(self.color);
        out.print_gutter(self.right);
        out.pop_style();
    }
}

/// Emit `n` blank rows.
pub(crate) fn emit_newlines(out: &mut layout_out::SpanCollector, n: u16) {
    for _ in 0..n {
        out.newline();
    }
}

pub(super) fn reasoning_color(
    effort: protocol::ReasoningEffort,
    theme: &crate::ui::Theme,
) -> Color {
    let group = match effort {
        protocol::ReasoningEffort::Off => "SmeltReasonOff",
        protocol::ReasoningEffort::Low => "SmeltReasonLow",
        protocol::ReasoningEffort::Medium => "SmeltReasonMed",
        protocol::ReasoningEffort::High => "SmeltReasonHigh",
        protocol::ReasoningEffort::Max => "SmeltReasonMax",
    };
    let style = theme.get(group);
    style.fg.or(style.bg).unwrap_or(Color::Reset)
}

pub(crate) fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

pub(crate) fn term_height() -> usize {
    terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
}

pub(super) fn format_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}
