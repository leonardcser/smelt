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
    let rendered = state
        .render_cache
        .as_ref()
        .filter(|(w, layout)| {
            *w as usize == width
                || smelt_core::content::block_layout::rendered_layout_width_independent(layout)
        })
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
