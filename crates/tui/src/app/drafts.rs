use crate::app::TuiApp;
use smelt_core::content::stream_parser::{ToolDraftUpdate, ToolStart};
use std::collections::{HashMap, HashSet};
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
    render_state: DraftRenderState,
}

#[derive(Default)]
struct DraftRenderState {
    last_render: Option<Instant>,
    rendered_raw_args: bool,
    rendered_arg_keys: HashSet<String>,
    pending_throttled_render: bool,
}

impl DraftRenderState {
    fn can_render(&self, now: Instant, force: bool) -> bool {
        force
            || self
                .last_render
                .is_none_or(|last| now.saturating_duration_since(last) >= DRAFT_RENDER_INTERVAL)
    }

    fn mark_rendered(&mut self, now: Instant) {
        self.last_render = Some(now);
        self.pending_throttled_render = false;
    }

    fn mark_throttled(&mut self) {
        self.pending_throttled_render = true;
    }

    fn pending_deadline(&self) -> Option<Instant> {
        if self.pending_throttled_render {
            self.last_render.map(|last| last + DRAFT_RENDER_INTERVAL)
        } else {
            None
        }
    }
}

impl ToolDraft {
    fn has_args(&self) -> bool {
        !self.preview.raw_arguments.is_empty() || !self.preview.args.is_empty()
    }

    fn has_new_arg_keys(&self) -> bool {
        self.preview
            .args
            .keys()
            .any(|key| !self.render_state.rendered_arg_keys.contains(key))
    }

    fn mark_rendered(&mut self, now: Instant) {
        self.render_state.mark_rendered(now);
        self.render_state.rendered_raw_args |= self.has_args();
        self.render_state
            .rendered_arg_keys
            .extend(self.preview.args.keys().cloned());
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

    fn finished_for_call(&self, call_id: &str) -> bool {
        self.drafts
            .values()
            .any(|draft| draft.call_id.as_deref() == Some(call_id) && draft.finished)
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
        let has_args = draft.has_args();
        let force = force_render
            || finished
            || (has_args && !draft.render_state.rendered_raw_args)
            || draft.has_new_arg_keys();
        let should_render = draft.render_state.can_render(now, force);
        if should_render {
            draft.mark_rendered(now);
            Some(draft.snapshot(stream_id))
        } else {
            if has_args {
                draft.render_state.mark_throttled();
            }
            None
        }
    }

    fn drain_due_renders(&mut self, now: Instant) -> Vec<ToolDraftSnapshot> {
        let stream_ids: Vec<String> = self
            .drafts
            .iter()
            .filter(|(_, draft)| {
                draft
                    .render_state
                    .pending_deadline()
                    .is_some_and(|deadline| deadline <= now)
            })
            .map(|(stream_id, _)| stream_id.clone())
            .collect();

        let mut snapshots = Vec::with_capacity(stream_ids.len());
        for stream_id in stream_ids {
            if let Some(draft) = self.drafts.get_mut(&stream_id) {
                draft.mark_rendered(now);
                snapshots.push(draft.snapshot(stream_id));
            }
        }
        snapshots
    }

    fn next_render_delay(&self, now: Instant) -> Option<Duration> {
        self.drafts
            .values()
            .filter_map(|draft| draft.render_state.pending_deadline())
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
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
        let preview_output = self
            .draft_tools
            .finished_for_call(&call_id)
            .then(|| self.lua.tool_preview_output(&tool_name, &args))
            .flatten();
        let promoted = self
            .apply_session_document_mutation(
                crate::app::session_document::SessionMutation::PromoteToolDraft {
                    stream_id: stream_id.clone(),
                    start: ToolStart {
                        call_id,
                        name: tool_name,
                        summary,
                        args,
                        preview_output,
                    },
                    now: self.core.clock.instant_now(),
                },
            )
            .applied;
        if promoted {
            if let Some(stream_id) = stream_id {
                self.draft_tools.remove_by_stream_id(&stream_id);
            }
        }
        promoted
    }

    pub(crate) fn clear_tool_drafts(&mut self) {
        self.draft_tools.clear();
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::ClearToolDrafts,
        );
    }

    pub(crate) fn flush_due_tool_drafts(&mut self) -> bool {
        let snapshots = self
            .draft_tools
            .drain_due_renders(self.core.clock.instant_now());
        let did_work = !snapshots.is_empty();
        for snapshot in snapshots {
            self.render_tool_draft(snapshot);
        }
        did_work
    }

    pub(crate) fn next_tool_draft_render_delay(&self) -> Option<Duration> {
        self.draft_tools
            .next_render_delay(self.core.clock.instant_now())
    }

    fn render_tool_draft(&mut self, snapshot: ToolDraftSnapshot) {
        let name = snapshot.tool_name.unwrap_or_else(|| "tool".to_string());
        let args = snapshot.args;
        let summary = crate::app::history::ToolSummaryResolver::new(&self.lua)
            .resolve_with_context(&name, &args, snapshot.finished);
        self.apply_session_document_mutation(
            crate::app::session_document::SessionMutation::UpsertToolDraft {
                update: ToolDraftUpdate {
                    stream_id: snapshot.stream_id,
                    call_id: snapshot.call_id,
                    name,
                    summary,
                    args,
                    raw_arguments: snapshot.raw_arguments,
                    finished: snapshot.finished,
                },
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
    fn first_argument_delta_renders_even_inside_throttle() {
        let mut controller = ToolDraftController::default();
        let now = Instant::now();

        let initial = controller.update(
            ToolDraftEvent {
                stream_id: "s".into(),
                call_id: Some("c".into()),
                tool_name: Some("bash".into()),
                delta: None,
                arguments: None,
                finished: false,
            },
            now,
            true,
        );
        assert!(initial.is_some());

        let update = controller
            .update(
                ToolDraftEvent {
                    stream_id: "s".into(),
                    call_id: Some("c".into()),
                    tool_name: Some("bash".into()),
                    delta: Some(r#"{"command":"echo hi"#.into()),
                    arguments: None,
                    finished: false,
                },
                now,
                false,
            )
            .expect("first argument content should render immediately");

        assert_eq!(
            update.args.get("command").and_then(|value| value.as_str()),
            Some("echo hi")
        );
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
    fn new_argument_delta_renders_inside_throttle() {
        let mut controller = ToolDraftController::default();
        let now = Instant::now();

        controller.update(
            ToolDraftEvent {
                stream_id: "s".into(),
                call_id: Some("c".into()),
                tool_name: Some("write_file".into()),
                delta: None,
                arguments: None,
                finished: false,
            },
            now,
            true,
        );

        let path_update = controller.update(
            ToolDraftEvent {
                stream_id: "s".into(),
                call_id: Some("c".into()),
                tool_name: Some("write_file".into()),
                delta: Some(r#"{"file_path":"src/live.rs","#.into()),
                arguments: None,
                finished: false,
            },
            now,
            false,
        );
        assert!(path_update.is_some());

        let content_update = controller
            .update(
                ToolDraftEvent {
                    stream_id: "s".into(),
                    call_id: Some("c".into()),
                    tool_name: Some("write_file".into()),
                    delta: Some(r#""content":"pub fn live() -> i32 { 1 }"#.into()),
                    arguments: None,
                    finished: false,
                },
                now,
                false,
            )
            .expect("new argument should render inside throttle window");

        assert_eq!(
            content_update
                .args
                .get("content")
                .and_then(|value| value.as_str()),
            Some("pub fn live() -> i32 { 1 }")
        );
    }

    #[test]
    fn delayed_flush_renders_throttled_argument_updates() {
        let mut controller = ToolDraftController::default();
        let now = Instant::now();

        controller.update(
            ToolDraftEvent {
                stream_id: "s".into(),
                call_id: Some("c".into()),
                tool_name: Some("write_file".into()),
                delta: None,
                arguments: None,
                finished: false,
            },
            now,
            true,
        );

        let first_update = controller
            .update(
                ToolDraftEvent {
                    stream_id: "s".into(),
                    call_id: Some("c".into()),
                    tool_name: Some("write_file".into()),
                    delta: Some(r#"{"content":"a"#.into()),
                    arguments: None,
                    finished: false,
                },
                now,
                false,
            )
            .expect("first argument content should render immediately");
        assert_eq!(
            first_update
                .args
                .get("content")
                .and_then(|value| value.as_str()),
            Some("a")
        );

        let throttled = controller.update(
            ToolDraftEvent {
                stream_id: "s".into(),
                call_id: Some("c".into()),
                tool_name: Some("write_file".into()),
                delta: Some("b".into()),
                arguments: None,
                finished: false,
            },
            now,
            false,
        );
        assert!(throttled.is_none());
        assert_eq!(
            controller.next_render_delay(now),
            Some(DRAFT_RENDER_INTERVAL)
        );

        assert!(controller
            .drain_due_renders(now + DRAFT_RENDER_INTERVAL - Duration::from_millis(1))
            .is_empty());

        let due = controller.drain_due_renders(now + DRAFT_RENDER_INTERVAL);
        assert_eq!(due.len(), 1);
        assert_eq!(
            due[0].args.get("content").and_then(|value| value.as_str()),
            Some("ab")
        );
        assert!(controller
            .drain_due_renders(now + DRAFT_RENDER_INTERVAL)
            .is_empty());
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
