//! `Block::ToolCall` renderer — pulls everything heavy from
//! `super::tools` (header pill, summary line, body output, status
//! glyphs). This file is a thin call site so the `render_block`
//! dispatch table stays one line per variant.

use smelt_core::content::layout_out::SpanCollector;
use smelt_core::transcript_model::{ToolState, ToolStatus};

use super::tools::render_tool;
use super::ToolBodyRenderer;
use std::collections::HashMap;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub(super) fn render(
    out: &mut SpanCollector,
    call_id: &str,
    name: &str,
    summary: &str,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    state: &ToolState,
    width: usize,
    renderer: Option<&dyn ToolBodyRenderer>,
) -> u16 {
    render_tool(
        out,
        call_id,
        name,
        summary,
        args,
        status,
        elapsed,
        state.output.as_deref(),
        state.user_message.as_deref(),
        width,
        renderer,
    )
}
