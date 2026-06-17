use crate::app::TuiApp;
use smelt_core::content::stream_parser::{ToolDraftUpdate, ToolStart};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_DRAFT_STRING_BYTES: usize = 200_000;

#[derive(Default)]
pub(crate) struct ToolDraftController {
    drafts: HashMap<String, ToolDraft>,
}

const DRAFT_RENDER_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct ToolDraft {
    call_id: Option<String>,
    tool_name: Option<String>,
    preview: DraftJsonPreview,
    finished: bool,
    last_render: Option<Instant>,
}

impl ToolDraft {
    fn should_render(&mut self, now: Instant, force: bool) -> bool {
        if force
            || self
                .last_render
                .is_none_or(|last| now.saturating_duration_since(last) >= DRAFT_RENDER_INTERVAL)
        {
            self.last_render = Some(now);
            true
        } else {
            false
        }
    }

    fn snapshot(&self, stream_id: String) -> ToolDraftSnapshot {
        ToolDraftSnapshot {
            stream_id,
            call_id: self.call_id.clone(),
            tool_name: self.tool_name.clone(),
            args: self.preview.args.clone(),
            raw_arguments: self.preview.raw_arguments(),
            finished: self.finished,
        }
    }
}

impl ToolDraftController {
    pub(crate) fn clear(&mut self) {
        self.drafts.clear();
    }

    pub(crate) fn stream_id_for_call(&self, call_id: &str) -> Option<String> {
        self.drafts.iter().find_map(|(stream_id, draft)| {
            (draft.call_id.as_deref() == Some(call_id)).then(|| stream_id.clone())
        })
    }

    fn update(
        &mut self,
        event: ToolDraftEvent,
        now: Instant,
        force_render: bool,
    ) -> Option<ToolDraftSnapshot> {
        let ToolDraftEvent {
            stream_id,
            call_id,
            tool_name,
            delta,
            arguments,
            finished,
        } = event;
        let draft = self.drafts.entry(stream_id.clone()).or_default();
        if call_id.as_deref().is_some_and(|s| !s.is_empty()) {
            draft.call_id = call_id;
        }
        if tool_name.as_deref().is_some_and(|s| !s.is_empty()) {
            draft.tool_name = tool_name;
        }
        if let Some(arguments) = arguments {
            draft.preview.replace(&arguments);
        } else if let Some(delta) = delta {
            draft.preview.append(&delta);
        }
        draft.finished |= finished;
        draft
            .should_render(now, force_render || finished)
            .then(|| draft.snapshot(stream_id))
    }

    fn remove_by_stream_id(&mut self, stream_id: &str) {
        self.drafts.remove(stream_id);
    }
}

struct ToolDraftEvent {
    stream_id: String,
    call_id: Option<String>,
    tool_name: Option<String>,
    delta: Option<String>,
    arguments: Option<String>,
    finished: bool,
}

struct ToolDraftSnapshot {
    stream_id: String,
    call_id: Option<String>,
    tool_name: Option<String>,
    args: HashMap<String, serde_json::Value>,
    raw_arguments: String,
    finished: bool,
}

impl TuiApp {
    pub(crate) fn handle_tool_draft_started(
        &mut self,
        stream_id: String,
        call_id: Option<String>,
        tool_name: Option<String>,
    ) {
        let event = ToolDraftEvent {
            stream_id,
            call_id,
            tool_name,
            delta: None,
            arguments: None,
            finished: false,
        };
        if let Some(snapshot) = self
            .draft_tools
            .update(event, self.core.clock.instant_now(), true)
        {
            self.render_tool_draft(snapshot);
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
        self.core.cells.set_dyn(
            "stream_delta",
            std::rc::Rc::new(smelt_core::cells::StreamDelta {
                kind: "tool_args".to_string(),
                bytes,
                text: delta.clone(),
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
            }),
        );
        let event = ToolDraftEvent {
            stream_id,
            call_id,
            tool_name,
            delta: Some(delta),
            arguments: None,
            finished: false,
        };
        if let Some(snapshot) = self
            .draft_tools
            .update(event, self.core.clock.instant_now(), false)
        {
            self.render_tool_draft(snapshot);
        }
    }

    pub(crate) fn handle_tool_draft_finished(
        &mut self,
        stream_id: String,
        call_id: String,
        tool_name: String,
        arguments: String,
    ) {
        let event = ToolDraftEvent {
            stream_id,
            call_id: Some(call_id),
            tool_name: Some(tool_name),
            delta: None,
            arguments: Some(arguments),
            finished: true,
        };
        if let Some(snapshot) = self
            .draft_tools
            .update(event, self.core.clock.instant_now(), true)
        {
            self.render_tool_draft(snapshot);
        }
    }

    pub(crate) fn promote_tool_draft(
        &mut self,
        call_id: String,
        tool_name: String,
        summary: protocol::StyledLines,
        args: HashMap<String, serde_json::Value>,
    ) -> bool {
        let stream_id = self.draft_tools.stream_id_for_call(&call_id);
        let promoted = self.parser.promote_tool_draft(
            self.transcript.history_mut(),
            stream_id.as_deref(),
            ToolStart {
                call_id,
                name: tool_name,
                summary,
                args,
            },
            self.core.clock.instant_now(),
        );
        if promoted {
            if let Some(stream_id) = stream_id {
                self.draft_tools.remove_by_stream_id(&stream_id);
            }
        }
        promoted
    }

    pub(crate) fn clear_tool_drafts(&mut self) {
        self.draft_tools.clear();
        self.parser.clear_tool_drafts(self.transcript.history_mut());
    }

    fn render_tool_draft(&mut self, snapshot: ToolDraftSnapshot) {
        let name = snapshot.tool_name.unwrap_or_else(|| "tool".to_string());
        let args = snapshot.args;
        let summary =
            crate::app::history::ToolSummaryResolver::new(&self.lua).resolve(&name, &args);
        self.parser.upsert_tool_draft(
            self.transcript.history_mut(),
            ToolDraftUpdate {
                stream_id: snapshot.stream_id,
                call_id: snapshot.call_id,
                name,
                summary,
                args,
                raw_arguments: snapshot.raw_arguments,
                finished: snapshot.finished,
            },
        );
    }
}

#[derive(Default)]
struct DraftJsonPreview {
    state: PreviewState,
    args: HashMap<String, serde_json::Value>,
    raw_arguments: String,
    raw_truncated: bool,
    key: String,
    string_value: String,
    raw_value: String,
    escaped: bool,
    nested_depth: usize,
    nested_in_string: bool,
    nested_escaped: bool,
}

#[derive(Default)]
enum PreviewState {
    #[default]
    BeforeObject,
    BeforeKey,
    InKey,
    AfterKey,
    BeforeValue,
    InString,
    InBare,
    InNested,
    AfterValue,
    Done,
}

impl DraftJsonPreview {
    fn append(&mut self, delta: &str) {
        self.push_raw(delta);
        for ch in delta.chars() {
            self.push_char(ch);
        }
    }

    fn replace(&mut self, arguments: &str) {
        *self = Self::default();
        self.append(arguments);
        if arguments.len() <= MAX_DRAFT_STRING_BYTES {
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(arguments)
            {
                self.args = map
                    .into_iter()
                    .map(|(key, value)| (key, cap_value(value)))
                    .collect();
            }
        }
    }

    fn raw_arguments(&self) -> String {
        if self.raw_truncated {
            let mut out = self.raw_arguments.clone();
            out.push_str("\n… draft truncated …");
            out
        } else {
            self.raw_arguments.clone()
        }
    }

    fn push_raw(&mut self, delta: &str) {
        if self.raw_arguments.len() >= MAX_DRAFT_STRING_BYTES {
            self.raw_truncated = true;
            return;
        }
        let remaining = MAX_DRAFT_STRING_BYTES - self.raw_arguments.len();
        if delta.len() <= remaining {
            self.raw_arguments.push_str(delta);
        } else {
            self.raw_arguments
                .push_str(smelt_buffer::text::slice(delta, 0..remaining));
            self.raw_truncated = true;
        }
    }

    fn push_char(&mut self, ch: char) {
        match self.state {
            PreviewState::BeforeObject => {
                if ch == '{' {
                    self.state = PreviewState::BeforeKey;
                }
            }
            PreviewState::BeforeKey => {
                if ch.is_whitespace() || ch == ',' {
                    return;
                }
                match ch {
                    '"' => {
                        self.key.clear();
                        self.escaped = false;
                        self.state = PreviewState::InKey;
                    }
                    '}' => self.state = PreviewState::Done,
                    _ => {}
                }
            }
            PreviewState::InKey => {
                if self.push_string_char(ch, StringTarget::Key) {
                    self.state = PreviewState::AfterKey;
                }
            }
            PreviewState::AfterKey => {
                if ch.is_whitespace() {
                    return;
                }
                if ch == ':' {
                    self.state = PreviewState::BeforeValue;
                } else if ch == ',' {
                    self.state = PreviewState::BeforeKey;
                }
            }
            PreviewState::BeforeValue => {
                if ch.is_whitespace() {
                    return;
                }
                match ch {
                    '"' => {
                        self.string_value.clear();
                        self.escaped = false;
                        self.state = PreviewState::InString;
                    }
                    '{' | '[' => {
                        self.raw_value.clear();
                        self.push_raw_value_char(ch);
                        self.nested_depth = 1;
                        self.nested_in_string = false;
                        self.nested_escaped = false;
                        self.state = PreviewState::InNested;
                    }
                    '}' => self.state = PreviewState::Done,
                    _ => {
                        self.raw_value.clear();
                        self.push_raw_value_char(ch);
                        self.state = PreviewState::InBare;
                    }
                }
            }
            PreviewState::InString => {
                let terminated = self.push_string_char(ch, StringTarget::Value);
                self.args.insert(
                    self.key.clone(),
                    serde_json::Value::String(self.string_value.clone()),
                );
                if terminated {
                    self.state = PreviewState::AfterValue;
                }
            }
            PreviewState::InBare => {
                if ch == ',' || ch == '}' {
                    self.commit_raw_value();
                    self.state = if ch == '}' {
                        PreviewState::Done
                    } else {
                        PreviewState::BeforeKey
                    };
                } else {
                    self.push_raw_value_char(ch);
                }
            }
            PreviewState::InNested => {
                self.push_raw_value_char(ch);
                self.advance_nested(ch);
            }
            PreviewState::AfterValue => {
                if ch.is_whitespace() {
                    return;
                }
                match ch {
                    ',' => self.state = PreviewState::BeforeKey,
                    '}' => self.state = PreviewState::Done,
                    _ => {}
                }
            }
            PreviewState::Done => {}
        }
    }

    fn push_string_char(&mut self, ch: char, target: StringTarget) -> bool {
        if self.escaped {
            let decoded = match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '"' => '"',
                '\\' => '\\',
                other => other,
            };
            self.push_decoded_string_char(decoded, target);
            self.escaped = false;
            return false;
        }
        match ch {
            '\\' => {
                self.escaped = true;
                false
            }
            '"' => true,
            other => {
                self.push_decoded_string_char(other, target);
                false
            }
        }
    }

    fn push_decoded_string_char(&mut self, ch: char, target: StringTarget) {
        let buf = match target {
            StringTarget::Key => &mut self.key,
            StringTarget::Value => &mut self.string_value,
        };
        if buf.len() < MAX_DRAFT_STRING_BYTES {
            buf.push(ch);
            if buf.len() > MAX_DRAFT_STRING_BYTES {
                let capped = smelt_buffer::text::slice(buf, 0..MAX_DRAFT_STRING_BYTES).to_string();
                *buf = capped;
            }
        }
    }

    fn push_raw_value_char(&mut self, ch: char) {
        if self.raw_value.len() < MAX_DRAFT_STRING_BYTES {
            self.raw_value.push(ch);
            if self.raw_value.len() > MAX_DRAFT_STRING_BYTES {
                let capped = smelt_buffer::text::slice(&self.raw_value, 0..MAX_DRAFT_STRING_BYTES)
                    .to_string();
                self.raw_value = capped;
            }
        }
    }

    fn commit_raw_value(&mut self) {
        let raw = self.raw_value.trim();
        if raw.is_empty() {
            return;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
            self.args.insert(self.key.clone(), cap_value(value));
        }
    }

    fn advance_nested(&mut self, ch: char) {
        if self.nested_in_string {
            if self.nested_escaped {
                self.nested_escaped = false;
            } else if ch == '\\' {
                self.nested_escaped = true;
            } else if ch == '"' {
                self.nested_in_string = false;
            }
            return;
        }
        match ch {
            '"' => self.nested_in_string = true,
            '{' | '[' => self.nested_depth += 1,
            '}' | ']' => {
                self.nested_depth = self.nested_depth.saturating_sub(1);
                if self.nested_depth == 0 {
                    self.commit_raw_value();
                    self.state = PreviewState::AfterValue;
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
enum StringTarget {
    Key,
    Value,
}

fn cap_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(truncate_utf8(&s, MAX_DRAFT_STRING_BYTES))
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(cap_value).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, cap_value(value)))
                .collect(),
        ),
        other => other,
    }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut out = smelt_buffer::text::slice(s, 0..max_bytes).to_string();
    out.push_str("\n… draft truncated …");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview_args(chunks: &[&str]) -> HashMap<String, serde_json::Value> {
        let mut preview = DraftJsonPreview::default();
        for chunk in chunks {
            preview.append(chunk);
        }
        preview.args
    }

    #[test]
    fn partial_json_reads_incomplete_string_value() {
        let args = preview_args(&[r#"{"file_path":"/tmp/a","content":"hello"#]);
        assert_eq!(
            args.get("file_path").and_then(|v| v.as_str()),
            Some("/tmp/a")
        );
        assert_eq!(args.get("content").and_then(|v| v.as_str()), Some("hello"));
    }

    #[test]
    fn complete_json_wins() {
        let args = preview_args(&[r#"{"command":"echo hi","timeout_ms":1000}"#]);
        assert_eq!(
            args.get("command").and_then(|v| v.as_str()),
            Some("echo hi")
        );
        assert_eq!(args.get("timeout_ms").and_then(|v| v.as_u64()), Some(1000));
    }

    #[test]
    fn previewer_reads_split_string_values() {
        let args = preview_args(&[r#"{"command":"echo"#, " hi", r#"","timeout_ms":10}"#]);
        assert_eq!(
            args.get("command").and_then(|v| v.as_str()),
            Some("echo hi")
        );
        assert_eq!(args.get("timeout_ms").and_then(|v| v.as_u64()), Some(10));
    }
}
