use super::{collect_indexed_tool_calls, non_empty, sse};
use super::{ParsedResponse, ProviderError, StreamDelta, ToolDefinition};
use crate::cancel::CancellationToken;
use crate::config::ModelConfig;
use crate::trim::{trim_tool_output, MAX_TOOL_OUTPUT_LINES};
use protocol::{FunctionCall, Message, ReasoningEffort, Role, TokenUsage, ToolCall};
use std::collections::HashMap;

fn supports_adaptive_thinking(model: &str) -> bool {
    model.contains("opus-4-6") || model.contains("sonnet-4-6")
}

fn parse_cache_write_tokens(u: &serde_json::Value) -> Option<u32> {
    u["cache_creation_input_tokens"]
        .as_u64()
        .map(|n| n as u32)
        .or_else(|| {
            let cc = u.get("cache_creation")?;
            let a = cc["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0);
            let b = cc["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0);
            if a + b > 0 {
                Some((a + b) as u32)
            } else {
                None
            }
        })
}

pub(super) fn build_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    let mut system_content: Option<String> = None;
    let mut content: Vec<serde_json::Value> = Vec::new();

    for m in messages {
        match m.role {
            Role::System => {
                let text = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                match &mut system_content {
                    Some(s) => s.push_str(&format!("\n\n{}", text)),
                    None => system_content = Some(text.to_string()),
                }
            }
            Role::User => {
                content.push(serde_json::json!({
                    "role": "user",
                    "content": m.content.as_ref().map(|c| c.as_text()).unwrap_or_default(),
                }));
            }
            Role::Assistant => {
                let mut message_content = Vec::new();
                if let Some(c) = &m.content {
                    message_content.push(serde_json::json!({
                        "type": "text",
                        "text": c.as_text(),
                    }));
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        message_content.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        }));
                    }
                }
                content.push(serde_json::json!({
                    "role": "assistant",
                    "content": message_content,
                }));
            }
            Role::Tool => {
                let output = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                let trimmed = trim_tool_output(output, MAX_TOOL_OUTPUT_LINES);
                content.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.as_deref().unwrap_or(""),
                        "content": trimmed,
                    }],
                }));
            }
        }
    }

    let api_tools: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.function.name,
                "description": t.function.description,
                "input_schema": t.function.parameters,
            })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": model,
        "messages": content,
        "max_tokens": 4096,
    });

    if let Some(sys) = system_content {
        body["system"] = serde_json::json!([{"type": "text", "text": sys}]);
    }
    if !api_tools.is_empty() {
        body["tools"] = serde_json::json!(api_tools);
    }
    if let Some(v) = config.temperature {
        body["temperature"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_p {
        body["top_p"] = serde_json::json!(v);
    }

    if effort != ReasoningEffort::Off && supports_adaptive_thinking(model) {
        body["thinking"] = serde_json::json!({
            "type": "adaptive",
            "display": "summarized",
        });
        body["output_config"] = serde_json::json!({
            "effort": effort.label(),
        });
    }

    body
}

pub(super) fn parse_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
    let mut content: Option<String> = None;
    let mut reasoning: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(content_blocks) = data["content"].as_array() {
        for block in content_blocks {
            match block["type"].as_str() {
                Some("text") => {
                    let text = block["text"].as_str().unwrap_or_default();
                    match &mut content {
                        Some(c) => c.push_str(text),
                        None => content = Some(text.to_string()),
                    }
                }
                Some("thinking") => {
                    if let Some(text) = block["thinking"].as_str() {
                        match &mut reasoning {
                            Some(r) => r.push_str(text),
                            None => reasoning = Some(text.to_string()),
                        }
                    }
                }
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    let arguments = block["input"].clone().to_string();
                    tool_calls.push(ToolCall::new(id, FunctionCall { name, arguments }));
                }
                _ => {}
            }
        }
    }

    // Summary mode places thinking in a top-level `thinking` array, not content blocks.
    if reasoning.is_none() {
        if let Some(thinking) = data["thinking"].as_array() {
            for block in thinking {
                if let Some(text) = block["text"].as_str() {
                    match &mut reasoning {
                        Some(r) => r.push_str(text),
                        None => reasoning = Some(text.to_string()),
                    }
                }
            }
        }
    }

    let u = &data["usage"];
    let usage = TokenUsage {
        prompt_tokens: u["input_tokens"].as_u64().map(|n| n as u32),
        completion_tokens: u["output_tokens"].as_u64().map(|n| n as u32),
        cache_read_tokens: u["cache_read_input_tokens"].as_u64().map(|n| n as u32),
        cache_write_tokens: parse_cache_write_tokens(u),
        reasoning_tokens: None,
    };

    Ok(ParsedResponse {
        content,
        reasoning,
        tool_calls,
        usage,
    })
}

/// Accumulator for one streaming response. Mutated by `apply_sse_event`.
#[derive(Default)]
pub(super) struct StreamState {
    pub(super) content: String,
    pub(super) reasoning: String,
    /// content block index -> (id, name, args-json)
    pub(super) tool_calls: HashMap<usize, (String, String, String)>,
    pub(super) usage: TokenUsage,
}

impl StreamState {
    pub(super) fn finalize(self) -> ParsedResponse {
        ParsedResponse {
            content: non_empty(self.content),
            reasoning: non_empty(self.reasoning),
            tool_calls: collect_indexed_tool_calls(self.tool_calls),
            usage: self.usage,
        }
    }
}

/// Apply one SSE event to the accumulator. Pure (modulo `on_delta`).
pub(super) fn apply_sse_event(
    state: &mut StreamState,
    ev: &serde_json::Value,
    on_delta: &mut dyn FnMut(StreamDelta),
) {
    let event_type = ev["type"].as_str().unwrap_or("");

    match event_type {
        "message_start" => {
            if let Some(u) = ev.get("message").and_then(|m| m.get("usage")) {
                state.usage.prompt_tokens = u["input_tokens"].as_u64().map(|n| n as u32);
                state.usage.cache_read_tokens =
                    u["cache_read_input_tokens"].as_u64().map(|n| n as u32);
                state.usage.cache_write_tokens = parse_cache_write_tokens(u);
            }
        }
        "content_block_start" => {
            if let Some(idx) = ev["index"].as_u64() {
                if let Some(cb) = ev.get("content_block") {
                    if cb["type"].as_str() == Some("tool_use") {
                        let id = cb["id"].as_str().unwrap_or_default().to_string();
                        let name = cb["name"].as_str().unwrap_or_default().to_string();
                        state
                            .tool_calls
                            .insert(idx as usize, (id, name, String::new()));
                    }
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = ev.get("delta") {
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = delta["text"].as_str() {
                            if !text.is_empty() {
                                state.content.push_str(text);
                                on_delta(StreamDelta::Text(text));
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta["thinking"].as_str() {
                            if !text.is_empty() {
                                state.reasoning.push_str(text);
                                on_delta(StreamDelta::Thinking(text));
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial_json) = delta["partial_json"].as_str() {
                            if !partial_json.is_empty() {
                                if let Some(idx) = ev["index"].as_u64() {
                                    if let Some(entry) = state.tool_calls.get_mut(&(idx as usize)) {
                                        entry.2.push_str(partial_json);
                                        on_delta(StreamDelta::ToolArgs {
                                            call_id: &entry.0,
                                            tool_name: &entry.1,
                                            delta: partial_json,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        "message_delta" => {
            if let Some(u) = ev.get("usage") {
                state.usage.completion_tokens = u["output_tokens"].as_u64().map(|n| n as u32);
                if state.usage.prompt_tokens.is_none() {
                    state.usage.prompt_tokens = u["input_tokens"].as_u64().map(|n| n as u32);
                }
            }
        }
        _ => {}
    }
}

pub(super) async fn read_stream(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    on_delta: &(dyn Fn(StreamDelta) + Send + Sync),
) -> Result<ParsedResponse, ProviderError> {
    let mut state = StreamState::default();

    sse::read_events(resp, cancel, |ev| {
        apply_sse_event(&mut state, ev, &mut |d| on_delta(d));
    })
    .await?;

    Ok(state.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FunctionSchema;
    use crate::test_util::{assistant_calls, assistant_text, system, tool_msg, user};
    use protocol::{FunctionCall, ToolCall};
    use serde_json::json;

    fn cfg() -> ModelConfig {
        ModelConfig::default()
    }

    // ---- supports_adaptive_thinking ----

    #[test]
    fn adaptive_thinking_recognizes_opus_and_sonnet_4_6() {
        assert!(supports_adaptive_thinking("claude-opus-4-6"));
        assert!(supports_adaptive_thinking("claude-sonnet-4-6"));
    }

    #[test]
    fn adaptive_thinking_rejects_other_models() {
        assert!(!supports_adaptive_thinking("claude-opus-3-5"));
        assert!(!supports_adaptive_thinking("claude-haiku-4-5"));
    }

    // ---- parse_cache_write_tokens ----

    #[test]
    fn parse_cache_write_prefers_top_level_cache_creation_input_tokens() {
        let v = json!({"cache_creation_input_tokens": 42});
        assert_eq!(parse_cache_write_tokens(&v), Some(42));
    }

    #[test]
    fn parse_cache_write_sums_ephemeral_5m_and_1h_when_top_level_absent() {
        let v = json!({"cache_creation": {
            "ephemeral_5m_input_tokens": 10,
            "ephemeral_1h_input_tokens": 5,
        }});
        assert_eq!(parse_cache_write_tokens(&v), Some(15));
    }

    #[test]
    fn parse_cache_write_returns_none_when_ephemeral_sum_is_zero() {
        let v = json!({"cache_creation": {
            "ephemeral_5m_input_tokens": 0,
            "ephemeral_1h_input_tokens": 0,
        }});
        assert_eq!(parse_cache_write_tokens(&v), None);
    }

    #[test]
    fn parse_cache_write_returns_none_when_nothing_present() {
        let v = json!({});
        assert_eq!(parse_cache_write_tokens(&v), None);
    }

    // ---- build_body ----

    #[test]
    fn build_body_joins_multiple_system_messages_with_double_newline() {
        let body = build_body(
            &[system("first"), system("second"), user("hi")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "first\n\nsecond");
    }

    #[test]
    fn build_body_omits_system_field_when_no_system_messages() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &cfg());
        assert!(body.get("system").is_none());
    }

    #[test]
    fn build_body_emits_user_message_with_text_content() {
        let body = build_body(&[user("hello")], &[], "m", ReasoningEffort::Off, &cfg());
        let msg = &body["messages"][0];
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"], "hello");
    }

    #[test]
    fn build_body_assistant_text_emits_text_block() {
        let body = build_body(
            &[assistant_text("yo")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        let msg = &body["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], "yo");
    }

    #[test]
    fn build_body_assistant_tool_calls_parsed_as_input_object() {
        let calls = vec![ToolCall::new(
            "id-1".into(),
            FunctionCall {
                name: "search".into(),
                arguments: r#"{"q":"rust"}"#.into(),
            },
        )];
        let body = build_body(
            &[assistant_calls(None, calls)],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "id-1");
        assert_eq!(block["name"], "search");
        assert_eq!(block["input"]["q"], "rust");
    }

    #[test]
    fn build_body_assistant_invalid_argument_json_defaults_to_empty_object() {
        let calls = vec![ToolCall::new(
            "id".into(),
            FunctionCall {
                name: "n".into(),
                arguments: "not json".into(),
            },
        )];
        let body = build_body(
            &[assistant_calls(None, calls)],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        let block = &body["messages"][0]["content"][0];
        assert!(block["input"].is_object());
        assert!(block["input"].as_object().unwrap().is_empty());
    }

    #[test]
    fn build_body_tool_message_emits_user_role_tool_result_block() {
        let body = build_body(
            &[tool_msg(Some("call-7"), "result body")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        let msg = &body["messages"][0];
        assert_eq!(msg["role"], "user");
        let block = &msg["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "call-7");
        assert_eq!(block["content"], "result body");
    }

    #[test]
    fn build_body_tool_message_without_call_id_uses_empty_string() {
        let body = build_body(
            &[tool_msg(None, "x")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        assert_eq!(body["messages"][0]["content"][0]["tool_use_id"], "");
    }

    #[test]
    fn build_body_serializes_tools_with_input_schema() {
        let tools = vec![ToolDefinition::new(FunctionSchema {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        })];
        let body = build_body(&[user("hi")], &tools, "m", ReasoningEffort::Off, &cfg());
        let t = &body["tools"][0];
        assert_eq!(t["name"], "f");
        assert_eq!(t["description"], "d");
        assert_eq!(t["input_schema"]["type"], "object");
    }

    #[test]
    fn build_body_sets_temperature_and_top_p_when_provided() {
        let mut c = cfg();
        c.temperature = Some(0.4);
        c.top_p = Some(0.8);
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &c);
        assert_eq!(body["temperature"], 0.4);
        assert_eq!(body["top_p"], 0.8);
    }

    #[test]
    fn build_body_emits_thinking_and_output_config_for_adaptive_model_with_effort() {
        let body = build_body(
            &[user("hi")],
            &[],
            "claude-sonnet-4-6",
            ReasoningEffort::High,
            &cfg(),
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(
            body["output_config"]["effort"],
            ReasoningEffort::High.label()
        );
    }

    #[test]
    fn build_body_omits_thinking_for_non_adaptive_models_even_with_effort() {
        let body = build_body(
            &[user("hi")],
            &[],
            "claude-haiku-4-5",
            ReasoningEffort::High,
            &cfg(),
        );
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn build_body_omits_thinking_when_effort_is_off() {
        let body = build_body(
            &[user("hi")],
            &[],
            "claude-sonnet-4-6",
            ReasoningEffort::Off,
            &cfg(),
        );
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn build_body_sets_default_max_tokens() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &cfg());
        assert_eq!(body["max_tokens"], 4096);
    }

    // ---- parse_response ----

    #[test]
    fn parse_response_concatenates_text_blocks() {
        let v = json!({"content": [
            {"type": "text", "text": "foo "},
            {"type": "text", "text": "bar"},
        ]});
        let r = parse_response(&v).unwrap();
        assert_eq!(r.content.as_deref(), Some("foo bar"));
    }

    #[test]
    fn parse_response_extracts_thinking_blocks_into_reasoning() {
        let v = json!({"content": [
            {"type": "thinking", "thinking": "ponder"},
            {"type": "thinking", "thinking": "ing"},
        ]});
        let r = parse_response(&v).unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("pondering"));
    }

    #[test]
    fn parse_response_uses_top_level_thinking_when_content_blocks_lack_it() {
        let v = json!({
            "content": [{"type": "text", "text": "hi"}],
            "thinking": [{"text": "summary"}],
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("summary"));
    }

    #[test]
    fn parse_response_extracts_tool_use_blocks() {
        let v = json!({"content": [
            {"type": "tool_use", "id": "id-1", "name": "f", "input": {"q":"x"}}
        ]});
        let r = parse_response(&v).unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "id-1");
        assert_eq!(r.tool_calls[0].function.name, "f");
        // input is serialized via Value::to_string — no spaces in JSON.
        assert!(r.tool_calls[0].function.arguments.contains("\"q\":\"x\""));
    }

    #[test]
    fn parse_response_ignores_unknown_block_types() {
        let v = json!({"content": [{"type":"weird"}]});
        let r = parse_response(&v).unwrap();
        assert!(r.content.is_none());
    }

    #[test]
    fn parse_response_propagates_usage_fields() {
        let v = json!({"usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_input_tokens": 3,
            "cache_creation_input_tokens": 2,
        }});
        let r = parse_response(&v).unwrap();
        assert_eq!(r.usage.prompt_tokens, Some(10));
        assert_eq!(r.usage.completion_tokens, Some(5));
        assert_eq!(r.usage.cache_read_tokens, Some(3));
        assert_eq!(r.usage.cache_write_tokens, Some(2));
    }

    // ---- apply_sse_event ----

    fn step(state: &mut StreamState, ev: serde_json::Value) {
        apply_sse_event(state, &ev, &mut |_| {});
    }

    #[test]
    fn sse_message_start_seeds_usage_input_and_cache_fields() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "message_start",
                "message": {"usage": {
                    "input_tokens": 11,
                    "cache_read_input_tokens": 4,
                    "cache_creation_input_tokens": 7,
                }}
            }),
        );
        assert_eq!(state.usage.prompt_tokens, Some(11));
        assert_eq!(state.usage.cache_read_tokens, Some(4));
        assert_eq!(state.usage.cache_write_tokens, Some(7));
    }

    #[test]
    fn sse_content_block_start_registers_tool_use_by_index() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "content_block_start",
                "index": 3,
                "content_block": {"type":"tool_use", "id":"id3", "name":"f"}
            }),
        );
        let entry = state.tool_calls.get(&3).unwrap();
        assert_eq!(entry.0, "id3");
        assert_eq!(entry.1, "f");
        assert_eq!(entry.2, "");
    }

    #[test]
    fn sse_content_block_start_ignored_for_non_tool_use_blocks() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type":"text"}
            }),
        );
        assert!(state.tool_calls.is_empty());
    }

    #[test]
    fn sse_text_delta_appends_to_content_and_streams() {
        let mut state = StreamState::default();
        let mut got: Vec<String> = Vec::new();
        apply_sse_event(
            &mut state,
            &json!({"type":"content_block_delta", "delta":{"type":"text_delta","text":"hi"}}),
            &mut |d| {
                if let StreamDelta::Text(t) = d {
                    got.push(t.into())
                }
            },
        );
        assert_eq!(state.content, "hi");
        assert_eq!(got, vec!["hi"]);
    }

    #[test]
    fn sse_empty_text_delta_does_not_emit_or_append() {
        let mut state = StreamState::default();
        let mut called = false;
        apply_sse_event(
            &mut state,
            &json!({"type":"content_block_delta", "delta":{"type":"text_delta","text":""}}),
            &mut |_| called = true,
        );
        assert!(state.content.is_empty());
        assert!(!called);
    }

    #[test]
    fn sse_thinking_delta_appends_to_reasoning_and_streams() {
        let mut state = StreamState::default();
        let mut got: Vec<String> = Vec::new();
        apply_sse_event(
            &mut state,
            &json!({"type":"content_block_delta", "delta":{"type":"thinking_delta","thinking":"why"}}),
            &mut |d| {
                if let StreamDelta::Thinking(t) = d {
                    got.push(t.into())
                }
            },
        );
        assert_eq!(state.reasoning, "why");
        assert_eq!(got, vec!["why"]);
    }

    #[test]
    fn sse_input_json_delta_appends_to_matching_tool_call_args() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type":"tool_use", "id":"i", "name":"n"}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type":"input_json_delta", "partial_json":"{\"a\":"}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type":"input_json_delta", "partial_json":"1}"}
            }),
        );
        let r = state.finalize();
        assert_eq!(r.tool_calls[0].function.arguments, "{\"a\":1}");
    }

    #[test]
    fn sse_input_json_delta_ignored_when_no_matching_index() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "content_block_delta", "index": 9,
                "delta": {"type":"input_json_delta", "partial_json":"x"}
            }),
        );
        assert!(state.tool_calls.is_empty());
    }

    #[test]
    fn sse_unknown_delta_type_ignored() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "content_block_delta", "delta": {"type":"weird"}
            }),
        );
        // No effect.
        assert!(state.content.is_empty());
    }

    #[test]
    fn sse_message_delta_sets_completion_tokens() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "message_delta",
                "usage": {"output_tokens": 12}
            }),
        );
        assert_eq!(state.usage.completion_tokens, Some(12));
    }

    #[test]
    fn sse_message_delta_fills_prompt_tokens_only_if_unset() {
        let mut state = StreamState::default();
        state.usage.prompt_tokens = Some(99);
        step(
            &mut state,
            json!({
                "type": "message_delta",
                "usage": {"input_tokens": 5, "output_tokens": 1}
            }),
        );
        // prompt_tokens stays at 99 because message_start already populated it.
        assert_eq!(state.usage.prompt_tokens, Some(99));

        let mut state2 = StreamState::default();
        step(
            &mut state2,
            json!({
                "type": "message_delta",
                "usage": {"input_tokens": 5, "output_tokens": 1}
            }),
        );
        assert_eq!(state2.usage.prompt_tokens, Some(5));
    }

    #[test]
    fn sse_unknown_event_type_is_noop() {
        let mut state = StreamState::default();
        step(&mut state, json!({"type":"who_knows"}));
        assert!(state.content.is_empty());
        assert!(state.tool_calls.is_empty());
    }

    // ---- finalize ----

    #[test]
    fn finalize_returns_tool_calls_sorted_by_index() {
        let mut state = StreamState::default();
        state
            .tool_calls
            .insert(2, ("b".into(), "B".into(), "{}".into()));
        state
            .tool_calls
            .insert(0, ("a".into(), "A".into(), "{}".into()));
        state
            .tool_calls
            .insert(1, ("c".into(), "C".into(), "{}".into()));
        let r = state.finalize();
        let names: Vec<_> = r
            .tool_calls
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert_eq!(names, vec!["A", "C", "B"]);
    }

    #[test]
    fn finalize_empty_content_and_reasoning_become_none() {
        let r = StreamState::default().finalize();
        assert!(r.content.is_none());
        assert!(r.reasoning.is_none());
        assert!(r.tool_calls.is_empty());
    }
}
