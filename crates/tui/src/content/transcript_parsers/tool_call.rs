//! `Block::ToolCall` renderer - thin delegation to `super::tools::render_tool`.

use smelt_core::content::builder::LineBuilder;
use smelt_core::transcript_model::{ToolState, ToolStatus};

use super::tools::render_tool;
use std::collections::HashMap;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub(super) fn render(
    out: &mut LineBuilder,
    _call_id: &str,
    name: &str,
    summary: &protocol::StyledLines,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    state: &ToolState,
    width: usize,
) -> u16 {
    // Cache hit only when the cached width matches the current layout width - a resize
    // invalidates without us having to track it explicitly.
    let rendered = state
        .render_cache
        .as_ref()
        .filter(|(w, _)| *w as usize == width)
        .map(|(_, layout)| layout);
    render_tool(
        out,
        name,
        summary,
        args,
        status,
        elapsed,
        state.output.as_deref(),
        state.user_message.as_deref(),
        rendered,
        width,
    )
}
