use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use super::{AppFocus, TuiApp};

static PROMPT_TRACE_ENABLED: OnceLock<bool> = OnceLock::new();

pub(crate) struct PromptInsertCheck {
    ch: char,
    before_text: String,
    before_cpos: usize,
}

impl TuiApp {
    pub(crate) fn prompt_trace_enabled(&self) -> bool {
        *PROMPT_TRACE_ENABLED.get_or_init(|| {
            std::env::var("SMELT_PROMPT_TRACE")
                .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
                .unwrap_or(false)
        })
    }

    pub(crate) fn trace_prompt_event(&self, label: &str, data: Value) {
        if !self.prompt_trace_enabled() {
            return;
        }
        engine::log::entry(
            engine::log::Level::Info,
            "prompt_trace",
            &serde_json::json!({
                "label": label,
                "prompt": self.prompt_trace_snapshot(),
                "data": data,
            }),
        );
    }

    pub(crate) fn prompt_text_hash(text: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        hasher.finish()
    }

    pub(crate) fn prompt_insert_check_for_event(&self, ev: &Event) -> Option<PromptInsertCheck> {
        let Event::Key(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            ..
        }) = ev
        else {
            return None;
        };
        if !matches!(*modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) {
            return None;
        }
        if ch.is_control() || *ch == smelt_buffer::ATTACHMENT_MARKER {
            return None;
        }
        if self.app_focus != AppFocus::Prompt
            || self.ui.focused_overlay().is_some()
            || self.ui.active_modal().is_some()
            || self.well_known.cmdline.is_some()
            || !self.term_focused
        {
            return None;
        }
        let ctx = crate::input::prompt_ctx_ref(&self.ui);
        if ctx.win.vim_enabled && ctx.win.vim_mode != crate::smelt_term::VimMode::Insert {
            return None;
        }
        if self.input.selection_range(ctx).is_some() {
            return None;
        }
        Some(PromptInsertCheck {
            ch: *ch,
            before_text: ctx.buf.source().to_string(),
            before_cpos: ctx.win.cpos,
        })
    }

    pub(crate) fn check_prompt_insert_after_event(&self, check: &PromptInsertCheck) {
        if !self.prompt_trace_enabled() {
            return;
        }
        let mut expected = check.before_text.clone();
        let insert_at = smelt_buffer::text::snap(&expected, check.before_cpos);
        expected.insert(insert_at, check.ch);
        let expected_cpos = insert_at + check.ch.len_utf8();
        let actual = self.prompt_buf().source();
        let actual_cpos = self.prompt_win().cpos;
        let text_matches = actual == expected;
        let cursor_matches = actual_cpos == expected_cpos;
        let label = if text_matches && !cursor_matches {
            "prompt_printable_cursor_suspect"
        } else if !text_matches {
            "prompt_printable_text_rewritten"
        } else {
            "prompt_printable_ok"
        };
        self.trace_prompt_event(
            label,
            serde_json::json!({
                "char": check.ch.to_string(),
                "before_cpos": check.before_cpos,
                "insert_at": insert_at,
                "expected_cpos": expected_cpos,
                "actual_cpos": actual_cpos,
                "expected_len": expected.len(),
                "actual_len": actual.len(),
                "expected_hash": Self::prompt_text_hash(&expected),
                "actual_hash": Self::prompt_text_hash(actual),
                "text_matches": text_matches,
                "cursor_matches": cursor_matches,
            }),
        );
    }

    fn prompt_trace_snapshot(&self) -> Value {
        let source = self.prompt_buf().source();
        let cpos = self.prompt_win().cpos;
        serde_json::json!({
            "len": source.len(),
            "hash": Self::prompt_text_hash(source),
            "cpos": cpos,
            "preview": prompt_preview(source),
            "cursor_context": prompt_cursor_context(source, cpos),
            "app_focus": format!("{:?}", self.app_focus),
            "vim_mode": format!("{:?}", self.prompt_win().vim_mode),
            "focused_overlay": self.ui.focused_overlay().is_some(),
            "active_modal": self.ui.active_modal().is_some(),
            "cmdline_open": self.well_known.cmdline.is_some(),
            "picker_count": self.picker_state.len(),
            "agent_running": self.agent_is_running(),
            "busy": self.busy_stack.is_busy(),
            "pending_chord": self.timers.pending_chord.is_some(),
            "pending_pane_chord": self.timers.pending_pane_chord.is_some(),
            "app_sequence": self.timers.app_sequence.has_pending(),
            "term_focused": self.term_focused,
        })
    }
}

fn prompt_preview(source: &str) -> String {
    const LIMIT: usize = 220;
    let mut out: String = source.chars().take(LIMIT).collect();
    if source.chars().nth(LIMIT).is_some() {
        out.push('…');
    }
    out
}

fn prompt_cursor_context(source: &str, cpos: usize) -> String {
    let cpos = smelt_buffer::text::snap(source, cpos);
    let start = cpos.saturating_sub(80);
    let end = (cpos + 80).min(source.len());
    smelt_buffer::text::slice(source, start..end).to_string()
}
