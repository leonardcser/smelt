use crate::app::TuiApp;
use smelt_core::content::stream_parser::ToolStart;
use smelt_core::transcript_model::BlockId;
use std::collections::HashMap;

impl TuiApp {
    pub(crate) fn handle_tool_draft_started(
        &mut self,
        stream_id: String,
        call_id: Option<String>,
        tool_name: Option<String>,
    ) {
        if let Some(block_id) = self
            .conversation
            .start_tool_draft(stream_id, call_id, tool_name)
        {
            self.refresh_tool_draft_summary(block_id);
        }
    }

    pub(crate) fn handle_tool_draft_delta(
        &mut self,
        stream_id: String,
        call_id: Option<String>,
        tool_name: Option<String>,
        delta: String,
    ) {
        let bytes = delta.len();
        self.core.signals.emit_dyn(
            "stream_delta",
            std::rc::Rc::new(smelt_core::signals::StreamDelta {
                kind: "tool_args".to_string(),
                bytes,
                text: delta.clone(),
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
            }),
        );
        if let Some((block_id, presentation_changed)) = self
            .conversation
            .append_tool_draft(stream_id, call_id, tool_name, delta)
        {
            if presentation_changed {
                self.refresh_tool_draft_summary(block_id);
            }
        }
    }

    pub(crate) fn handle_tool_draft_finished(
        &mut self,
        stream_id: String,
        call_id: String,
        tool_name: String,
        arguments: String,
    ) {
        if let Some(block_id) = self
            .conversation
            .finish_tool_draft(stream_id, call_id, tool_name, arguments)
        {
            self.refresh_tool_draft_summary(block_id);
        }
    }

    pub(crate) fn promote_tool_draft(
        &mut self,
        invocation_id: protocol::InvocationId,
        call_id: String,
        tool_name: String,
        summary: protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
        called_at_ms: u64,
    ) -> bool {
        let (stream_id, finished) = self.conversation.tool_draft_state(&call_id);
        let preview_output = if finished {
            let lua = self.lua.execution();
            crate::lua::scope_app(self, || lua.tool_preview_output(&tool_name, &args))
        } else {
            None
        };
        self.conversation.promote_tool_draft(
            stream_id.clone(),
            ToolStart {
                invocation_id,
                call_id,
                name: tool_name,
                summary,
                args,
                preview_output,
                called_at_ms,
            },
            self.core.clock.instant_now(),
        )
    }

    pub(crate) fn clear_tool_drafts(&mut self) {
        self.conversation.clear_stream_tool_drafts();
    }

    fn refresh_tool_draft_summary(&mut self, block_id: BlockId) {
        let Some((name, args, finished)) = self.conversation.tool_draft_preview(block_id) else {
            return;
        };
        let lua = self.lua.execution();
        let summary = crate::lua::scope_app(self, || {
            crate::app::history::ToolSummaryResolver::new(&lua)
                .resolve_with_context(&name, &args, finished)
        });
        self.conversation.set_tool_draft_summary(block_id, summary);
    }
}
