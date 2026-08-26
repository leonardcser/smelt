use super::*;

impl TuiApp {
    pub(crate) fn begin_turn(&mut self) {
        self.conversation.begin_turn();
    }

    pub(crate) fn push_block(&mut self, block: Block) {
        let appended = self.try_push_block(block);
        debug_assert!(appended);
    }

    pub(in crate::app) fn try_push_block(&mut self, block: Block) -> bool {
        self.conversation.append_block(block)
    }

    pub(crate) fn append_streaming_thinking(&mut self, delta: &str) {
        self.conversation
            .append_streaming_thinking(delta.to_string());
    }

    pub(crate) fn flush_streaming_thinking(&mut self) {
        self.conversation.flush_streaming_thinking();
    }

    pub(crate) fn append_streaming_text(&mut self, delta: &str) {
        self.conversation.append_streaming_text(delta.to_string());
    }

    pub(crate) fn flush_streaming_text(&mut self) {
        self.conversation.flush_streaming_text();
    }

    pub(crate) fn update_compaction_preview(&mut self, summary: String) {
        let follow_tail = self.transcript_win().is_following_tail();
        let existing = self.conversation.transcript_compaction_preview_id();
        let Some(id) = self.conversation.update_compaction_preview(summary) else {
            return;
        };
        if existing.is_none() {
            let width = self.transcript_width() as u16;
            self.conversation.fold_transcript_node(
                &self.lua,
                width,
                crate::content::transcript_scene::RenderNodeId::Block(id),
                crate::content::transcript_buf::FoldAction::Peek,
            );
        }
        if follow_tail {
            self.transcript_win_mut().follow_tail();
        }
        self.request_urgent_render();
    }

    pub(crate) fn clear_compaction_preview(&mut self) {
        self.conversation.clear_compaction_preview();
    }

    pub(crate) fn start_tool_at(
        &mut self,
        invocation_id: protocol::InvocationId,
        call_id: String,
        name: String,
        summary: ::protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
        called_at_ms: u64,
    ) {
        self.conversation.start_tool(
            smelt_core::content::stream_parser::ToolStart {
                invocation_id,
                call_id,
                name,
                summary,
                args,
                preview_output: None,
                called_at_ms,
            },
            self.core.clock.instant_now(),
        );
    }

    pub(crate) fn start_exec(&mut self, command: String) {
        self.conversation.start_exec(command);
        self.request_urgent_render();
    }

    pub(crate) fn append_exec_output(&mut self, chunk: String) {
        self.transcript_work.push_append_exec_output(chunk);
        self.request_continuation_render();
    }

    pub(crate) fn finish_exec(&mut self, final_output: Option<String>) {
        self.transcript_work.push_finish_exec(final_output);
        self.request_urgent_render();
    }

    pub(crate) fn has_active_exec(&self) -> bool {
        self.conversation.has_active_exec()
    }

    pub(crate) fn append_active_output_line(
        &mut self,
        invocation_id: protocol::InvocationId,
        line: String,
    ) {
        if let Some(pending) = self
            .conversation
            .append_tool_output_line(invocation_id, line)
        {
            self.transcript_work.push_front_tool_output(pending);
            self.request_continuation_render();
        }
    }

    pub(crate) fn set_active_status(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
    ) {
        let now = self.core.clock.instant_now();
        self.conversation
            .set_tool_status(invocation_id, status, now);
    }

    pub(crate) fn set_active_user_message(
        &mut self,
        invocation_id: protocol::InvocationId,
        msg: String,
    ) {
        self.conversation.set_tool_user_message(invocation_id, msg);
    }

    pub(crate) fn finish_tool(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        let now = self.core.clock.instant_now();
        self.conversation
            .finish_tool(invocation_id, status, output, engine_elapsed, now);
    }
}

impl TuiApp {
    pub(crate) fn has_transcript_content(&mut self) -> bool {
        !self.conversation.transcript().is_empty()
    }

    /// Explicit loaded transcript materialization for APIs/tests that request the
    /// currently loaded post-render display text. Do not use for normal viewport
    /// rendering.
    pub(crate) fn materialize_loaded_transcript_display_rows_expensive(
        &mut self,
    ) -> Arc<Vec<String>> {
        let _perf = smelt_perf::perf::begin("transcript:materialize_rows_loaded:explicit");
        self.sync_transcript_renderer_generation();
        let width = self.transcript_width() as u16;
        let anchors = self.capture_transcript_view_anchors(width);
        let theme = self.ui.theme().clone();
        let rows = self
            .conversation
            .build_transcript_rows(&self.lua, width, &theme);
        self.restore_transcript_view_anchors(width, anchors);
        rows
    }

    pub(super) fn capture_transcript_view_anchors(&mut self, width: u16) -> TranscriptViewAnchors {
        let scroll = self.window_scroll_snapshot(crate::app::TRANSCRIPT_WIN);
        let (following_tail, pinned_to_tail, scroll_top, cursor, selection_anchor, drag_endpoint) = {
            let win = self.transcript_win();
            let state = win.document_view_state();
            let following_tail = scroll.as_ref().is_some_and(|scroll| scroll.follow);
            let has_document_selection =
                state.selection_anchor.is_some() || state.drag_endpoint.is_some();
            (
                following_tail,
                scroll.as_ref().is_some_and(|scroll| {
                    !scroll.follow
                        && scroll.viewport > 0
                        && scroll.at_bottom
                        && !win.selection_active()
                        && !has_document_selection
                }),
                win.scroll_top(),
                state.cursor,
                state.selection_anchor,
                state.drag_endpoint,
            )
        };
        let search_current = self
            .overlays
            .search_session()
            .filter(|session| session.target == self.well_known.transcript)
            .and_then(|session| match &session.backend {
                crate::app::search::SearchBackend::Transcript(transcript) => transcript
                    .current
                    .and_then(|index| transcript.matches.get(index).copied())
                    .map(|matched| (matched, session.query.clone())),
                crate::app::search::SearchBackend::Full { .. } => None,
            })
            .map(|(matched, query)| {
                self.conversation
                    .transcript_search_range_anchor(matched, query)
            });
        TranscriptViewAnchors {
            following_tail,
            pinned_to_tail,
            scroll_top: (!following_tail && !pinned_to_tail).then(|| {
                self.conversation.transcript_position_anchor(
                    &self.lua,
                    width,
                    crate::smelt_edit::DocPosition {
                        row: scroll_top,
                        byte_col: 0,
                    },
                )
            }),
            cursor_screen_row: cursor.row.checked_sub(scroll_top),
            cursor: Some(
                self.conversation
                    .transcript_position_anchor(&self.lua, width, cursor),
            ),
            selection_anchor: selection_anchor.map(|position| {
                self.conversation
                    .transcript_position_anchor(&self.lua, width, position)
            }),
            drag_endpoint: drag_endpoint.map(|position| {
                self.conversation
                    .transcript_position_anchor(&self.lua, width, position)
            }),
            search_current,
        }
    }

    pub(super) fn restore_transcript_view_anchors(
        &mut self,
        width: u16,
        anchors: TranscriptViewAnchors,
    ) {
        let theme = self.ui.theme().clone();
        let cursor = anchors.cursor.map(|anchor| {
            self.conversation
                .resolve_transcript_position_anchor(&self.lua, width, anchor)
        });
        let scroll_top = cursor
            .as_ref()
            .and_then(|cursor| {
                anchors
                    .cursor_screen_row
                    .map(|screen_row| cursor.row.saturating_sub(screen_row))
            })
            .or_else(|| {
                anchors.scroll_top.map(|anchor| {
                    self.conversation
                        .resolve_transcript_position_anchor(&self.lua, width, anchor)
                        .row
                })
            });
        let selection_anchor = anchors.selection_anchor.map(|anchor| {
            self.conversation
                .resolve_transcript_position_anchor(&self.lua, width, anchor)
        });
        let drag_endpoint = anchors.drag_endpoint.map(|anchor| {
            self.conversation
                .resolve_transcript_position_anchor(&self.lua, width, anchor)
        });
        let search_current = anchors.search_current.map(|anchor| {
            self.conversation
                .resolve_transcript_search_range_anchor(&self.lua, width, &theme, anchor)
        });

        if let Some(win) = self.ui.win_mut(self.well_known.transcript) {
            let restored_scroll_top = selection_anchor
                .as_ref()
                .map(|position| position.row)
                .or(scroll_top);
            let mut state = win.document_view_state();
            if let Some(cursor) = cursor {
                state.cursor = cursor;
            }
            state.selection_anchor = selection_anchor;
            state.drag_endpoint = drag_endpoint;
            win.set_document_view_state(state);
            if anchors.following_tail || anchors.pinned_to_tail {
                win.follow_tail();
            } else if let Some(row) = restored_scroll_top {
                win.pin_scroll(row);
            }
        }

        if let Some(matched) = search_current {
            self.overlays
                .replace_current_transcript_search_match(matched);
        }
    }
}
