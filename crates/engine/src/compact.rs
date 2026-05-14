//! Conversation history compaction: replaces older history with a model-generated
//! handoff summary to prevent context-window overflow.

use crate::cancel::CancellationToken;
use crate::log;
use crate::provider::{ChatOptions, Provider, ProviderError, TokenUsage};
use protocol::{Content, Message, ReasoningEffort, Role};

/// Handoff instructions handed to the summarizing model.
pub(crate) const SUMMARIZATION_PROMPT: &str = include_str!("prompts/compact.md");

/// Prefix on handoff summary messages. Also used to detect prior summaries on re-compaction.
pub const SUMMARY_PREFIX: &str = include_str!("prompts/compact_summary_prefix.md");

/// Soft token cap on user messages carried forward after compaction.
pub(crate) const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

/// Per-message byte cap when flattening history for the summarizer.
const MAX_STRINGIFIED_MESSAGE_BYTES: usize = 8_000;

/// How many times `run_compact` will drop the oldest history message and
/// retry when the summarization call itself hits the model's context window.
const MAX_CONTEXT_TRIMS: usize = 20;

const MAX_EMPTY_RETRIES: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitialContextInjection {
    /// Drop everything except the summary (used by `/compact`).
    DoNotInject,
    /// Carry recent user messages forward (used by mid-turn auto-compact).
    BeforeLastUserMessage,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CompactReason {
    ContextLimit,
    UserRequested,
}

impl CompactReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ContextLimit => "context_limit",
            Self::UserRequested => "user_requested",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CompactPhase {
    MidTurn,
    Manual,
}

impl CompactPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::MidTurn => "mid_turn",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CompactOptions {
    pub(crate) injection: InitialContextInjection,
    pub(crate) phase: CompactPhase,
    pub(crate) reason: CompactReason,
}

/// Compact `history` (excluding system prompt) and return replacement history plus token usage.
/// Drops oldest entries on context-window errors (up to `MAX_CONTEXT_TRIMS`) and
/// retries empty summaries (up to `MAX_EMPTY_RETRIES`).
pub(crate) async fn run_compact(
    provider: &Provider,
    history: &[Message],
    model: &str,
    instructions: Option<&str>,
    cancel: &CancellationToken,
    options: CompactOptions,
) -> Result<(Vec<Message>, TokenUsage), ProviderError> {
    if history.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "not enough history to compact".into(),
        ));
    }

    let CompactOptions {
        injection,
        phase,
        reason,
    } = options;

    log::entry(
        log::Level::Info,
        "compaction_started",
        &serde_json::json!({
            "phase": phase.as_str(),
            "reason": reason.as_str(),
            "history_len": history.len(),
            "injection": match injection {
                InitialContextInjection::DoNotInject => "do_not_inject",
                InitialContextInjection::BeforeLastUserMessage => "before_last_user_message",
            },
        }),
    );

    let mut window_start = 0usize;
    let mut empty_retries = 0u8;
    let mut context_trims = 0usize;

    let (summary_text, usage) = loop {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        let request_messages =
            build_summarize_request(&history[window_start..], instructions, SUMMARIZATION_PROMPT);

        let opts = ChatOptions::new(cancel);
        match provider
            .chat(&request_messages, &[], model, ReasoningEffort::Off, &opts)
            .await
        {
            Ok(resp) => {
                let text = resp.content.unwrap_or_default().trim().to_string();
                if text.is_empty() {
                    if empty_retries < MAX_EMPTY_RETRIES {
                        empty_retries += 1;
                        log::entry(
                            log::Level::Warn,
                            "compaction_empty_retry",
                            &serde_json::json!({ "attempt": empty_retries }),
                        );
                        continue;
                    }
                    return Err(ProviderError::InvalidResponse(
                        "compaction returned empty summary after retries".into(),
                    ));
                }
                break (text, resp.usage);
            }
            Err(e) if is_context_window_error(&e) => {
                if window_start + 1 < history.len() && context_trims < MAX_CONTEXT_TRIMS {
                    window_start += 1;
                    context_trims += 1;
                    log::entry(
                        log::Level::Warn,
                        "compaction_trim_oldest",
                        &serde_json::json!({
                            "trimmed": context_trims,
                            "remaining_items": history.len() - window_start,
                            "error": e.to_string(),
                        }),
                    );
                    continue;
                }
                log::entry(
                    log::Level::Warn,
                    "compaction_error",
                    &serde_json::json!({
                        "stage": "context_window_exhausted",
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
            Err(e) => {
                log::entry(
                    log::Level::Warn,
                    "compaction_error",
                    &serde_json::json!({
                        "stage": "chat",
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
        }
    };

    let user_messages = collect_user_messages(history);
    let replacement = build_compacted_history(user_messages, &summary_text, injection);

    log::entry(
        log::Level::Info,
        "compaction_complete",
        &serde_json::json!({
            "phase": phase.as_str(),
            "reason": reason.as_str(),
            "context_trims": context_trims,
            "empty_retries": empty_retries,
            "user_messages_kept": replacement.len().saturating_sub(1),
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
        }),
    );

    Ok((replacement, usage))
}

fn build_summarize_request(
    history: &[Message],
    instructions: Option<&str>,
    prompt: &str,
) -> Vec<Message> {
    let conversation = stringify_conversation(history);

    let mut system_text = prompt.trim().to_string();
    if let Some(extra) = instructions {
        let extra = extra.trim();
        if !extra.is_empty() {
            system_text.push_str(
                "\n\nThe user has asked you to pay special attention to the following \
                 when summarizing:\n",
            );
            system_text.push_str(extra);
        }
    }

    vec![
        Message::system(system_text),
        Message::user(Content::text(format!(
            "Conversation to summarize:\n\n{conversation}"
        ))),
    ]
}

fn stringify_conversation(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let (role_label, text): (&str, String) = match m.role {
            Role::System => ("System", message_text(m)),
            Role::User => ("User", message_text(m)),
            Role::Assistant => ("Assistant", assistant_text(m)),
            Role::Tool => ("ToolResult", message_text(m)),
        };

        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let truncated = truncate_bytes_floor(text, MAX_STRINGIFIED_MESSAGE_BYTES);
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(role_label);
        out.push_str(": ");
        out.push_str(&truncated);
    }
    out
}

fn message_text(m: &Message) -> String {
    m.content
        .as_ref()
        .map(|c| c.as_text().to_string())
        .unwrap_or_default()
}

fn assistant_text(m: &Message) -> String {
    let mut text = String::new();
    if let Some(r) = m.reasoning_content.as_deref() {
        let r = r.trim();
        if !r.is_empty() {
            text.push_str("[thinking]\n");
            text.push_str(r);
            text.push_str("\n\n");
        }
    }
    if let Some(c) = m.content.as_ref() {
        text.push_str(c.as_text());
    }
    if let Some(calls) = m.tool_calls.as_ref() {
        for call in calls {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[tool_call] ");
            text.push_str(&call.function.name);
            text.push('(');
            text.push_str(&call.function.arguments);
            text.push(')');
        }
    }
    text
}

fn truncate_bytes_floor(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let end = text.floor_char_boundary(max_bytes);
    let mut out = String::with_capacity(end + 32);
    out.push_str(&text[..end]);
    out.push_str("\n…[truncated for compaction]");
    out
}

/// Collect non-summary user messages as plain text.
fn collect_user_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| {
            let text = match m.role {
                Role::User => m.content.as_ref()?.as_text().to_string(),

                _ => return None,
            };
            let trimmed = text.trim();
            if trimmed.is_empty() || is_summary_text(trimmed) {
                None
            } else {
                Some(text)
            }
        })
        .collect()
}

#[cfg(test)]
fn is_summary_message(message: &Message) -> bool {
    if !matches!(message.role, Role::User) {
        return false;
    }
    message
        .content
        .as_ref()
        .map(|c| is_summary_text(c.as_text().trim()))
        .unwrap_or(false)
}

fn is_summary_text(text: &str) -> bool {
    text.starts_with(SUMMARY_PREFIX.trim_end())
}

fn build_compacted_history(
    user_messages: Vec<String>,
    summary_text: &str,
    injection: InitialContextInjection,
) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();

    if matches!(injection, InitialContextInjection::BeforeLastUserMessage) {
        let selected = select_recent_user_messages(user_messages, COMPACT_USER_MESSAGE_MAX_TOKENS);
        for text in selected {
            out.push(Message::user(Content::text(text)));
        }
    }

    let summary_body = if summary_text.trim().is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.trim().to_string()
    };
    let prefixed = format!("{}\n{}", SUMMARY_PREFIX.trim_end(), summary_body);
    out.push(Message::user(Content::text(prefixed)));

    out
}

fn select_recent_user_messages(user_messages: Vec<String>, max_tokens: usize) -> Vec<String> {
    if max_tokens == 0 || user_messages.is_empty() {
        return Vec::new();
    }
    let mut remaining = max_tokens;
    let mut selected: Vec<String> = Vec::new();
    for message in user_messages.into_iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = approx_token_count(&message);
        if tokens <= remaining {
            selected.push(message);
            remaining = remaining.saturating_sub(tokens);
        } else {
            let max_bytes = remaining.saturating_mul(4);
            selected.push(truncate_bytes_floor(&message, max_bytes));
            break;
        }
    }
    selected.reverse();
    selected
}

/// Rough token estimate (~4 bytes per token).
fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// True when the error indicates the model's context window was exceeded.
fn is_context_window_error(e: &ProviderError) -> bool {
    let body = match e {
        ProviderError::InvalidResponse(b) => b.as_str(),
        ProviderError::Server { body, .. } => body.as_str(),
        _ => return false,
    };
    let lower = body.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("too many tokens")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_recent_respects_budget() {
        let msgs = vec![
            "a".repeat(4000), // ~1000 tokens
            "b".repeat(4000),
            "c".repeat(4000),
        ];
        let kept = select_recent_user_messages(msgs, 2000);
        assert_eq!(kept.len(), 2, "should keep the two most recent");
        assert!(kept[0].starts_with('b'));
        assert!(kept[1].starts_with('c'));
    }

    #[test]
    fn select_recent_truncates_oldest_kept() {
        let original = vec!["a".repeat(12_000), "b".repeat(400)];
        let kept = select_recent_user_messages(original.clone(), 500);
        assert_eq!(kept.len(), 2);
        assert!(
            kept[0].len() < original[0].len(),
            "oldest kept got truncated"
        );
        assert!(kept[0].contains("truncated"));
    }

    #[test]
    fn summary_roundtrip_detected() {
        let replacement = build_compacted_history(
            vec!["earlier user ask".to_string()],
            "work in progress",
            InitialContextInjection::BeforeLastUserMessage,
        );
        let summary_msg = replacement.last().unwrap();
        assert!(is_summary_message(summary_msg));
        let kept_user = &replacement[0];
        assert!(!is_summary_message(kept_user));
    }

    #[test]
    fn do_not_inject_drops_user_messages() {
        let replacement = build_compacted_history(
            vec!["a".into(), "b".into()],
            "summary",
            InitialContextInjection::DoNotInject,
        );
        assert_eq!(replacement.len(), 1);
        assert!(is_summary_message(&replacement[0]));
    }

    #[test]
    fn summary_detection_ignores_non_user_roles() {
        let as_user = Message::user(Content::text(format!(
            "{}\nbody",
            SUMMARY_PREFIX.trim_end()
        )));
        let as_assistant =
            Message::assistant(Some(as_user.content.as_ref().unwrap().clone()), None, None);
        assert!(is_summary_message(&as_user));
        assert!(!is_summary_message(&as_assistant));
    }

    #[test]
    fn collect_skips_summary_messages() {
        let history = vec![
            Message::user(Content::text("hello")),
            Message::user(Content::text(format!(
                "{}\nprior summary",
                SUMMARY_PREFIX.trim_end()
            ))),
            Message::user(Content::text("hi again")),
        ];
        let collected = collect_user_messages(&history);
        assert_eq!(collected, vec!["hello".to_string(), "hi again".to_string()]);
    }

    #[test]
    fn context_error_detection() {
        assert!(is_context_window_error(&ProviderError::InvalidResponse(
            "context_length_exceeded: prompt too long".into()
        )));
        assert!(is_context_window_error(&ProviderError::InvalidResponse(
            "The prompt is too long for the model".into()
        )));
        assert!(!is_context_window_error(&ProviderError::InvalidResponse(
            "invalid json schema".into()
        )));
        assert!(!is_context_window_error(&ProviderError::Cancelled));
    }

    // ---- reason / phase string forms ----

    #[test]
    fn compact_reason_as_str_matches_each_variant() {
        assert_eq!(CompactReason::ContextLimit.as_str(), "context_limit");
        assert_eq!(CompactReason::UserRequested.as_str(), "user_requested");
    }

    #[test]
    fn compact_phase_as_str_matches_each_variant() {
        assert_eq!(CompactPhase::MidTurn.as_str(), "mid_turn");
        assert_eq!(CompactPhase::Manual.as_str(), "manual");
    }

    // ---- approx_token_count ----

    #[test]
    fn approx_token_count_rounds_up_via_div_ceil() {
        assert_eq!(approx_token_count(""), 0);
        assert_eq!(approx_token_count("abc"), 1);
        assert_eq!(approx_token_count("abcd"), 1);
        assert_eq!(approx_token_count("abcde"), 2);
    }

    // ---- truncate_bytes_floor ----

    #[test]
    fn truncate_bytes_floor_passthrough_when_under_limit() {
        let s = "short text";
        assert_eq!(truncate_bytes_floor(s, 100), "short text");
    }

    #[test]
    fn truncate_bytes_floor_appends_marker_when_truncated() {
        let s = "a".repeat(20);
        let t = truncate_bytes_floor(&s, 5);
        assert!(t.starts_with("aaaaa"));
        assert!(t.contains("[truncated for compaction]"));
    }

    #[test]
    fn truncate_bytes_floor_respects_char_boundary_for_multibyte() {
        // A multi-byte character at the boundary: ensure we don't split it.
        let s = format!("{}{}", "a".repeat(3), "é".repeat(5));
        // 3 bytes 'a', then é (2 bytes each). Limit of 4 would split é.
        let t = truncate_bytes_floor(&s, 4);
        // The non-marker part must end at a char boundary.
        let body = t.split("\n…").next().unwrap();
        assert!(s.is_char_boundary(body.len()));
    }

    // ---- stringify_conversation ----

    fn user(text: &str) -> Message {
        Message::user(Content::text(text.to_string()))
    }

    fn assistant(
        text: Option<&str>,
        reasoning: Option<&str>,
        calls: Option<Vec<protocol::ToolCall>>,
    ) -> Message {
        Message::assistant(
            text.map(|t| Content::text(t.to_string())),
            reasoning.map(|r| r.to_string()),
            calls,
        )
    }

    #[test]
    fn stringify_conversation_labels_each_role() {
        let history = vec![
            Message::system("sys".to_string()),
            user("hi"),
            assistant(Some("hello"), None, None),
        ];
        let out = stringify_conversation(&history);
        assert!(out.contains("System: sys"));
        assert!(out.contains("User: hi"));
        assert!(out.contains("Assistant: hello"));
    }

    #[test]
    fn stringify_conversation_drops_empty_messages() {
        let history = vec![user(""), user("kept")];
        let out = stringify_conversation(&history);
        assert!(out.contains("User: kept"));
        assert_eq!(out.matches("User: ").count(), 1);
    }

    #[test]
    fn stringify_conversation_includes_thinking_block_and_tool_calls() {
        let calls = vec![protocol::ToolCall::new(
            "id".into(),
            protocol::FunctionCall {
                name: "do_thing".into(),
                arguments: r#"{"a":1}"#.into(),
            },
        )];
        let msg = assistant(Some("body"), Some("reasoning"), Some(calls));
        let out = stringify_conversation(&[msg]);
        assert!(out.contains("[thinking]"));
        assert!(out.contains("reasoning"));
        assert!(out.contains("body"));
        assert!(out.contains("[tool_call] do_thing("));
        assert!(out.contains(r#"{"a":1}"#));
    }

    // ---- select_recent_user_messages ----

    #[test]
    fn select_recent_zero_budget_returns_empty() {
        let kept = select_recent_user_messages(vec!["x".into()], 0);
        assert!(kept.is_empty());
    }

    #[test]
    fn select_recent_empty_input_returns_empty() {
        let kept = select_recent_user_messages(Vec::new(), 1000);
        assert!(kept.is_empty());
    }

    #[test]
    fn select_recent_preserves_chronological_order_in_output() {
        let msgs = vec!["older".into(), "middle".into(), "newest".into()];
        let kept = select_recent_user_messages(msgs, 10_000);
        assert_eq!(kept, vec!["older", "middle", "newest"]);
    }

    // ---- collect_user_messages ----

    #[test]
    fn collect_user_messages_skips_assistant_and_tool_roles() {
        let history = vec![
            user("from-user"),
            assistant(Some("a"), None, None),
            Message {
                role: Role::Tool,
                content: Some(Content::text("t".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some("id".into()),
                is_error: false,
            },
        ];
        let out = collect_user_messages(&history);
        assert_eq!(out, vec!["from-user".to_string()]);
    }

    #[test]
    fn collect_user_messages_skips_empty_user_messages() {
        let history = vec![user(""), user("  "), user("kept")];
        let out = collect_user_messages(&history);
        assert_eq!(out, vec!["kept"]);
    }

    // ---- build_summarize_request ----

    #[test]
    fn build_summarize_request_appends_extra_instructions_when_present() {
        let req = build_summarize_request(&[user("hi")], Some("note this"), "BASE_PROMPT");
        let system_text = req[0].content.as_ref().unwrap().as_text().to_string();
        assert!(system_text.contains("BASE_PROMPT"));
        assert!(system_text.contains("note this"));
        assert!(system_text.contains("special attention"));
    }

    #[test]
    fn build_summarize_request_omits_extras_when_instructions_blank() {
        let req = build_summarize_request(&[user("hi")], Some("   "), "BASE");
        let system_text = req[0].content.as_ref().unwrap().as_text().to_string();
        assert!(!system_text.contains("special attention"));
    }

    #[test]
    fn build_summarize_request_user_message_contains_stringified_conversation() {
        let req = build_summarize_request(&[user("the-content")], None, "BASE");
        let user_text = req[1].content.as_ref().unwrap().as_text().to_string();
        assert!(user_text.starts_with("Conversation to summarize:"));
        assert!(user_text.contains("User: the-content"));
    }

    // ---- build_compacted_history ----

    #[test]
    fn build_compacted_history_replaces_empty_summary_with_placeholder() {
        let out = build_compacted_history(Vec::new(), "   ", InitialContextInjection::DoNotInject);
        let summary_body = out
            .last()
            .unwrap()
            .content
            .as_ref()
            .unwrap()
            .as_text()
            .to_string();
        assert!(summary_body.contains("(no summary available)"));
    }

    #[test]
    fn build_compacted_history_prepends_user_messages_when_injecting() {
        let out = build_compacted_history(
            vec!["q1".into(), "q2".into()],
            "summary",
            InitialContextInjection::BeforeLastUserMessage,
        );
        assert!(out.len() >= 2);
        // Last is summary; earlier entries are the kept user messages in order.
        let texts: Vec<_> = out[..out.len() - 1]
            .iter()
            .map(|m| m.content.as_ref().unwrap().as_text().to_string())
            .collect();
        assert_eq!(texts, vec!["q1", "q2"]);
    }

    // ---- is_context_window_error: cover each branch ----

    #[test]
    fn is_context_window_error_recognizes_each_synonym() {
        let synonyms = [
            "context_length_exceeded",
            "exceeded context length",
            "context window saturated",
            "maximum context exceeded",
            "prompt is too long",
            "prompt too long",
            "too many tokens",
        ];
        for s in synonyms {
            assert!(
                is_context_window_error(&ProviderError::InvalidResponse(s.into())),
                "{s}"
            );
        }
    }

    #[test]
    fn is_context_window_error_checks_server_body_too() {
        let e = ProviderError::Server {
            status: 500,
            body: "context_length_exceeded".into(),
        };
        assert!(is_context_window_error(&e));
    }

    #[test]
    fn is_context_window_error_ignores_unrelated_error_kinds() {
        assert!(!is_context_window_error(&ProviderError::Network(
            "x".into()
        )));
        assert!(!is_context_window_error(&ProviderError::Auth(
            "nope".into()
        )));
    }
}
