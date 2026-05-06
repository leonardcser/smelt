//! Transcript-presentation helpers that have to live in `core`.
//!
//! The actual per-block rendering (markdown / tools / view-state
//! collapse) lives in `tui::content::transcript_parsers`. Core only
//! holds the pieces that `transcript_model.rs` (a core type) and
//! `headless_app.rs` (a core entry point) reach for — the trait that
//! lets tui inject a Lua-backed tool renderer, the gap-between rule
//! that drives `BlockHistory::block_gap`, and the simple
//! `/<word>` heuristic used in headless command dispatch.

use crate::content::builder::LineBuilder;
use crate::transcript_model::{Block, ToolOutput};
use std::collections::HashMap;

/// Callback trait for tool-specific body rendering. Implemented in `tui`
/// by a Lua caller that receives a mode-spec from the tool's `render` hook.
pub trait ToolBodyRenderer: Send + Sync {
    fn render(
        &self,
        name: &str,
        args: &HashMap<String, serde_json::Value>,
        output: Option<&ToolOutput>,
        width: usize,
        out: &mut LineBuilder,
    ) -> u16;

    /// Whether the tool wants its elapsed time displayed in the
    /// transcript header. Default `false`; Lua tool defs opt in via
    /// `elapsed_visible = true` and the tui-side renderer reads the
    /// flag back through this method.
    fn elapsed_visible(&self, _name: &str) -> bool {
        false
    }

    /// Paint one wrapped line of the tool's summary into `out`. Returns
    /// `true` if the renderer handled it (the tui-side Lua bridge calls
    /// the tool's `render_summary` callback); `false` means "paint as
    /// plain text". Default `false` for fallback / test renderers.
    fn render_summary_line(
        &self,
        _name: &str,
        _line: &str,
        _args: &HashMap<String, serde_json::Value>,
        _out: &mut LineBuilder,
    ) -> bool {
        false
    }

    /// Paint zero or more rows below the summary line (e.g. `web_fetch`
    /// renders the prompt as a dim wrapped subline). Returns the row
    /// count painted. Default `0`.
    fn render_subhead(
        &self,
        _name: &str,
        _args: &HashMap<String, serde_json::Value>,
        _width: usize,
        _out: &mut LineBuilder,
    ) -> u16 {
        0
    }

    /// Optional dim-styled badge painted in the row-0 suffix slot
    /// (between the elapsed-time pill and the line break). `bash` uses
    /// it for `(timeout: 2m)` while the command is pending. `status`
    /// is the lowercase tool status (`"pending" | "ok" | …`).
    fn header_suffix(
        &self,
        _name: &str,
        _args: &HashMap<String, serde_json::Value>,
        _status: &str,
    ) -> Option<String> {
        None
    }
}

/// Simple heuristic: does this look like a `/command` line?
/// Used by the headless prompt path before the Lua command registry
/// is reachable.
pub fn is_command_like(text: &str) -> bool {
    let name = text
        .strip_prefix('/')
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("");
    !name.is_empty()
}

/// Element types for spacing calculation.
pub enum Element<'a> {
    Block(&'a Block),
}

/// Number of blank lines to insert between two adjacent elements.
pub fn gap_between(above: &Element, below: &Element) -> u16 {
    match (above, below) {
        // CodeLine→CodeLine: no gap (consecutive lines in same block).
        (Element::Block(Block::CodeLine { .. }), Element::Block(Block::CodeLine { .. })) => {
            return 0
        }
        // Transitions into/out of code lines need a blank line,
        // except after headings (headings have no trailing gap).
        (Element::Block(Block::CodeLine { .. }), _) => return 1,
        (Element::Block(Block::Text { content }), Element::Block(Block::CodeLine { .. })) => {
            let last_line = content.lines().last().unwrap_or("");
            if last_line.trim_start().starts_with('#') {
                return 0;
            }
            return 1;
        }
        (_, Element::Block(Block::CodeLine { .. })) => return 1,
        _ => {}
    }
    match (above, below) {
        (Element::Block(Block::User { .. }), _) => 1,
        (_, Element::Block(Block::User { .. })) => 1,
        (Element::Block(Block::Exec { .. }), _) => 1,
        (_, Element::Block(Block::Exec { .. })) => 1,
        (Element::Block(Block::ToolCall { .. }), Element::Block(Block::ToolCall { .. })) => 1,
        (Element::Block(Block::Text { .. }), Element::Block(Block::ToolCall { .. })) => 1,
        (Element::Block(Block::Thinking { .. }), Element::Block(Block::Thinking { .. })) => 0,
        (_, Element::Block(Block::Thinking { .. })) => 1,
        (Element::Block(Block::Thinking { .. }), _) => 1,
        (Element::Block(Block::ToolCall { .. }), Element::Block(Block::Text { .. })) => 1,
        (_, Element::Block(Block::Compacted { .. })) => 1,
        (Element::Block(Block::Compacted { .. }), _) => 1,

        // Text→Text: 1 gap (paragraph spacing), except when the previous
        // text block ends with a markdown heading — headings do not get a
        // trailing blank line.
        (Element::Block(Block::Text { content }), Element::Block(Block::Text { .. })) => {
            let last_line = content.lines().last().unwrap_or("");
            if last_line.trim_start().starts_with('#') {
                0
            } else {
                1
            }
        }
        _ => 0,
    }
}
