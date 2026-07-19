//! Append-only conversation history.
//!
//! `HistoryItem` is the canonical in-memory + on-disk representation of a
//! conversation. It supersedes the older [`crate::message::Message`] struct,
//! which now lives on as the *wire* format for OpenAI/Anthropic requests.
//!
//! The shape encodes one invariant the rest of the codebase used to enforce
//! by discipline: **an assistant step that invoked tools carries every tool
//! result inline**. There is no way to construct an `AssistantStep` with a
//! `ToolInvocation` whose `result` is missing, so the engine cannot leave the
//! history in a half-applied state mid-tool - the bug pattern that produced
//! "tool_call_id … did not have response messages" errors on resumed
//! sessions.
//!
//! Mid-flight UI state (streaming text, in-progress tool calls) is *not*
//! represented here. The engine emits `*Delta`, `ToolStarted`,
//! `ToolFinished`, and `StepCommitted` events for that. `HistoryItem` only
//! ever holds committed, complete steps.

use crate::content::Content;
use crate::message::{FunctionCall, ReasoningBlock, ToolCall, ToolOutcome};
use serde::{Deserialize, Serialize};

pub const COMPACTION_SUMMARY_PREFIX: &str = include_str!("compact_summary_prefix.md");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryItem {
    System {
        content: Content,
    },
    User {
        content: Content,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        display: Option<String>,
    },
    Assistant(AssistantStep),
    Note(HistoryNote),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProcessStatusEvent {
    BackgroundProcessCompleted {
        process_id: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        exit_code: Option<i32>,
    },
}

impl ProcessStatusEvent {
    pub fn background_process_completed(
        process_id: impl Into<String>,
        exit_code: Option<i32>,
    ) -> Self {
        Self::BackgroundProcessCompleted {
            process_id: process_id.into(),
            exit_code,
        }
    }

    pub fn event_type(&self) -> &'static str {
        match self {
            Self::BackgroundProcessCompleted { .. } => "background_process_completed",
        }
    }

    pub fn process_id(&self) -> Option<&str> {
        match self {
            Self::BackgroundProcessCompleted { process_id, .. } => Some(process_id),
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::BackgroundProcessCompleted { exit_code, .. } => *exit_code,
        }
    }

    pub fn field_value(&self, field: &str) -> Option<String> {
        match field {
            "event" | "event_type" => Some(self.event_type().to_string()),
            "process_id" => self.process_id().map(str::to_string),
            "exit_code" => self.exit_code().map(|code| code.to_string()),
            _ => None,
        }
    }

    pub fn snapshot_json_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = serde_json::Map::new();
        let event_type = self.event_type();
        fields.insert("event".into(), serde_json::json!(event_type));
        fields.insert("event_type".into(), serde_json::json!(event_type));
        if let Ok(event_data) = serde_json::to_value(self) {
            fields.insert("event_data".into(), event_data);
        }
        if let Some(process_id) = self.process_id() {
            fields.insert("process_id".into(), serde_json::json!(process_id));
        }
        if let Some(exit_code) = self.exit_code() {
            fields.insert("exit_code".into(), serde_json::json!(exit_code));
        }
        fields
    }

    pub fn display_text(&self) -> String {
        match self {
            Self::BackgroundProcessCompleted {
                process_id,
                exit_code,
            } => {
                let status = match exit_code {
                    Some(0) => "finished successfully".to_string(),
                    Some(code) => format!("exited with code {code}"),
                    None => "exited".to_string(),
                };
                format!("background process {process_id} {status}")
            }
        }
    }
}

pub const DEFAULT_CONTEXT_NOTE_NAME: &str = "cwd";

fn default_context_note_name() -> String {
    DEFAULT_CONTEXT_NOTE_NAME.to_string()
}

fn is_default_context_note_name(name: &str) -> bool {
    name == DEFAULT_CONTEXT_NOTE_NAME
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "note_kind", rename_all = "snake_case")]
pub enum HistoryNote {
    ModeChange {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        mode: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        base_mode: Option<String>,
        text: String,
    },
    Context {
        #[serde(
            default = "default_context_note_name",
            skip_serializing_if = "is_default_context_note_name"
        )]
        name: String,
        text: String,
    },
    ProcessStatus {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        event: Option<ProcessStatusEvent>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryNoteKind {
    ModeChange,
    Context,
    ProcessStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum HistoryAppendPolicy {
    Append,
    ReplaceNoteKind { kind: HistoryNoteKind },
    ReplaceContextNote { name: String },
    RemoveContextNote { name: String },
    ModeChange { base: crate::mode::AgentMode },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryAppend {
    pub item: HistoryItem,
    pub policy: HistoryAppendPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAppendResult {
    Unchanged,
    Pushed,
    ReplacedLast,
    RemovedLast,
}

impl HistoryAppendPolicy {
    pub fn replacement_note_kind(&self) -> Option<HistoryNoteKind> {
        match self {
            Self::Append => None,
            Self::ReplaceNoteKind { kind } => Some(*kind),
            Self::ReplaceContextNote { .. } | Self::RemoveContextNote { .. } => {
                Some(HistoryNoteKind::Context)
            }
            Self::ModeChange { .. } => Some(HistoryNoteKind::ModeChange),
        }
    }
}

impl HistoryAppend {
    pub fn append(item: HistoryItem) -> Self {
        Self {
            item,
            policy: HistoryAppendPolicy::Append,
        }
    }

    pub fn replace_note_kind(item: HistoryItem, kind: HistoryNoteKind) -> Self {
        Self {
            item,
            policy: HistoryAppendPolicy::ReplaceNoteKind { kind },
        }
    }

    pub fn replace_context_note(item: HistoryItem, name: impl Into<String>) -> Self {
        Self {
            item,
            policy: HistoryAppendPolicy::ReplaceContextNote { name: name.into() },
        }
    }

    pub fn remove_context_note(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            item: HistoryItem::note(HistoryNote::named_context(name.clone(), String::new())),
            policy: HistoryAppendPolicy::RemoveContextNote { name },
        }
    }

    pub fn mode_change(item: HistoryItem, base: crate::mode::AgentMode) -> Self {
        Self {
            item,
            policy: HistoryAppendPolicy::ModeChange { base },
        }
    }

    pub fn replacement_note_kind(&self) -> Option<HistoryNoteKind> {
        self.policy.replacement_note_kind()
    }
}

impl HistoryNote {
    pub fn mode_change(text: impl Into<String>) -> Self {
        Self::ModeChange {
            mode: None,
            base_mode: None,
            text: text.into(),
        }
    }

    pub fn mode_change_for_mode(mode: impl Into<String>, text: impl Into<String>) -> Self {
        Self::ModeChange {
            mode: Some(mode.into()),
            base_mode: None,
            text: text.into(),
        }
    }

    pub fn mode_change_for_transition(
        base_mode: impl Into<String>,
        mode: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::ModeChange {
            mode: Some(mode.into()),
            base_mode: Some(base_mode.into()),
            text: text.into(),
        }
    }

    pub fn context(text: impl Into<String>) -> Self {
        Self::named_context(DEFAULT_CONTEXT_NOTE_NAME, text)
    }

    pub fn named_context(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Context {
            name: name.into(),
            text: text.into(),
        }
    }

    pub fn process_status(text: impl Into<String>) -> Self {
        Self::ProcessStatus {
            text: text.into(),
            event: None,
        }
    }

    pub fn process_status_event(event: ProcessStatusEvent) -> Self {
        Self::ProcessStatus {
            text: event.display_text(),
            event: Some(event),
        }
    }

    pub fn process_status_with_event(text: impl Into<String>, event: ProcessStatusEvent) -> Self {
        Self::ProcessStatus {
            text: text.into(),
            event: Some(event),
        }
    }

    pub fn kind(&self) -> HistoryNoteKind {
        match self {
            HistoryNote::ModeChange { .. } => HistoryNoteKind::ModeChange,
            HistoryNote::Context { .. } => HistoryNoteKind::Context,
            HistoryNote::ProcessStatus { .. } => HistoryNoteKind::ProcessStatus,
        }
    }

    pub fn text(&self) -> &str {
        match self {
            HistoryNote::ModeChange { text, .. }
            | HistoryNote::Context { text, .. }
            | HistoryNote::ProcessStatus { text, .. } => text,
        }
    }

    pub fn mode(&self) -> Option<&str> {
        match self {
            HistoryNote::ModeChange { mode, .. } => mode.as_deref(),
            HistoryNote::Context { .. } | HistoryNote::ProcessStatus { .. } => None,
        }
    }

    pub fn base_mode(&self) -> Option<&str> {
        match self {
            HistoryNote::ModeChange { base_mode, .. } => base_mode.as_deref(),
            HistoryNote::Context { .. } | HistoryNote::ProcessStatus { .. } => None,
        }
    }

    pub fn context_name(&self) -> Option<&str> {
        match self {
            HistoryNote::Context { name, .. } => Some(name),
            HistoryNote::ModeChange { .. } | HistoryNote::ProcessStatus { .. } => None,
        }
    }

    pub fn process_status_event_ref(&self) -> Option<&ProcessStatusEvent> {
        match self {
            HistoryNote::ProcessStatus { event, .. } => event.as_ref(),
            HistoryNote::ModeChange { .. } | HistoryNote::Context { .. } => None,
        }
    }

    pub fn to_model_text(&self) -> String {
        match self {
            HistoryNote::ModeChange { text, .. } => crate::note::mode_change_note(text),
            HistoryNote::Context { text, .. } => crate::note::context_note(text),
            HistoryNote::ProcessStatus { text, .. } => crate::note::process_status_note(text),
        }
    }
}

/// A committed assistant message.
///
/// - `invocations` empty ⇒ terminal step (the assistant produced text /
///   reasoning and the conversation continues with the user).
/// - `invocations` non-empty ⇒ tool step. Every tool the model asked for is
///   in this vec, and each one already has its `result` recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantStep {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub reasoning_blocks: Vec<ReasoningBlock>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub invocations: Vec<ToolInvocation>,
}

impl AssistantStep {
    /// Terminal step - no tool calls. The conversation continues with the
    /// next user message (or ends if the user does nothing).
    pub fn terminal(
        content: Option<Content>,
        reasoning: Option<String>,
        reasoning_blocks: Vec<ReasoningBlock>,
    ) -> Self {
        Self {
            content,
            reasoning,
            reasoning_blocks,
            invocations: Vec::new(),
        }
    }

    /// Tool step - every `ToolCall` in `calls` is paired with the matching
    /// `ToolOutcome` from `results`. Panics in debug if the lengths or
    /// call_ids don't line up; that's a bug in the caller.
    pub fn with_invocations(
        content: Option<Content>,
        reasoning: Option<String>,
        reasoning_blocks: Vec<ReasoningBlock>,
        invocations: Vec<ToolInvocation>,
    ) -> Self {
        Self {
            content,
            reasoning,
            reasoning_blocks,
            invocations,
        }
    }
}

/// One tool call from an assistant step together with its execution result.
///
/// `arguments` is the JSON-encoded argument object the LLM emitted (kept as
/// a string so the wire format round-trips byte-identically).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolInvocation {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub result: ToolOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

impl ToolInvocation {
    pub fn from_call(call: &ToolCall, result: ToolOutcome, elapsed_ms: Option<u64>) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
            result,
            elapsed_ms,
        }
    }

    pub fn as_tool_call(&self) -> ToolCall {
        ToolCall::new(
            self.call_id.clone(),
            FunctionCall {
                name: self.name.clone(),
                arguments: self.arguments.clone(),
            },
        )
    }
}

impl HistoryItem {
    pub fn system(text: impl Into<String>) -> Self {
        HistoryItem::System {
            content: Content::text(text),
        }
    }

    pub fn user(content: Content) -> Self {
        HistoryItem::User {
            content,
            display: None,
        }
    }

    pub fn user_with_display(content: Content, display: impl Into<String>) -> Self {
        HistoryItem::User {
            content,
            display: Some(display.into()),
        }
    }

    pub fn note(note: HistoryNote) -> Self {
        HistoryItem::Note(note)
    }

    pub fn note_kind(&self) -> Option<HistoryNoteKind> {
        match self {
            HistoryItem::Note(note) => Some(note.kind()),
            _ => None,
        }
    }

    pub fn is_transcript_visible(&self) -> bool {
        !matches!(
            self,
            HistoryItem::System { .. } | HistoryItem::Note(HistoryNote::Context { .. })
        )
    }

    pub fn note_text(&self) -> Option<&str> {
        match self {
            HistoryItem::Note(note) => Some(note.text()),
            _ => None,
        }
    }

    pub fn as_note(&self) -> Option<&HistoryNote> {
        match self {
            HistoryItem::Note(note) => Some(note),
            _ => None,
        }
    }

    pub fn assistant(turn: AssistantStep) -> Self {
        HistoryItem::Assistant(turn)
    }

    pub fn as_assistant(&self) -> Option<&AssistantStep> {
        match self {
            HistoryItem::Assistant(turn) => Some(turn),
            _ => None,
        }
    }
}

pub fn replace_last_note_kind(
    items: &mut [HistoryItem],
    item: &HistoryItem,
    kind: HistoryNoteKind,
) -> bool {
    if item.note_kind() != Some(kind) {
        return false;
    }
    let Some(last) = items.last_mut() else {
        return false;
    };
    if last.note_kind() != Some(kind) {
        return false;
    }
    *last = item.clone();
    true
}

pub fn replace_context_note(items: &mut [HistoryItem], item: &HistoryItem, name: &str) -> bool {
    let Some(note) = item.as_note() else {
        return false;
    };
    if note.context_name() != Some(name) {
        return false;
    }
    let Some(existing) = items.iter_mut().rev().find(|existing| {
        existing
            .as_note()
            .and_then(HistoryNote::context_name)
            .is_some_and(|context_name| context_name == name)
    }) else {
        return false;
    };
    *existing = item.clone();
    true
}

pub fn remove_context_note(items: &mut Vec<HistoryItem>, name: &str) -> bool {
    let Some(idx) = items.iter().rposition(|existing| {
        existing
            .as_note()
            .and_then(HistoryNote::context_name)
            .is_some_and(|context_name| context_name == name)
    }) else {
        return false;
    };
    items.remove(idx);
    true
}

pub fn apply_history_append(
    items: &mut Vec<HistoryItem>,
    append: &HistoryAppend,
) -> HistoryAppendResult {
    match &append.policy {
        HistoryAppendPolicy::Append => {
            items.push(append.item.clone());
            HistoryAppendResult::Pushed
        }
        HistoryAppendPolicy::ReplaceNoteKind { kind } => {
            if replace_last_note_kind(items, &append.item, *kind) {
                HistoryAppendResult::ReplacedLast
            } else {
                items.push(append.item.clone());
                HistoryAppendResult::Pushed
            }
        }
        HistoryAppendPolicy::ReplaceContextNote { name } => {
            if append.item.as_note().and_then(HistoryNote::context_name) != Some(name.as_str()) {
                return HistoryAppendResult::Unchanged;
            }
            if replace_context_note(items, &append.item, name) {
                HistoryAppendResult::ReplacedLast
            } else {
                items.push(append.item.clone());
                HistoryAppendResult::Pushed
            }
        }
        HistoryAppendPolicy::RemoveContextNote { name } => {
            if remove_context_note(items, name) {
                HistoryAppendResult::RemovedLast
            } else {
                HistoryAppendResult::Unchanged
            }
        }
        HistoryAppendPolicy::ModeChange { base } => {
            append_mode_change(items, append.item.clone(), base)
        }
    }
}

fn append_mode_change(
    items: &mut Vec<HistoryItem>,
    item: HistoryItem,
    mode_base: &crate::mode::AgentMode,
) -> HistoryAppendResult {
    let Some(new_mode) = item
        .as_note()
        .and_then(HistoryNote::mode)
        .map(str::to_string)
    else {
        if replace_last_note_kind(items, &item, HistoryNoteKind::ModeChange) {
            return HistoryAppendResult::ReplacedLast;
        }
        items.push(item);
        return HistoryAppendResult::Pushed;
    };

    let fallback = item
        .as_note()
        .and_then(HistoryNote::base_mode)
        .unwrap_or(mode_base.as_str());
    let last_mode = items
        .last()
        .is_some_and(|last| last.note_kind() == Some(HistoryNoteKind::ModeChange));

    if last_mode {
        let last_has_mode = items
            .last()
            .and_then(HistoryItem::as_note)
            .and_then(HistoryNote::mode)
            .is_some();
        if last_has_mode && new_mode == effective_mode_at(items, items.len() - 1, fallback) {
            items.pop();
            return HistoryAppendResult::RemovedLast;
        }
        *items.last_mut().expect("last mode note") = item;
        return HistoryAppendResult::ReplacedLast;
    }

    if new_mode == effective_mode_at(items, items.len(), fallback) {
        return HistoryAppendResult::Unchanged;
    }

    items.push(item);
    HistoryAppendResult::Pushed
}

pub fn effective_mode_at<'a>(
    items: &'a [HistoryItem],
    hist_idx: usize,
    fallback: &'a str,
) -> &'a str {
    let end = hist_idx.min(items.len());
    items[..end]
        .iter()
        .rev()
        .filter_map(HistoryItem::as_note)
        .find_map(HistoryNote::mode)
        .or_else(|| {
            items[end..]
                .iter()
                .filter_map(HistoryItem::as_note)
                .find_map(HistoryNote::base_mode)
        })
        .unwrap_or(fallback)
}

// Provider-wire `Vec<Message>` ↔ semantic `Vec<HistoryItem>` conversion.
//
// `history_to_messages` builds provider requests. `history_from_messages` accepts
// host-hook replacements that still speak the provider message shape and repairs
// orphan tool_use blocks by synthesizing an "interrupted" result.

use crate::message::{Message, Role};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserHistoryContent {
    Plain,
    CompactionSummary { summary: String },
    ModeChange { text: String },
    ProcessStatus { text: String },
}

pub fn classify_user_history_content(content: &Content) -> UserHistoryContent {
    let text = content.text_content();
    if let Some(summary) = text.strip_prefix(COMPACTION_SUMMARY_PREFIX.trim_end()) {
        return UserHistoryContent::CompactionSummary {
            summary: summary.trim_start_matches('\n').to_string(),
        };
    }
    if let Some(note) = text.strip_prefix(crate::note::MODE_NOTE_PREFIX) {
        return UserHistoryContent::ModeChange {
            text: note.trim().to_string(),
        };
    }
    if let Some(note) = text.strip_prefix(crate::note::PROCESS_STATUS_NOTE_PREFIX) {
        return UserHistoryContent::ProcessStatus {
            text: note.trim().to_string(),
        };
    }
    UserHistoryContent::Plain
}

pub fn compaction_summary_content(summary: &str) -> Content {
    Content::text(format!(
        "{}\n{summary}",
        COMPACTION_SUMMARY_PREFIX.trim_end()
    ))
}

pub fn note_from_user_content(content: &Content) -> Option<HistoryNote> {
    match classify_user_history_content(content) {
        UserHistoryContent::ModeChange { text } => Some(HistoryNote::mode_change(text)),
        UserHistoryContent::ProcessStatus { text } => Some(HistoryNote::process_status(text)),
        UserHistoryContent::Plain | UserHistoryContent::CompactionSummary { .. } => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryTranscriptProjection {
    User,
    Assistant,
    Mode,
    ProcessStatus,
    Compacted,
}

impl HistoryTranscriptProjection {
    fn from_descriptor_kind(kind: &str) -> Option<Self> {
        match kind {
            "user" => Some(Self::User),
            "assistant" | "thinking" | "tool" | "exec" | "code" => Some(Self::Assistant),
            "mode" => Some(Self::Mode),
            "process_status" => Some(Self::ProcessStatus),
            "compacted" => Some(Self::Compacted),
            _ => None,
        }
    }
}

fn history_item_transcript_projection(item: &HistoryItem) -> Option<HistoryTranscriptProjection> {
    match item {
        HistoryItem::User { content, .. } => match classify_user_history_content(content) {
            UserHistoryContent::Plain => Some(HistoryTranscriptProjection::User),
            UserHistoryContent::CompactionSummary { .. } => {
                Some(HistoryTranscriptProjection::Compacted)
            }
            UserHistoryContent::ModeChange { .. } => Some(HistoryTranscriptProjection::Mode),
            UserHistoryContent::ProcessStatus { .. } => {
                Some(HistoryTranscriptProjection::ProcessStatus)
            }
        },
        HistoryItem::Assistant(_) => Some(HistoryTranscriptProjection::Assistant),
        HistoryItem::Note(note) => match note.kind() {
            HistoryNoteKind::ModeChange => Some(HistoryTranscriptProjection::Mode),
            HistoryNoteKind::ProcessStatus => Some(HistoryTranscriptProjection::ProcessStatus),
            HistoryNoteKind::Context => None,
        },
        HistoryItem::System { .. } => None,
    }
}

pub fn transcript_descriptor_kind_matches_history_item(
    descriptor_kind: &str,
    item: &HistoryItem,
) -> bool {
    let Some(descriptor_projection) =
        HistoryTranscriptProjection::from_descriptor_kind(descriptor_kind)
    else {
        return false;
    };
    history_item_transcript_projection(item) == Some(descriptor_projection)
}

/// Convert user-role wire content into semantic history.
///
/// Reserved `[smelt:*]` prefixes are model-visible encodings for internal
/// notes, not real user turns.
pub fn history_item_from_user_content(content: Content) -> HistoryItem {
    match note_from_user_content(&content) {
        Some(note) => HistoryItem::Note(note),
        None => HistoryItem::User {
            content,
            display: None,
        },
    }
}

/// Fold provider-wire messages into semantic history.
///
/// This is used when host hooks replace a request with provider messages.
/// Pairs each assistant message that has `tool_calls` with the immediately
/// following `Role::Tool` messages by `tool_call_id`. Any `tool_call` whose
/// id isn't satisfied by a following tool message gets a synthetic
/// "interrupted (resumed)" result so the result is loss-bounded on disk and
/// LLM requests never go out with orphaned tool_use blocks.
pub fn history_from_messages(messages: Vec<Message>) -> Vec<HistoryItem> {
    let mut out: Vec<HistoryItem> = Vec::with_capacity(messages.len());
    let mut i = 0usize;
    while i < messages.len() {
        let m = &messages[i];
        match m.role {
            Role::System => {
                if let Some(c) = m.content.clone() {
                    out.push(HistoryItem::System { content: c });
                }
                i += 1;
            }
            Role::User => {
                if let Some(c) = m.content.clone() {
                    out.push(history_item_from_user_content(c));
                }
                i += 1;
            }
            Role::Assistant => {
                let calls: Vec<ToolCall> = m.tool_calls.clone().unwrap_or_default();
                // Collect Role::Tool messages directly following this
                // assistant. Pair by call_id.
                let mut results_by_id: std::collections::HashMap<
                    String,
                    (String, bool, Option<serde_json::Value>),
                > = std::collections::HashMap::new();
                let mut j = i + 1;
                while j < messages.len() && matches!(messages[j].role, Role::Tool) {
                    if let (Some(id), Some(content)) = (
                        messages[j].tool_call_id.clone(),
                        messages[j].content.clone(),
                    ) {
                        results_by_id.insert(
                            id,
                            (
                                content.as_text().to_string(),
                                messages[j].is_error,
                                messages[j].tool_metadata.clone(),
                            ),
                        );
                    }
                    j += 1;
                }
                let invocations = calls
                    .into_iter()
                    .map(|tc| {
                        let (content, is_error, metadata) =
                            results_by_id.remove(&tc.id).unwrap_or_else(|| {
                                (
                                    "interrupted (resumed): no recorded tool result".into(),
                                    true,
                                    None,
                                )
                            });
                        ToolInvocation {
                            call_id: tc.id,
                            name: tc.function.name,
                            arguments: tc.function.arguments,
                            result: ToolOutcome {
                                content,
                                is_error,
                                metadata,
                            },
                            elapsed_ms: None,
                        }
                    })
                    .collect::<Vec<_>>();
                out.push(HistoryItem::Assistant(AssistantStep {
                    content: m.content.clone(),
                    reasoning: m.reasoning_content.clone(),
                    reasoning_blocks: m.reasoning_details.clone().unwrap_or_default(),
                    invocations,
                }));
                i = j;
            }
            Role::Tool => {
                // Stray tool message with no preceding assistant tool_call -
                // drop it. (This can happen in synthetic test fixtures; real
                // sessions never see it.)
                i += 1;
            }
        }
    }
    out
}

/// Render a slice of `HistoryItem`s back into the provider-wire `Vec<Message>`
/// shape. The result satisfies the
/// assistant-tool_calls ↔ tool_call_id pairing invariant by construction.
pub fn history_to_messages(items: &[HistoryItem]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(items.len() * 2);
    for item in items {
        match item {
            HistoryItem::System { content } => {
                out.push(Message::system_content(content.clone()));
            }
            HistoryItem::User { content, .. } => {
                out.push(Message::user(content.clone()));
            }
            HistoryItem::Note(note) => {
                out.push(Message::user(Content::text(note.to_model_text())));
            }
            HistoryItem::Assistant(turn) => {
                let tool_calls = if turn.invocations.is_empty() {
                    None
                } else {
                    Some(
                        turn.invocations
                            .iter()
                            .map(|inv| inv.as_tool_call())
                            .collect(),
                    )
                };
                let reasoning_details = if turn.reasoning_blocks.is_empty() {
                    None
                } else {
                    Some(turn.reasoning_blocks.clone())
                };
                out.push(Message::assistant_with_reasoning(
                    turn.content.clone(),
                    turn.reasoning.clone(),
                    reasoning_details,
                    tool_calls,
                ));
                for inv in &turn.invocations {
                    out.push(Message::tool_with_metadata(
                        inv.call_id.clone(),
                        inv.result.content.clone(),
                        inv.result.is_error,
                        inv.result.metadata.clone(),
                    ));
                }
            }
        }
    }
    out
}

/// Number of provider-wire messages emitted for one semantic history item.
/// Keep this in lockstep with [`history_to_messages`].
pub fn history_item_message_count(item: &HistoryItem) -> usize {
    match item {
        HistoryItem::System { .. } | HistoryItem::User { .. } | HistoryItem::Note(_) => 1,
        HistoryItem::Assistant(turn) => 1 + turn.invocations.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::FunctionCall;

    fn tc(id: &str, name: &str) -> ToolCall {
        ToolCall::new(
            id.into(),
            FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
        )
    }

    fn mode_item(mode: &str) -> HistoryItem {
        HistoryItem::note(HistoryNote::mode_change_for_mode(
            mode,
            format!("now in {mode} mode"),
        ))
    }

    fn transition_item(base: &str, mode: &str) -> HistoryItem {
        HistoryItem::note(HistoryNote::mode_change_for_transition(
            base,
            mode,
            format!("now in {mode} mode"),
        ))
    }

    #[test]
    fn effective_mode_at_uses_transition_base_after_rewind_boundary() {
        let history = vec![
            HistoryItem::user(Content::text("before")),
            transition_item("normal", "apply"),
            HistoryItem::user(Content::text("after")),
        ];

        assert_eq!(effective_mode_at(&history, 1, "apply"), "normal");
        assert_eq!(effective_mode_at(&history, 2, "normal"), "apply");
    }

    #[test]
    fn mode_append_back_to_base_removes_pending_mode_note() {
        let base = crate::mode::AgentMode::parse("normal").unwrap();
        let mut history = vec![HistoryItem::user(Content::text("hello"))];

        assert_eq!(
            apply_history_append(
                &mut history,
                &HistoryAppend::mode_change(mode_item("apply"), base.clone()),
            ),
            HistoryAppendResult::Pushed
        );
        assert_eq!(
            apply_history_append(
                &mut history,
                &HistoryAppend::mode_change(mode_item("normal"), base.clone()),
            ),
            HistoryAppendResult::RemovedLast
        );

        assert_eq!(history, vec![HistoryItem::user(Content::text("hello"))]);
    }

    #[test]
    fn mode_append_replaces_with_distinct_mode() {
        let base = crate::mode::AgentMode::parse("normal").unwrap();
        let mut history = vec![
            HistoryItem::user(Content::text("hello")),
            mode_item("apply"),
        ];

        assert_eq!(
            apply_history_append(
                &mut history,
                &HistoryAppend::mode_change(mode_item("yolo"), base.clone()),
            ),
            HistoryAppendResult::ReplacedLast
        );

        assert_eq!(
            history
                .last()
                .and_then(HistoryItem::as_note)
                .and_then(HistoryNote::mode),
            Some("yolo")
        );
    }

    #[test]
    fn mode_append_matching_effective_mode_is_noop() {
        let base = crate::mode::AgentMode::parse("normal").unwrap();
        let mut history = vec![HistoryItem::user(Content::text("hello"))];

        assert_eq!(
            apply_history_append(
                &mut history,
                &HistoryAppend::mode_change(mode_item("normal"), base.clone()),
            ),
            HistoryAppendResult::Unchanged
        );

        assert_eq!(history, vec![HistoryItem::user(Content::text("hello"))]);
    }

    #[test]
    fn assistant_step_with_no_invocations_round_trips() {
        let turn = AssistantStep::terminal(Some(Content::text("hi")), None, vec![]);
        let item = HistoryItem::Assistant(turn);
        let msgs = history_to_messages(std::slice::from_ref(&item));
        let back = history_from_messages(msgs);
        assert_eq!(back, vec![item]);
    }

    #[test]
    fn assistant_with_invocations_round_trips_through_provider_messages() {
        let inv = ToolInvocation {
            call_id: "call-1".into(),
            name: "f".into(),
            arguments: "{\"x\":1}".into(),
            result: ToolOutcome {
                content: "ok".into(),
                is_error: false,
                metadata: None,
            },
            elapsed_ms: None,
        };
        let item = HistoryItem::Assistant(AssistantStep::with_invocations(
            None,
            None,
            vec![],
            vec![inv],
        ));
        let history = vec![item.clone()];
        let back = history_from_messages(history_to_messages(&history));
        assert_eq!(back, history);
    }

    #[test]
    fn notes_round_trip_through_provider_user_messages() {
        let item = HistoryItem::note(HistoryNote::mode_change("now in apply mode"));
        let messages = history_to_messages(std::slice::from_ref(&item));
        assert!(matches!(messages[0].role, Role::User));
        assert!(messages[0].content.as_ref().is_some_and(|content| content
            .text_content()
            .starts_with(crate::note::MODE_NOTE_PREFIX)));
        assert_eq!(history_from_messages(messages), vec![item]);
    }

    #[test]
    fn user_content_helper_classifies_reserved_notes() {
        let process = history_item_from_user_content(Content::text(
            crate::note::process_status_note("background process 751225 exited with code 1"),
        ));
        assert_eq!(
            process,
            HistoryItem::note(HistoryNote::process_status(
                "background process 751225 exited with code 1"
            ))
        );

        let mode = history_item_from_user_content(Content::text(crate::note::mode_change_note(
            "now in apply mode.",
        )));
        assert_eq!(
            mode,
            HistoryItem::note(HistoryNote::mode_change("now in apply mode."))
        );
    }

    #[test]
    fn compaction_summary_stays_user_history_but_projects_as_compacted() {
        let content = compaction_summary_content("# Goal\nRetain this");
        assert_eq!(
            classify_user_history_content(&content),
            UserHistoryContent::CompactionSummary {
                summary: "# Goal\nRetain this".into()
            }
        );
        let item = history_item_from_user_content(content);
        assert!(matches!(item, HistoryItem::User { .. }));
        assert!(transcript_descriptor_kind_matches_history_item(
            "compacted",
            &item
        ));
        assert!(!transcript_descriptor_kind_matches_history_item(
            "user", &item
        ));
    }

    #[test]
    fn transcript_descriptor_compatibility_uses_semantic_history_projection() {
        let user = HistoryItem::user(Content::text("follow up"));
        let legacy_mode =
            HistoryItem::user(Content::text(crate::note::mode_change_note("apply mode")));
        let legacy_process = HistoryItem::user(Content::text(crate::note::process_status_note(
            "process finished",
        )));
        let mode = HistoryItem::note(HistoryNote::mode_change("apply mode"));
        let process = HistoryItem::note(HistoryNote::process_status("process finished"));
        let context = HistoryItem::note(HistoryNote::context("cwd changed"));

        for (kind, item) in [
            ("user", &user),
            ("mode", &legacy_mode),
            ("process_status", &legacy_process),
            ("mode", &mode),
            ("process_status", &process),
        ] {
            assert!(transcript_descriptor_kind_matches_history_item(kind, item));
        }
        for (kind, item) in [
            ("user", &legacy_mode),
            ("mode", &process),
            ("process_status", &mode),
            ("user", &context),
            ("unknown", &context),
        ] {
            assert!(!transcript_descriptor_kind_matches_history_item(kind, item));
        }
    }

    #[test]
    fn context_notes_use_stable_model_prefix() {
        let note = HistoryNote::context("Current working directory: /work.");
        assert_eq!(
            note.to_model_text(),
            "[smelt:context] Current working directory: /work."
        );
        assert_eq!(
            history_to_messages(&[HistoryItem::note(note.clone())])[0]
                .content
                .as_ref()
                .map(Content::text_content)
                .as_deref(),
            Some("[smelt:context] Current working directory: /work.")
        );
    }

    #[test]
    fn named_context_notes_replace_only_matching_name() {
        let mut history = vec![
            HistoryItem::note(HistoryNote::named_context("cwd", "cwd one")),
            HistoryItem::note(HistoryNote::named_context("goal", "goal one")),
        ];

        assert_eq!(
            apply_history_append(
                &mut history,
                &HistoryAppend::replace_context_note(
                    HistoryItem::note(HistoryNote::named_context("cwd", "cwd two")),
                    "cwd",
                ),
            ),
            HistoryAppendResult::ReplacedLast
        );

        assert_eq!(
            history,
            vec![
                HistoryItem::note(HistoryNote::named_context("cwd", "cwd two")),
                HistoryItem::note(HistoryNote::named_context("goal", "goal one")),
            ]
        );
    }

    #[test]
    fn context_note_policy_name_mismatch_is_unchanged() {
        let mut history = vec![HistoryItem::note(HistoryNote::named_context(
            "cwd", "cwd one",
        ))];

        assert_eq!(
            apply_history_append(
                &mut history,
                &HistoryAppend::replace_context_note(
                    HistoryItem::note(HistoryNote::named_context("goal", "goal one")),
                    "cwd",
                ),
            ),
            HistoryAppendResult::Unchanged
        );

        assert_eq!(
            history,
            vec![HistoryItem::note(HistoryNote::named_context(
                "cwd", "cwd one"
            ))]
        );
    }

    #[test]
    fn removing_named_context_note_leaves_other_names() {
        let mut history = vec![
            HistoryItem::note(HistoryNote::named_context("cwd", "cwd")),
            HistoryItem::note(HistoryNote::named_context("goal", "goal")),
        ];

        assert_eq!(
            apply_history_append(&mut history, &HistoryAppend::remove_context_note("goal")),
            HistoryAppendResult::RemovedLast
        );

        assert_eq!(
            history,
            vec![HistoryItem::note(HistoryNote::named_context("cwd", "cwd"))]
        );
    }

    #[test]
    fn context_note_deserialize_defaults_to_cwd_name() {
        let note: HistoryNote = serde_json::from_value(serde_json::json!({
            "note_kind": "context",
            "text": "Current working directory: /work."
        }))
        .expect("deserialize context note");

        assert_eq!(note.context_name(), Some(DEFAULT_CONTEXT_NOTE_NAME));
    }

    #[test]
    fn ordinary_user_content_stays_user() {
        let item = history_item_from_user_content(Content::text("hello"));
        assert!(
            matches!(item, HistoryItem::User { content, .. } if content.text_content() == "hello")
        );
    }

    #[test]
    fn notes_serialize_without_kind_field_collision() {
        let item = HistoryItem::note(HistoryNote::mode_change_for_mode(
            "apply",
            "now in apply mode",
        ));
        let json = serde_json::to_value(&item).expect("serialize note item");
        assert_eq!(json["kind"], "note");
        assert_eq!(json["note_kind"], "mode_change");
        assert_eq!(json["mode"], "apply");
        assert_eq!(json["text"], "now in apply mode");
        let back: HistoryItem = serde_json::from_value(json).expect("deserialize note item");
        assert_eq!(back, item);
    }

    #[test]
    fn process_status_note_serializes_typed_event() {
        let item = HistoryItem::note(HistoryNote::process_status_event(
            ProcessStatusEvent::background_process_completed("751225", Some(1)),
        ));

        let json = serde_json::to_value(&item).expect("serialize note item");

        assert_eq!(json["kind"], "note");
        assert_eq!(json["note_kind"], "process_status");
        assert_eq!(json["text"], "background process 751225 exited with code 1");
        assert_eq!(json["event"]["event"], "background_process_completed");
        assert_eq!(json["event"]["process_id"], "751225");
        assert_eq!(json["event"]["exit_code"], 1);
        let back: HistoryItem = serde_json::from_value(json).expect("deserialize note item");
        assert_eq!(back, item);
    }
    #[test]
    fn orphan_tool_use_in_provider_messages_is_repaired_with_interrupted_result() {
        // Mimic the broken state from issue #8: assistant with tool_calls
        // followed by no Tool messages.
        let messages = vec![
            Message::user(Content::text("go")),
            Message::assistant_with_reasoning(
                None,
                None,
                None,
                Some(vec![tc("web_fetch:36", "web_fetch")]),
            ),
        ];
        let history = history_from_messages(messages);
        let assistant = history
            .iter()
            .find_map(|i| i.as_assistant())
            .expect("assistant step");
        assert_eq!(assistant.invocations.len(), 1);
        assert!(assistant.invocations[0].result.is_error);
        assert!(assistant.invocations[0]
            .result
            .content
            .contains("interrupted"));
    }

    #[test]
    fn pairs_assistant_with_immediately_following_tool_messages() {
        let messages = vec![
            Message::user(Content::text("go")),
            Message::assistant_with_reasoning(
                None,
                None,
                None,
                Some(vec![tc("a", "f"), tc("b", "g")]),
            ),
            Message::tool("a".into(), "result-a", false),
            Message::tool("b".into(), "result-b", true),
        ];
        let history = history_from_messages(messages);
        let assistant = history
            .iter()
            .find_map(|i| i.as_assistant())
            .expect("assistant step");
        assert_eq!(assistant.invocations.len(), 2);
        assert_eq!(assistant.invocations[0].result.content, "result-a");
        assert!(!assistant.invocations[0].result.is_error);
        assert_eq!(assistant.invocations[1].result.content, "result-b");
        assert!(assistant.invocations[1].result.is_error);
    }
}
