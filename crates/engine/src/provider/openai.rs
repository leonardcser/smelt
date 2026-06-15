use super::sse;
use super::{non_empty, non_empty_blocks};
use super::{ParsedResponse, ProviderError, StreamDelta, ToolDefinition};
use crate::cancel::CancellationToken;
use crate::config::ModelConfig;
use crate::log;
use crate::trim::{trim_tool_output, MAX_TOOL_OUTPUT_LINES};
use protocol::{
    FunctionCall, Message, ReasoningBlock, ReasoningEffort, Role, TokenUsage, ToolCall,
};
use std::collections::HashMap;

fn add_tokens(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// OpenAI reports total input tokens (including cached). Keep non-cached
/// input for pricing, but preserve total/context tokens for context-window
/// accounting.
fn parse_usage(u: &serde_json::Value) -> TokenUsage {
    let total_input = u["input_tokens"].as_u64().map(|n| n as u32);
    let cached = u["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .map(|n| n as u32);
    let output = u["output_tokens"].as_u64().map(|n| n as u32);
    let total = u["total_tokens"].as_u64().map(|n| n as u32);
    TokenUsage {
        context_tokens: total.or_else(|| add_tokens(total_input, output)),
        prompt_tokens: match (total_input, cached) {
            (Some(t), Some(c)) => Some(t.saturating_sub(c)),
            (t, _) => t,
        },
        completion_tokens: output,
        cache_read_tokens: cached,
        cache_write_tokens: None,
        reasoning_tokens: u["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .map(|n| n as u32),
    }
}

fn effort_label(effort: ReasoningEffort) -> String {
    if effort == ReasoningEffort::Max {
        "xhigh".to_string()
    } else {
        effort.label().to_string()
    }
}

// COMPAT(openai-reasoning-summary-shape): old sessions may contain OpenAI
// Responses reasoning summaries as an object or string; current wire uses an array.
fn normalize_openai_reasoning_item(data: &serde_json::Value) -> serde_json::Value {
    let mut out = data.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    obj.remove("id");
    let Some(summary) = obj.get_mut("summary") else {
        return out;
    };
    if summary.is_object() {
        *summary = serde_json::json!([summary.clone()]);
    } else if let Some(text) = summary.as_str() {
        *summary = serde_json::json!([{ "type": "summary_text", "text": text }]);
    }
    out
}

pub(super) fn build_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    let mut instructions = String::new();
    let mut input = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {
                let text = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                if !instructions.is_empty() {
                    instructions.push('\n');
                }
                instructions.push_str(text);
            }
            Role::User => {
                let content_val = match &m.content {
                    Some(protocol::Content::Text(t)) => serde_json::json!(t),
                    Some(protocol::Content::Parts(parts)) => {
                        let items: Vec<serde_json::Value> = parts
                            .iter()
                            .map(|p| match p {
                                protocol::ContentPart::Text { text } => {
                                    serde_json::json!({"type": "input_text", "text": text})
                                }
                                protocol::ContentPart::ImageUrl { url, .. } => {
                                    serde_json::json!({"type": "input_image", "image_url": url})
                                }
                            })
                            .collect();
                        serde_json::json!(items)
                    }
                    None => serde_json::json!(""),
                };
                input.push(serde_json::json!({
                    "role": "user",
                    "content": content_val,
                }));
            }
            Role::Assistant => {
                // Reasoning items must appear *before* the message and
                // function_call items they preceded in the original
                // response, so the server can link them up by id.
                if let Some(blocks) = &m.reasoning_details {
                    for block in blocks {
                        if block.provider == ReasoningBlock::OPENAI_RESPONSES {
                            input.push(normalize_openai_reasoning_item(&block.data));
                        }
                    }
                }
                if let Some(content) = &m.content {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content.as_text()}],
                    }));
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        let args =
                            if serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                                .is_ok()
                            {
                                &tc.function.arguments
                            } else {
                                "{}"
                            };
                        input.push(serde_json::json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.function.name,
                            "arguments": args,
                        }));
                    }
                }
            }
            Role::Tool => {
                let output = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                let trimmed = trim_tool_output(output, MAX_TOOL_OUTPUT_LINES);
                let call_id = match &m.tool_call_id {
                    Some(id) if !id.is_empty() => id.as_str(),
                    _ => {
                        log::entry(
                            log::Level::Error,
                            "tool_message_missing_call_id",
                            &serde_json::json!({
                                "content": output,
                                "tool_call_id": m.tool_call_id.clone(),
                            }),
                        );
                        "missing_call_id"
                    }
                };
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": trimmed,
                }));
            }
        }
    }

    let api_tools: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.function.name,
                "description": t.function.description,
                "parameters": t.function.parameters,
            })
        })
        .collect();

    let mut body =
        serde_json::json!({ "model": model, "instructions": instructions, "input": input });

    if !api_tools.is_empty() {
        body["tools"] = serde_json::json!(api_tools);
    }
    if let Some(v) = config.temperature {
        body["temperature"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_p {
        body["top_p"] = serde_json::json!(v);
    }
    if effort != ReasoningEffort::Off {
        body["reasoning"] = serde_json::json!({
            "effort": effort_label(effort),
            "summary": "auto",
        });
        // Ask the server to return encrypted reasoning so we can replay it
        // on subsequent turns without keeping a `previous_response_id` -
        // stateless rounds with full history match smelt's design.
        body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
    }
    if let Some(v) = config.max_tokens {
        body["max_output_tokens"] = serde_json::json!(v);
    }

    body
}

pub(super) fn parse_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
    let output = data["output"]
        .as_array()
        .ok_or_else(|| ProviderError::InvalidResponse("no output in response".into()))?;

    let mut content: Option<String> = None;
    let mut reasoning: Option<String> = None;
    let mut reasoning_blocks: Vec<ReasoningBlock> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                if let Some(parts) = item["content"].as_array() {
                    for part in parts {
                        if part["type"].as_str() == Some("output_text") {
                            let text = part["text"].as_str().unwrap_or_default();
                            match &mut content {
                                Some(c) => c.push_str(text),
                                None => content = Some(text.to_string()),
                            }
                        }
                    }
                }
            }
            Some("function_call") => {
                let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                let name = item["name"].as_str().unwrap_or_default().to_string();
                let arguments = item["arguments"].as_str().unwrap_or("{}").to_string();
                tool_calls.push(ToolCall::new(call_id, FunctionCall { name, arguments }));
            }
            Some("reasoning") => {
                let data = normalize_openai_reasoning_item(item);
                let mut texts: Vec<&str> = Vec::new();
                if let Some(summaries) = data["summary"].as_array() {
                    texts.extend(summaries.iter().filter_map(|s| s["text"].as_str()));
                }
                if texts.is_empty() {
                    if let Some(parts) = data["content"].as_array() {
                        texts.extend(parts.iter().filter_map(|p| p["text"].as_str()));
                    }
                }
                if !texts.is_empty() {
                    reasoning = Some(texts.join("\n"));
                }
                reasoning_blocks.push(ReasoningBlock {
                    provider: ReasoningBlock::OPENAI_RESPONSES.to_string(),
                    data,
                });
            }
            _ => {}
        }
    }

    let usage = parse_usage(&data["usage"]);

    Ok(ParsedResponse {
        content,
        reasoning,
        reasoning_blocks: non_empty_blocks(reasoning_blocks),
        tool_calls,
        usage,
    })
}

/// Accumulator for one streaming response. Mutated by `apply_sse_event`.
#[derive(Default)]
pub(super) struct StreamState {
    pub(super) content: String,
    pub(super) reasoning: String,
    /// Verbatim reasoning items captured via `response.output_item.done`.
    /// Echoed back on the next turn after normalization; item ids are
    /// omitted because stateless requests do not persist server items.
    pub(super) reasoning_items: Vec<serde_json::Value>,
    /// item_id -> (call_id, name, args)
    pub(super) tool_calls: HashMap<String, (String, String, String)>,
    pub(super) usage: TokenUsage,
    pub(super) error: Option<ProviderError>,
    pub(super) saw_completed: bool,
}

impl StreamState {
    pub(super) fn finalize(self) -> Result<ParsedResponse, ProviderError> {
        if let Some(err) = self.error {
            return Err(err);
        }
        if !self.saw_completed {
            return Err(ProviderError::Stream(
                "stream ended without response.completed".into(),
            ));
        }
        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .into_values()
            .filter(|(call_id, name, _)| !call_id.is_empty() && !name.is_empty())
            .map(|(call_id, name, args)| {
                ToolCall::new(
                    call_id,
                    FunctionCall {
                        name,
                        arguments: args,
                    },
                )
            })
            .collect();
        let reasoning_blocks: Vec<ReasoningBlock> = self
            .reasoning_items
            .into_iter()
            .map(|data| ReasoningBlock {
                provider: ReasoningBlock::OPENAI_RESPONSES.to_string(),
                data: normalize_openai_reasoning_item(&data),
            })
            .collect();
        Ok(ParsedResponse {
            content: non_empty(self.content),
            reasoning: non_empty(self.reasoning),
            reasoning_blocks: non_empty_blocks(reasoning_blocks),
            tool_calls,
            usage: self.usage,
        })
    }
}

#[cfg(any(test, feature = "fuzz"))]
pub(super) fn parse_stream_events<'a>(
    events: impl IntoIterator<Item = &'a serde_json::Value>,
    on_delta: &mut dyn FnMut(StreamDelta),
) -> Result<ParsedResponse, ProviderError> {
    let mut state = StreamState::default();
    for ev in events {
        apply_sse_event(&mut state, ev, on_delta, super::unix_now());
    }
    state.finalize()
}

/// Apply one SSE event to the accumulator. Pure (modulo the on_delta callback).
pub(super) fn apply_sse_event(
    state: &mut StreamState,
    ev: &serde_json::Value,
    on_delta: &mut dyn FnMut(StreamDelta),
    now_secs: u64,
) {
    let ev_type = ev["type"].as_str().unwrap_or("");

    match ev_type {
        "response.output_item.added" if ev["item"]["type"].as_str() == Some("function_call") => {
            let item = &ev["item"];
            let id = item["id"].as_str().unwrap_or("").to_string();
            let call_id = item["call_id"].as_str().unwrap_or("").to_string();
            let name = item["name"].as_str().unwrap_or("").to_string();
            if !id.is_empty() && !name.is_empty() {
                state.tool_calls.insert(id, (call_id, name, String::new()));
            }
        }
        "response.output_item.done" if ev["item"]["type"].as_str() == Some("reasoning") => {
            // Capture the full reasoning item - `id` + `encrypted_content`
            // + `summary` - so it can be echoed back on the next request.
            state.reasoning_items.push(ev["item"].clone());
        }
        "response.output_text.delta" => {
            if let Some(text) = ev["delta"].as_str() {
                if !text.is_empty() {
                    state.content.push_str(text);
                    on_delta(StreamDelta::Text(text));
                }
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(item_id) = ev["item_id"].as_str() {
                if let Some(entry) = state.tool_calls.get_mut(item_id) {
                    if let Some(args) = ev["delta"].as_str() {
                        if !args.is_empty() {
                            entry.2.push_str(args);
                            on_delta(StreamDelta::ToolArgs {
                                call_id: &entry.0,
                                tool_name: &entry.1,
                                delta: args,
                            });
                        }
                    }
                }
            }
        }
        "response.function_call_arguments.done" => {
            if let Some(item_id) = ev["item_id"].as_str() {
                if let Some(entry) = state.tool_calls.get_mut(item_id) {
                    entry.2 = ev["arguments"].as_str().unwrap_or("{}").to_string();
                }
            }
        }
        "response.reasoning.delta" | "response.reasoning_summary_text.delta" => {
            if let Some(text) = ev["delta"].as_str() {
                if !text.is_empty() {
                    state.reasoning.push_str(text);
                    on_delta(StreamDelta::Thinking(text));
                }
            }
        }
        "response.completed" | "response.done" => {
            state.saw_completed = true;
            if let Some(u) = ev.get("response").and_then(|r| r.get("usage")) {
                state.usage = parse_usage(u);
            }
        }
        "response.failed" => {
            if let Some(error) = ev.get("response").and_then(|r| r.get("error")) {
                let code = error["code"].as_str().unwrap_or("");
                let err_type = error["type"].as_str().unwrap_or("");
                let message = error["message"].as_str().unwrap_or("");
                let resets_at = super::json_as_u64(&error["resets_at"]);
                if code == "rate_limit_exceeded" {
                    let retry_after = super::parse_retry_from_body(message);
                    state.error = Some(super::rate_limit_error(resets_at, retry_after, now_secs));
                } else if code == "insufficient_quota"
                    || code == "billing_not_active"
                    || err_type == "usage_limit_reached"
                {
                    state.error = Some(ProviderError::QuotaExceeded(message.to_string()));
                } else if code == "context_length_exceeded" {
                    state.error = Some(ProviderError::InvalidResponse(message.to_string()));
                } else {
                    state.error = Some(ProviderError::Server {
                        status: 0,
                        body: message.to_string(),
                    });
                }
            }
        }
        "response.incomplete" => {
            let reason = ev
                .get("response")
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(|r| r.as_str())
                .unwrap_or("unknown");
            state.error = Some(ProviderError::Stream(format!(
                "incomplete response returned, reason: {reason}"
            )));
        }
        _ => {}
    }
}

pub(super) async fn read_stream(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    on_delta: &(dyn Fn(StreamDelta) + Send + Sync),
    now_secs: u64,
) -> Result<ParsedResponse, ProviderError> {
    let mut state = StreamState::default();

    sse::read_events(resp, cancel, |ev| {
        apply_sse_event(&mut state, ev, &mut |d| on_delta(d), now_secs);
    })
    .await?;

    state.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FunctionSchema;
    use crate::test_util::{assistant_text, system, tool_msg, user};
    use protocol::{Content, ContentPart, FunctionCall, Message, Role, ToolCall};
    use serde_json::json;

    fn assistant_calls(calls: Vec<ToolCall>) -> Message {
        crate::test_util::assistant_calls(None, calls)
    }

    fn cfg() -> ModelConfig {
        ModelConfig::default()
    }

    fn completed_state() -> StreamState {
        StreamState {
            saw_completed: true,
            ..Default::default()
        }
    }

    // ---- parse_usage ----

    #[test]
    fn parse_usage_subtracts_cached_from_input_tokens() {
        let v = json!({
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 30},
            "output_tokens": 40,
            "output_tokens_details": {"reasoning_tokens": 5},
            "total_tokens": 140,
        });
        let u = parse_usage(&v);
        assert_eq!(u.context_tokens, Some(140));
        assert_eq!(u.prompt_tokens, Some(70));
        assert_eq!(u.completion_tokens, Some(40));
        assert_eq!(u.cache_read_tokens, Some(30));
        assert_eq!(u.reasoning_tokens, Some(5));
        assert_eq!(u.cache_write_tokens, None);
    }

    #[test]
    fn parse_usage_handles_cached_exceeding_input_via_saturating_sub() {
        let v = json!({
            "input_tokens": 10,
            "input_tokens_details": {"cached_tokens": 50},
        });
        assert_eq!(parse_usage(&v).prompt_tokens, Some(0));
    }

    #[test]
    fn parse_usage_passes_through_input_when_no_cached_field() {
        let v = json!({"input_tokens": 42, "output_tokens": 3});
        assert_eq!(parse_usage(&v).prompt_tokens, Some(42));
        assert_eq!(parse_usage(&v).context_tokens, Some(45));
    }

    #[test]
    fn parse_usage_empty_object_yields_all_none() {
        let v = json!({});
        let u = parse_usage(&v);
        assert_eq!(u.prompt_tokens, None);
        assert_eq!(u.completion_tokens, None);
        assert_eq!(u.cache_read_tokens, None);
        assert_eq!(u.reasoning_tokens, None);
    }

    // ---- effort_label ----

    #[test]
    fn effort_label_maps_max_to_xhigh() {
        assert_eq!(effort_label(ReasoningEffort::Max), "xhigh");
    }

    #[test]
    fn effort_label_falls_through_for_other_levels() {
        assert_eq!(
            effort_label(ReasoningEffort::Low),
            ReasoningEffort::Low.label()
        );
        assert_eq!(
            effort_label(ReasoningEffort::High),
            ReasoningEffort::High.label()
        );
    }

    // ---- build_body ----

    #[test]
    fn build_body_collects_system_messages_into_instructions_joined_by_newline() {
        let msgs = vec![system("a"), system("b"), user("hi")];
        let body = build_body(&msgs, &[], "gpt-x", ReasoningEffort::Off, &cfg());
        assert_eq!(body["instructions"], "a\nb");
        assert_eq!(body["input"][0]["role"], "user");
    }

    #[test]
    fn build_body_user_text_serialized_as_string() {
        let body = build_body(&[user("hello")], &[], "m", ReasoningEffort::Off, &cfg());
        assert_eq!(body["input"][0]["content"], "hello");
    }

    #[test]
    fn build_body_user_parts_serialized_as_input_text_and_input_image() {
        let m = Message {
            role: Role::User,
            content: Some(Content::Parts(vec![
                ContentPart::Text {
                    text: "see this".into(),
                },
                ContentPart::ImageUrl {
                    url: "https://x/y.png".into(),
                    label: None,
                },
            ])),
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        };
        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());
        let parts = &body["input"][0]["content"];
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[0]["text"], "see this");
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["image_url"], "https://x/y.png");
    }

    #[test]
    fn build_body_user_none_content_serialized_as_empty_string() {
        let m = Message {
            role: Role::User,
            content: None,
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        };
        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());
        assert_eq!(body["input"][0]["content"], "");
    }

    #[test]
    fn build_body_assistant_with_content_emits_output_text_message() {
        let body = build_body(
            &[assistant_text("hi back")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        let msg = &body["input"][0];
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"][0]["type"], "output_text");
        assert_eq!(msg["content"][0]["text"], "hi back");
    }

    #[test]
    fn build_body_assistant_without_content_skips_message_entry() {
        let m = Message {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        };
        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());
        assert!(body["input"].as_array().unwrap().is_empty());
    }

    #[test]
    fn build_body_wraps_legacy_reasoning_summary_object() {
        let m = Message {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,
            reasoning_details: Some(vec![ReasoningBlock {
                provider: ReasoningBlock::OPENAI_RESPONSES.to_string(),
                data: json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": {"type": "summary_text", "text": "legacy"},
                    "encrypted_content": "ciphertext",
                }),
            }]),
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        };

        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());

        assert_eq!(body["input"][0]["type"], "reasoning");
        assert!(body["input"][0].get("id").is_none());
        assert!(body["input"][0]["summary"].is_array());
        assert_eq!(body["input"][0]["summary"][0]["text"], "legacy");
    }

    #[test]
    fn build_body_strips_replayed_reasoning_item_ids() {
        let m = Message {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,
            reasoning_details: Some(vec![ReasoningBlock {
                provider: ReasoningBlock::OPENAI_RESPONSES.to_string(),
                data: json!({
                    "type": "reasoning",
                    "id": "rs_0638b310c39c8750016a1de93c61b4819191b4c8d21301d82f",
                    "summary": [{"type": "summary_text", "text": "thought"}],
                }),
            }]),
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        };

        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());

        assert_eq!(body["input"][0]["type"], "reasoning");
        assert!(body["input"][0].get("id").is_none());
        assert_eq!(body["input"][0]["summary"][0]["text"], "thought");
    }

    #[test]
    fn build_body_assistant_emits_function_call_entries_for_tool_calls() {
        let calls = vec![ToolCall::new(
            "call-1".into(),
            FunctionCall {
                name: "search".into(),
                arguments: r#"{"q":"rust"}"#.into(),
            },
        )];
        let body = build_body(
            &[assistant_calls(calls)],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        let fc = &body["input"][0];
        assert_eq!(fc["type"], "function_call");
        assert_eq!(fc["call_id"], "call-1");
        assert_eq!(fc["name"], "search");
        assert_eq!(fc["arguments"], r#"{"q":"rust"}"#);
    }

    #[test]
    fn build_body_assistant_tool_call_arguments_default_to_empty_object_when_invalid_json() {
        let calls = vec![ToolCall::new(
            "id".into(),
            FunctionCall {
                name: "n".into(),
                arguments: "not-json".into(),
            },
        )];
        let body = build_body(
            &[assistant_calls(calls)],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        assert_eq!(body["input"][0]["arguments"], "{}");
    }

    #[test]
    fn build_body_tool_message_emits_function_call_output_with_call_id() {
        let body = build_body(
            &[tool_msg(Some("call-x"), "result")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        let out = &body["input"][0];
        assert_eq!(out["type"], "function_call_output");
        assert_eq!(out["call_id"], "call-x");
        assert_eq!(out["output"], "result");
    }

    #[test]
    fn build_body_tool_message_without_call_id_uses_missing_call_id_marker() {
        let body = build_body(
            &[tool_msg(None, "result")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        assert_eq!(body["input"][0]["call_id"], "missing_call_id");
    }

    #[test]
    fn build_body_tool_message_with_empty_call_id_uses_missing_call_id_marker() {
        let body = build_body(
            &[tool_msg(Some(""), "result")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        assert_eq!(body["input"][0]["call_id"], "missing_call_id");
    }

    #[test]
    fn build_body_serializes_tools_when_provided() {
        let tools = vec![ToolDefinition::new(FunctionSchema {
            name: "do".into(),
            description: "desc".into(),
            parameters: json!({"type":"object"}),
        })];
        let body = build_body(&[user("hi")], &tools, "m", ReasoningEffort::Off, &cfg());
        let t = &body["tools"][0];
        assert_eq!(t["type"], "function");
        assert_eq!(t["name"], "do");
        assert_eq!(t["description"], "desc");
        assert_eq!(t["parameters"]["type"], "object");
    }

    #[test]
    fn build_body_omits_tools_field_when_none() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &cfg());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_body_sets_temperature_and_top_p_when_provided() {
        let mut c = cfg();
        c.temperature = Some(0.7);
        c.top_p = Some(0.9);
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &c);
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["top_p"], 0.9);
    }

    #[test]
    fn build_body_omits_temperature_and_top_p_when_none() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &cfg());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn build_body_emits_reasoning_block_when_effort_is_not_off() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::High, &cfg());
        assert_eq!(body["reasoning"]["effort"], ReasoningEffort::High.label());
        assert_eq!(body["reasoning"]["summary"], "auto");
    }

    #[test]
    fn build_body_omits_reasoning_block_when_effort_is_off() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &cfg());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn build_body_includes_model_name() {
        let body = build_body(&[user("hi")], &[], "gpt-foo", ReasoningEffort::Off, &cfg());
        assert_eq!(body["model"], "gpt-foo");
    }

    // ---- parse_response ----

    #[test]
    fn parse_response_returns_error_when_output_missing() {
        let v = json!({});
        match parse_response(&v) {
            Err(ProviderError::InvalidResponse(_)) => {}
            other => panic!("expected InvalidResponse, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn parse_response_concatenates_output_text_parts_from_message_items() {
        let v = json!({
            "output": [
                {"type": "message", "content": [
                    {"type": "output_text", "text": "hello "},
                    {"type": "output_text", "text": "world"},
                ]}
            ],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.content.as_deref(), Some("hello world"));
        assert!(r.reasoning.is_none());
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn parse_response_extracts_function_calls() {
        let v = json!({
            "output": [
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"a\":1}"}
            ],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "c1");
        assert_eq!(r.tool_calls[0].function.name, "f");
        assert_eq!(r.tool_calls[0].function.arguments, "{\"a\":1}");
    }

    #[test]
    fn parse_response_function_call_missing_arguments_defaults_to_empty_object() {
        let v = json!({
            "output": [
                {"type": "function_call", "call_id": "c1", "name": "f"}
            ],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.tool_calls[0].function.arguments, "{}");
    }

    #[test]
    fn parse_response_reasoning_prefers_summary_over_content() {
        let v = json!({
            "output": [
                {"type": "reasoning",
                 "summary": [{"text": "sum1"}, {"text": "sum2"}],
                 "content": [{"text": "should-not-appear"}]}
            ],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("sum1\nsum2"));
    }

    #[test]
    fn parse_response_normalizes_legacy_reasoning_summary_object() {
        let v = json!({
            "output": [
                {"type": "reasoning",
                 "summary": {"type": "summary_text", "text": "legacy"}}
            ],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("legacy"));
        let details = r.reasoning_blocks.unwrap();
        assert!(details[0].data["summary"].is_array());
        assert_eq!(details[0].data["summary"][0]["text"], "legacy");
    }

    #[test]
    fn parse_response_reasoning_falls_back_to_content_when_summary_empty() {
        let v = json!({
            "output": [
                {"type": "reasoning",
                 "summary": [],
                 "content": [{"text": "fallback"}]}
            ],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("fallback"));
    }

    #[test]
    fn parse_response_reasoning_none_when_both_summary_and_content_empty() {
        let v = json!({
            "output": [{"type": "reasoning"}],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert!(r.reasoning.is_none());
    }

    #[test]
    fn parse_response_ignores_unknown_output_types() {
        let v = json!({
            "output": [{"type": "wat"}],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert!(r.content.is_none());
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn parse_response_propagates_usage() {
        let v = json!({
            "output": [],
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.usage.prompt_tokens, Some(5));
        assert_eq!(r.usage.completion_tokens, Some(3));
    }

    // ---- apply_sse_event ----

    fn step(state: &mut StreamState, ev: serde_json::Value) {
        apply_sse_event(state, &ev, &mut |_| {}, 1_000);
    }

    #[test]
    fn sse_output_text_delta_appends_to_content_and_emits_text_delta() {
        let mut state = StreamState::default();
        let mut deltas: Vec<String> = Vec::new();
        apply_sse_event(
            &mut state,
            &json!({"type": "response.output_text.delta", "delta": "hi"}),
            &mut |d| {
                if let StreamDelta::Text(t) = d {
                    deltas.push(t.into())
                }
            },
            1_000,
        );
        assert_eq!(state.content, "hi");
        assert_eq!(deltas, vec!["hi".to_string()]);
    }

    #[test]
    fn sse_output_text_delta_ignores_empty_string() {
        let mut state = StreamState::default();
        let mut called = false;
        apply_sse_event(
            &mut state,
            &json!({"type": "response.output_text.delta", "delta": ""}),
            &mut |_| called = true,
            1_000,
        );
        assert!(state.content.is_empty());
        assert!(!called);
    }

    #[test]
    fn sse_function_call_added_then_args_delta_and_done() {
        let mut state = completed_state();
        step(
            &mut state,
            json!({
                "type": "response.output_item.added",
                "item": {"type": "function_call", "id": "i1", "call_id": "c1", "name": "f"}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "i1", "delta": "{\"a\":"
            }),
        );
        step(
            &mut state,
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "i1", "delta": "1}"
            }),
        );
        let r = state.finalize().unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "c1");
        assert_eq!(r.tool_calls[0].function.name, "f");
        assert_eq!(r.tool_calls[0].function.arguments, "{\"a\":1}");
    }

    #[test]
    fn sse_function_call_args_done_replaces_accumulated_args() {
        let mut state = completed_state();
        step(
            &mut state,
            json!({
                "type": "response.output_item.added",
                "item": {"type": "function_call", "id": "i1", "call_id": "c1", "name": "f"}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "i1", "delta": "garbage"
            }),
        );
        step(
            &mut state,
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1", "arguments": "{\"final\":true}"
            }),
        );
        let r = state.finalize().unwrap();
        assert_eq!(r.tool_calls[0].function.arguments, "{\"final\":true}");
    }

    #[test]
    fn sse_function_call_added_skipped_when_id_or_name_empty() {
        let mut state = completed_state();
        step(
            &mut state,
            json!({
                "type": "response.output_item.added",
                "item": {"type": "function_call", "id": "", "call_id": "c1", "name": "f"}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "response.output_item.added",
                "item": {"type": "function_call", "id": "i1", "call_id": "c1", "name": ""}
            }),
        );
        let r = state.finalize().unwrap();
        assert!(r.tool_calls.is_empty());
    }

    #[test]
    fn sse_function_call_added_ignored_when_item_type_is_not_function_call() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.output_item.added",
                "item": {"type": "message", "id": "i1"}
            }),
        );
        assert!(state.tool_calls.is_empty());
    }

    #[test]
    fn sse_args_delta_ignored_when_item_id_unknown() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "unknown", "delta": "x"
            }),
        );
        assert!(state.tool_calls.is_empty());
    }

    #[test]
    fn sse_args_done_defaults_to_empty_object_when_arguments_missing() {
        let mut state = completed_state();
        step(
            &mut state,
            json!({
                "type": "response.output_item.added",
                "item": {"type": "function_call", "id": "i1", "call_id": "c1", "name": "f"}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": "i1"
            }),
        );
        let r = state.finalize().unwrap();
        assert_eq!(r.tool_calls[0].function.arguments, "{}");
    }

    #[test]
    fn sse_reasoning_delta_appends_and_emits_thinking() {
        let mut state = StreamState::default();
        let mut thinking: Vec<String> = Vec::new();
        apply_sse_event(
            &mut state,
            &json!({"type": "response.reasoning.delta", "delta": "ponder"}),
            &mut |d| {
                if let StreamDelta::Thinking(t) = d {
                    thinking.push(t.into())
                }
            },
            1_000,
        );
        apply_sse_event(
            &mut state,
            &json!({"type": "response.reasoning_summary_text.delta", "delta": "ing"}),
            &mut |d| {
                if let StreamDelta::Thinking(t) = d {
                    thinking.push(t.into())
                }
            },
            1_000,
        );
        assert_eq!(state.reasoning, "pondering");
        assert_eq!(thinking, vec!["ponder", "ing"]);
    }

    #[test]
    fn sse_completed_event_extracts_usage() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 8, "output_tokens": 2}}
            }),
        );
        assert_eq!(state.usage.prompt_tokens, Some(8));
        assert_eq!(state.usage.completion_tokens, Some(2));
    }

    #[test]
    fn sse_done_event_also_extracts_usage() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.done",
                "response": {"usage": {"input_tokens": 1}}
            }),
        );
        assert_eq!(state.usage.prompt_tokens, Some(1));
    }

    #[test]
    fn sse_failed_rate_limit_sets_rate_limited_error() {
        let mut state = StreamState::default();
        let resets_at = crate::provider::unix_now() + 30;
        step(
            &mut state,
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "rate_limit_exceeded", "resets_at": resets_at}}
            }),
        );
        match state.error.unwrap() {
            ProviderError::RateLimited { resets_at: actual } => assert_eq!(actual, Some(resets_at)),
            e => panic!("expected RateLimited, got {e:?}"),
        }
    }

    #[test]
    fn sse_failed_rate_limit_without_retry_time_stays_rate_limited() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "rate_limit_exceeded", "message": "request rate exceeded"}}
            }),
        );
        assert!(matches!(
            state.error.unwrap(),
            ProviderError::RateLimited { resets_at: None }
        ));
    }

    #[test]
    fn sse_failed_rate_limit_with_long_retry_window_stays_rate_limited() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "rate_limit_exceeded", "message": "try again in 3600s"}}
            }),
        );
        assert!(matches!(
            state.error.unwrap(),
            ProviderError::RateLimited { resets_at: Some(_) }
        ));
    }

    #[test]
    fn sse_failed_usage_limit_reached_sets_quota_exceeded() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.failed",
                "response": {"error": {"type": "usage_limit_reached"}}
            }),
        );
        assert!(matches!(
            state.error.unwrap(),
            ProviderError::QuotaExceeded(_)
        ));
    }

    #[test]
    fn sse_failed_quota_codes_set_quota_exceeded() {
        for code in ["insufficient_quota", "billing_not_active"] {
            let mut state = StreamState::default();
            step(
                &mut state,
                json!({
                    "type": "response.failed",
                    "response": {"error": {"code": code, "message": "nope"}}
                }),
            );
            assert!(matches!(
                state.error.unwrap(),
                ProviderError::QuotaExceeded(_)
            ));
        }
    }

    #[test]
    fn sse_failed_context_length_sets_invalid_response() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "context_length_exceeded", "message": "too long"}}
            }),
        );
        assert!(matches!(
            state.error.unwrap(),
            ProviderError::InvalidResponse(_)
        ));
    }

    #[test]
    fn sse_failed_unknown_code_falls_through_to_server_error() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "wat", "message": "oops"}}
            }),
        );
        match state.error.unwrap() {
            ProviderError::Server { status, body } => {
                assert_eq!(status, 0);
                assert_eq!(body, "oops");
            }
            e => panic!("expected Server, got {e:?}"),
        }
    }

    #[test]
    fn sse_incomplete_sets_stream_error() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "response.incomplete",
                "response": {"incomplete_details": {"reason": "max_output_tokens"}}
            }),
        );
        match state.error.unwrap() {
            ProviderError::Stream(message) => assert!(message.contains("max_output_tokens")),
            e => panic!("expected Stream, got {e:?}"),
        }
    }

    #[test]
    fn sse_unknown_event_type_ignored() {
        let mut state = StreamState::default();
        step(&mut state, json!({"type": "something.unknown"}));
        // No state change, no panic.
        assert!(state.content.is_empty());
        assert!(state.error.is_none());
    }

    // ---- finalize ----

    #[test]
    fn finalize_returns_stream_error_without_completed() {
        let state = StreamState::default();
        assert!(matches!(state.finalize(), Err(ProviderError::Stream(_))));
    }

    #[test]
    fn finalize_returns_error_when_state_error_set() {
        let state = StreamState {
            error: Some(ProviderError::QuotaExceeded("x".into())),
            ..Default::default()
        };
        assert!(matches!(
            state.finalize(),
            Err(ProviderError::QuotaExceeded(_))
        ));
    }

    #[test]
    fn finalize_filters_tool_calls_missing_call_id_or_name() {
        let mut state = StreamState {
            saw_completed: true,
            ..Default::default()
        };
        state
            .tool_calls
            .insert("i1".into(), ("".into(), "name".into(), "{}".into()));
        state
            .tool_calls
            .insert("i2".into(), ("c2".into(), "".into(), "{}".into()));
        state
            .tool_calls
            .insert("i3".into(), ("c3".into(), "n3".into(), "{}".into()));
        let r = state.finalize().unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "c3");
    }

    #[test]
    fn finalize_empty_content_and_reasoning_become_none() {
        let state = StreamState {
            saw_completed: true,
            ..Default::default()
        };
        let r = state.finalize().unwrap();
        assert!(r.content.is_none());
        assert!(r.reasoning.is_none());
        assert!(r.tool_calls.is_empty());
        assert!(r.reasoning_blocks.is_none());
    }

    // ---- reasoning round-trip ----

    #[test]
    fn build_body_requests_encrypted_reasoning_when_effort_on() {
        let body = build_body(&[user("hi")], &[], "gpt-5", ReasoningEffort::High, &cfg());
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn build_body_omits_include_when_effort_off() {
        let body = build_body(&[user("hi")], &[], "gpt-5", ReasoningEffort::Off, &cfg());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn parse_response_captures_reasoning_items_without_server_id() {
        let v = json!({
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": [{"text": "sum"}], "encrypted_content": "enc"},
                {"type": "message", "content": [{"type": "output_text", "text": "answer"}]},
            ],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        let blocks = r.reasoning_blocks.expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].provider, "openai_responses");
        assert!(blocks[0].data.get("id").is_none());
        assert_eq!(blocks[0].data["encrypted_content"], "enc");
    }

    #[test]
    fn sse_output_item_done_reasoning_captured_without_server_id() {
        let mut state = completed_state();
        step(
            &mut state,
            json!({
                "type": "response.output_item.done",
                "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "enc", "summary": []}
            }),
        );
        let r = state.finalize().unwrap();
        let blocks = r.reasoning_blocks.expect("blocks");
        assert!(blocks[0].data.get("id").is_none());
        assert_eq!(blocks[0].data["encrypted_content"], "enc");
    }

    #[test]
    fn build_body_prepends_reasoning_items_before_message_and_function_call() {
        let calls = vec![ToolCall::new(
            "c1".into(),
            FunctionCall {
                name: "f".into(),
                arguments: "{}".into(),
            },
        )];
        let mut m = assistant_calls(calls);
        m.content = Some(protocol::Content::Text("answer".into()));
        m.reasoning_details = Some(vec![ReasoningBlock {
            provider: ReasoningBlock::OPENAI_RESPONSES.to_string(),
            data: json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "enc"}),
        }]);
        let body = build_body(&[m], &[], "gpt-5", ReasoningEffort::Off, &cfg());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert!(input[0].get("id").is_none());
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[2]["type"], "function_call");
    }

    #[test]
    fn build_body_skips_reasoning_blocks_from_other_providers() {
        let mut m = assistant_text("answer");
        m.reasoning_details = Some(vec![ReasoningBlock {
            provider: ReasoningBlock::ANTHROPIC.to_string(),
            data: json!({"type": "thinking", "thinking": "x"}),
        }]);
        let body = build_body(&[m], &[], "gpt-5", ReasoningEffort::Off, &cfg());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
    }
}
