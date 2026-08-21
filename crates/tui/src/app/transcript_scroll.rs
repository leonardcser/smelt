use crate::app::transcript::{TranscriptProjectionHint, TranscriptProjectionRestore};
use crate::app::transcript_scroll_trace::{
    TranscriptScrollIntent, TranscriptScrollTraceRenderInput,
};
use crate::app::TuiApp;
use crate::smelt_edit::{RowIndex, WinId};
use serde_json::json;

pub(crate) enum WindowScrollCommand {
    Pin(RowIndex),
    Tail,
}

pub(crate) struct WindowScrollSnapshot {
    pub(crate) top: RowIndex,
    pub(crate) follow: bool,
    pub(crate) total: RowIndex,
    pub(crate) viewport: u16,
    pub(crate) max: RowIndex,
    pub(crate) overflow: bool,
    pub(crate) at_bottom: bool,
    pub(crate) needs_tail_repin: bool,
}

impl TuiApp {
    pub(crate) fn window_scroll_snapshot(&self, window_id: WinId) -> Option<WindowScrollSnapshot> {
        let window = self.ui.win(window_id)?;
        let total = self
            .ui
            .buf(window.buf)
            .map(|buffer| window.scroll_row_total(buffer))
            .unwrap_or(0);
        let viewport = window
            .viewport
            .map(|viewport| viewport.rect.height)
            .unwrap_or(0);
        let max = total.saturating_sub(RowIndex::from(viewport));
        let top = window.scroll_top().min(max);
        let overflow = total > RowIndex::from(viewport);
        let numeric_at_bottom = top >= max;
        let semantic_needs_tail_repin = window_id == crate::app::TRANSCRIPT_WIN
            && self.conversation.transcript().needs_tail_repin();
        let needs_tail_repin = overflow && (semantic_needs_tail_repin || !numeric_at_bottom);
        Some(WindowScrollSnapshot {
            top,
            follow: window.is_following_tail(),
            total,
            viewport,
            max,
            overflow,
            at_bottom: numeric_at_bottom && !semantic_needs_tail_repin,
            needs_tail_repin,
        })
    }

    pub(crate) fn scroll_window(&mut self, win_id: WinId, scroll: WindowScrollCommand) {
        let Some(win) = self.ui.win(win_id) else {
            return;
        };
        let is_transcript = win_id == crate::app::TRANSCRIPT_WIN;
        let window_scroll_before = is_transcript.then(|| self.transcript_scroll_top());
        let buf_id = win.buf;

        match scroll {
            WindowScrollCommand::Pin(target) => {
                let viewport_rows = win.viewport.map(|v| v.rect.height).unwrap_or(0);
                let resolved_scroll = {
                    let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
                    if let (Some(win), Some(buf)) = (win, buf) {
                        win.scroll_to_preserving_cursor_screen_row(target, buf, viewport_rows);
                        win.pin_current_scroll();
                        Some(win.scroll_top())
                    } else {
                        None
                    }
                };
                if let (true, Some(scroll_top), Some(window_scroll_before)) =
                    (is_transcript, resolved_scroll, window_scroll_before)
                {
                    self.record_transcript_scroll_intent(
                        "lua_scroll",
                        TranscriptScrollIntent::ExactContentAnchor(
                            crate::app::transcript_scroll_trace::TranscriptTraceAnchor::EstimatedRow(
                                scroll_top,
                            ),
                        ),
                        window_scroll_before,
                    );
                    self.request_urgent_render();
                }
            }
            WindowScrollCommand::Tail => {
                if let (true, Some(window_scroll_before)) = (is_transcript, window_scroll_before) {
                    self.record_transcript_scroll_intent(
                        "lua_scroll_tail",
                        TranscriptScrollIntent::Tail,
                        window_scroll_before,
                    );
                    self.request_urgent_render();
                    return;
                }

                let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
                if let (Some(win), Some(buf)) = (win, buf) {
                    win.jump_to_bottom(buf);
                }
            }
        }
    }

    pub(crate) fn record_transcript_scroll_intent(
        &mut self,
        label: impl Into<String>,
        intent: TranscriptScrollIntent,
        window_scroll_before: RowIndex,
    ) {
        self.record_transcript_scroll_intent_for_projection(
            label,
            intent,
            window_scroll_before,
            TranscriptProjectionRestore::default(),
            None,
            None,
        );
    }

    pub(crate) fn record_transcript_scroll_intent_with_hint(
        &mut self,
        label: impl Into<String>,
        intent: TranscriptScrollIntent,
        window_scroll_before: RowIndex,
        hint: TranscriptProjectionHint,
    ) {
        self.record_transcript_scroll_intent_for_projection(
            label,
            intent,
            window_scroll_before,
            TranscriptProjectionRestore::default(),
            None,
            Some(hint),
        );
    }

    pub(crate) fn record_transcript_scroll_intent_from_document_command(
        &mut self,
        label: impl Into<String>,
        intent: TranscriptScrollIntent,
        window_scroll_before: RowIndex,
        restore: TranscriptProjectionRestore,
        local_scroll_top: Option<RowIndex>,
    ) {
        self.record_transcript_scroll_intent_for_projection(
            label,
            intent,
            window_scroll_before,
            restore,
            local_scroll_top,
            None,
        );
    }

    pub(crate) fn record_transcript_scroll_intent_for_projection(
        &mut self,
        label: impl Into<String>,
        mut intent: TranscriptScrollIntent,
        window_scroll_before: RowIndex,
        mut restore: TranscriptProjectionRestore,
        local_scroll_top: Option<RowIndex>,
        hint: Option<TranscriptProjectionHint>,
    ) {
        let label = label.into();
        if matches!(&intent, TranscriptScrollIntent::UserDelta { .. })
            && restore.cursor_screen_row.is_none()
        {
            restore.cursor_screen_row = self
                .ui
                .win(crate::app::TRANSCRIPT_WIN)
                .filter(|win| {
                    win.selection_active() && win.document_view_state().drag_endpoint.is_none()
                })
                .and_then(|win| {
                    win.viewport
                        .and_then(|v| win.cursor_screen_row(v.rect.height))
                });
        }
        if intent.is_downward_local_delta()
            && self
                .ui
                .win(crate::app::TRANSCRIPT_WIN)
                .is_some_and(|win| win.is_following_tail() && !win.selection_active())
        {
            intent = TranscriptScrollIntent::Tail;
        }
        let keep_tail_follow_until_projection = intent.is_explicit_tail_intent()
            && self
                .ui
                .win(crate::app::TRANSCRIPT_WIN)
                .is_some_and(|win| win.is_following_tail());
        if !matches!(&intent, TranscriptScrollIntent::Tail) && !keep_tail_follow_until_projection {
            if let Some(win) = self.ui.win_mut(crate::app::TRANSCRIPT_WIN) {
                win.pin_current_scroll();
            }
        }
        self.conversation.set_pending_transcript_projection(
            intent.clone(),
            restore,
            local_scroll_top,
            hint,
        );
        if !self.conversation.transcript().scroll_trace_enabled() {
            return;
        }
        let window_scroll_after_input = self.transcript_scroll_top();
        self.conversation.record_transcript_scroll_trace_event(
            "scroll_intent_input",
            json!({
                "label": &label,
                "intent": format!("{:?}", intent),
                "window_scroll_before": window_scroll_before,
                "window_scroll_after_input": window_scroll_after_input,
            }),
        );
        self.conversation.set_next_transcript_scroll_trace_input(
            TranscriptScrollTraceRenderInput {
                input_event_or_tick: label,
                scroll_intent: intent,
                window_scroll_before,
                window_scroll_after_input,
            },
        );
    }
}
