//! Append-only conversation history.
//!
//! `HistoryItem` is the canonical in-memory + on-disk representation of a
//! conversation. It supersedes the older [`crate::message::Message`] struct,
//! which now lives on as the *wire* format for OpenAI/Anthropic requests.
//!
//! The shape encodes one invariant the rest of the codebase used to enforce
//! by discipline: **an assistant turn that invoked tools carries every tool
//! result inline**. There is no way to construct an `AssistantTurn` with a
//! `ToolInvocation` whose `result` is missing, so the engine cannot leave the
//! history in a half-applied state mid-tool — the bug pattern that produced
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryItem {
    System {
        content: Content,
    },
    User {
        content: Content,
    },
    Assistant(AssistantTurn),
}

/// A committed assistant message.
///
/// - `invocations` empty ⇒ terminal turn (the assistant produced text /
///   reasoning and the conversation continues with the user).
/// - `invocations` non-empty ⇒ tool turn. Every tool the model asked for is
///   in this vec, and each one already has its `result` recorded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantTurn {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub reasoning_blocks: Vec<ReasoningBlock>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub invocations: Vec<ToolInvocation>,
}

impl AssistantTurn {
    /// Terminal turn — no tool calls. The conversation continues with the
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

    /// Tool turn — every `ToolCall` in `calls` is paired with the matching
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

/// One tool call from an assistant turn together with its execution result.
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
        HistoryItem::User { content }
    }

    pub fn assistant(turn: AssistantTurn) -> Self {
        HistoryItem::Assistant(turn)
    }

    pub fn as_assistant(&self) -> Option<&AssistantTurn> {
        match self {
            HistoryItem::Assistant(turn) => Some(turn),
            _ => None,
        }
    }
}

// ---- Legacy `Vec<Message>` ↔ `Vec<HistoryItem>` conversion ------------
//
// The wire format (Anthropic / OpenAI request bodies) and on-disk session
// files still serialize as `Vec<Message>`. Conversion happens at two
// boundaries: deserializing an old session file, and building an LLM
// request body. Both directions are loss-free for *valid* histories. The
// `Vec<Message> -> Vec<HistoryItem>` direction also repairs orphan tool_use
// blocks by synthesizing a "interrupted" result, which is what makes
// resuming a session that was killed mid-tool safe (see issue #8).

use crate::message::{Message, Role};

/// Fold a legacy `Vec<Message>` into `Vec<HistoryItem>`.
///
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
                    out.push(HistoryItem::User { content: c });
                }
                i += 1;
            }
            Role::Assistant => {
                let calls: Vec<ToolCall> = m.tool_calls.clone().unwrap_or_default();
                // Collect Role::Tool messages directly following this
                // assistant. Pair by call_id.
                let mut results_by_id: std::collections::HashMap<String, (String, bool)> =
                    std::collections::HashMap::new();
                let mut j = i + 1;
                while j < messages.len() && matches!(messages[j].role, Role::Tool) {
                    if let (Some(id), Some(content)) =
                        (messages[j].tool_call_id.clone(), messages[j].content.clone())
                    {
                        results_by_id
                            .insert(id, (content.as_text().to_string(), messages[j].is_error));
                    }
                    j += 1;
                }
                let invocations = calls
                    .into_iter()
                    .map(|tc| {
                        let (content, is_error) = results_by_id.remove(&tc.id).unwrap_or_else(|| {
                            (
                                "interrupted (resumed): no recorded tool result".into(),
                                true,
                            )
                        });
                        ToolInvocation {
                            call_id: tc.id,
                            name: tc.function.name,
                            arguments: tc.function.arguments,
                            result: ToolOutcome {
                                content,
                                is_error,
                                metadata: None,
                            },
                            elapsed_ms: None,
                        }
                    })
                    .collect::<Vec<_>>();
                out.push(HistoryItem::Assistant(AssistantTurn {
                    content: m.content.clone(),
                    reasoning: m.reasoning_content.clone(),
                    reasoning_blocks: m.reasoning_details.clone().unwrap_or_default(),
                    invocations,
                }));
                i = j;
            }
            Role::Tool => {
                // Stray tool message with no preceding assistant tool_call —
                // drop it. (This can happen in synthetic test fixtures; real
                // sessions never see it.)
                i += 1;
            }
        }
    }
    out
}

/// Render a slice of `HistoryItem`s back into the legacy `Vec<Message>`
/// shape used by the provider wire layer. The result satisfies the
/// assistant-tool_calls ↔ tool_call_id pairing invariant by construction.
pub fn history_to_messages(items: &[HistoryItem]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(items.len() * 2);
    for item in items {
        match item {
            HistoryItem::System { content } => {
                out.push(Message::system_content(content.clone()));
            }
            HistoryItem::User { content } => {
                out.push(Message::user(content.clone()));
            }
            HistoryItem::Assistant(turn) => {
                let tool_calls = if turn.invocations.is_empty() {
                    None
                } else {
                    Some(turn.invocations.iter().map(|inv| inv.as_tool_call()).collect())
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
                    out.push(Message::tool(
                        inv.call_id.clone(),
                        inv.result.content.clone(),
                        inv.result.is_error,
                    ));
                }
            }
        }
    }
    out
}

/// For each input `Message` index, return the index into the produced
/// `Vec<HistoryItem>`. Useful for remapping snapshot keys when loading an
/// older session whose `token_snapshots` etc. were keyed by message
/// position.
///
/// The returned vector has the same length as `messages`. Indexing is
/// "message position N maps to the HistoryItem that absorbed N".
pub fn message_to_history_positions(messages: &[Message]) -> Vec<usize> {
    let mut out = Vec::with_capacity(messages.len());
    let mut hist_idx = 0usize;
    let mut i = 0usize;
    while i < messages.len() {
        let m = &messages[i];
        match m.role {
            Role::System | Role::User => {
                out.push(hist_idx);
                hist_idx += 1;
                i += 1;
            }
            Role::Assistant => {
                out.push(hist_idx);
                let mut j = i + 1;
                while j < messages.len() && matches!(messages[j].role, Role::Tool) {
                    out.push(hist_idx);
                    j += 1;
                }
                hist_idx += 1;
                i = j;
            }
            Role::Tool => {
                // Stray Tool with no preceding assistant — dropped by
                // history_from_messages. Map to the current hist_idx so
                // the caller can still use the table without panicking.
                out.push(hist_idx);
                i += 1;
            }
        }
    }
    out
}

/// Walk a `&[HistoryItem]` once, yielding `(history_index, message_index)`
/// pairs for every `Message` that `history_to_messages` would emit. Useful
/// for remapping snapshot keys when serializing a `Session` back to the
/// legacy wire shape.
pub fn history_to_message_positions(items: &[HistoryItem]) -> Vec<usize> {
    let mut out = Vec::with_capacity(items.len());
    let mut msg_idx = 0usize;
    for item in items {
        out.push(msg_idx);
        match item {
            HistoryItem::System { .. } | HistoryItem::User { .. } => {
                msg_idx += 1;
            }
            HistoryItem::Assistant(turn) => {
                msg_idx += 1 + turn.invocations.len();
            }
        }
    }
    out
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

    #[test]
    fn assistant_turn_with_no_invocations_round_trips() {
        let turn = AssistantTurn::terminal(Some(Content::text("hi")), None, vec![]);
        let item = HistoryItem::Assistant(turn);
        let msgs = history_to_messages(std::slice::from_ref(&item));
        let back = history_from_messages(msgs);
        assert_eq!(back, vec![item]);
    }

    #[test]
    fn assistant_with_invocations_round_trips_through_legacy_messages() {
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
        let item = HistoryItem::Assistant(AssistantTurn::with_invocations(
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
    fn orphan_tool_use_in_legacy_session_is_repaired_with_interrupted_result() {
        // Mimic the broken state from issue #8: assistant with tool_calls
        // followed by no Tool messages.
        let legacy = vec![
            Message::user(Content::text("go")),
            Message::assistant_with_reasoning(
                None,
                None,
                None,
                Some(vec![tc("web_fetch:36", "web_fetch")]),
            ),
        ];
        let history = history_from_messages(legacy);
        let assistant = history
            .iter()
            .find_map(|i| i.as_assistant())
            .expect("assistant turn");
        assert_eq!(assistant.invocations.len(), 1);
        assert!(assistant.invocations[0].result.is_error);
        assert!(assistant.invocations[0]
            .result
            .content
            .contains("interrupted"));
    }

    #[test]
    fn pairs_assistant_with_immediately_following_tool_messages() {
        let legacy = vec![
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
        let history = history_from_messages(legacy);
        let assistant = history
            .iter()
            .find_map(|i| i.as_assistant())
            .expect("assistant turn");
        assert_eq!(assistant.invocations.len(), 2);
        assert_eq!(assistant.invocations[0].result.content, "result-a");
        assert!(!assistant.invocations[0].result.is_error);
        assert_eq!(assistant.invocations[1].result.content, "result-b");
        assert!(assistant.invocations[1].result.is_error);
    }
}
