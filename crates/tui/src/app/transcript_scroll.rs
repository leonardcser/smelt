use crate::app::transcript::{TranscriptProjectionHint, TranscriptProjectionRestore};
use crate::app::transcript_scroll_trace::{
    TranscriptScrollIntent, TranscriptScrollTraceRenderInput,
};
use crate::app::TuiApp;
use crate::smelt_edit::RowIndex;
use serde_json::json;

impl TuiApp {
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
        intent: TranscriptScrollIntent,
        window_scroll_before: RowIndex,
        mut restore: TranscriptProjectionRestore,
        local_scroll_top: Option<RowIndex>,
        hint: Option<TranscriptProjectionHint>,
    ) {
        let label = label.into();
        if matches!(intent, TranscriptScrollIntent::UserDelta { .. })
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
        if !matches!(intent, TranscriptScrollIntent::Tail) {
            if let Some(win) = self.ui.win_mut(crate::app::TRANSCRIPT_WIN) {
                win.pin_current_scroll();
            }
        }
        self.transcript.set_pending_projection_with_hint(
            intent.clone(),
            restore,
            local_scroll_top,
            hint,
        );
        if !self.transcript.scroll_trace_enabled() {
            return;
        }
        let window_scroll_after_input = self.transcript_scroll_top();
        self.transcript.record_scroll_trace_event(
            "scroll_intent_input",
            json!({
                "label": &label,
                "intent": format!("{:?}", intent),
                "window_scroll_before": window_scroll_before,
                "window_scroll_after_input": window_scroll_after_input,
            }),
        );
        self.transcript
            .set_next_scroll_trace_input(TranscriptScrollTraceRenderInput {
                input_event_or_tick: label,
                scroll_intent: intent,
                window_scroll_before,
                window_scroll_after_input,
            });
    }
}
