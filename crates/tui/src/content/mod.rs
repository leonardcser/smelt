// Re-export headless-safe modules from core
pub use smelt_core::content::highlight;
pub use smelt_core::content::builder;
pub(crate) mod selection;

// Tui-specific submodules
pub(crate) mod block_buffers;
pub(crate) mod layout;
pub(crate) mod prompt_buf;
pub(crate) mod prompt_wrap;
pub(crate) mod status;
pub(crate) mod to_buffer;
pub(crate) mod transcript_buf;
pub mod transcript_parsers;
pub(crate) mod transcript_snapshot;

use crossterm::terminal;
use smelt_core::style::Color;

/// Emit `n` blank rows.
pub(crate) fn emit_newlines(out: &mut builder::LineBuilder, n: u16) {
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
