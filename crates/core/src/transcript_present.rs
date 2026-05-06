//! Tool-body renderer trait: the seam between transcript layout
//! (core) and the Lua-driven render hook that paints the body of
//! each tool block (tui).
//!
//! The actual per-block rendering (markdown / tools / view-state
//! collapse) lives in `tui::content::transcript_parsers`. The trait
//! survives here so `transcript_model.rs` can hold an
//! `Arc<dyn ToolBodyRenderer>` without depending on tui.

use crate::content::builder::LineBuilder;
use crate::transcript_model::ToolOutput;
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

