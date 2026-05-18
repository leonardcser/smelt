use super::extract::extract_tool_calls_from_text;
use super::{collect_indexed_tool_calls, non_empty, sse};
use super::{ParsedResponse, ProviderError, StreamDelta, ToolDefinition};
use crate::cancel::CancellationToken;
use crate::config::ModelConfig;
use crate::trim::{trim_tool_output, MAX_TOOL_OUTPUT_LINES};
use protocol::{Message, ReasoningEffort, Role, TokenUsage, ToolCall};
use std::collections::HashMap;

pub(super) fn build_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    let api_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let mut v = serde_json::to_value(m).unwrap();
            if let Some(obj) = v.as_object_mut() {
                obj.remove("is_error");
                // Provider-shaped reasoning is only meaningful to the
                // Anthropic / OpenAI Responses round-trip; chat-completions
                // backends would reject the extra field.
                obj.remove("reasoning_details");
                if m.role == Role::Tool {
                    if let Some(s) = obj.get("content").and_then(|c| c.as_str()) {
                        let trimmed = trim_tool_output(s, MAX_TOOL_OUTPUT_LINES);
                        obj.insert("content".into(), serde_json::json!(trimmed));
                    }
                }
                super::sanitize_tool_call_arguments(obj);
            }
            v
        })
        .collect();

    let mut body = serde_json::json!({ "model": model, "messages": api_messages });

    if !tools.is_empty() {
        body["tools"] = serde_json::to_value(tools).unwrap();
    }
    if let Some(v) = config.temperature {
        body["temperature"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_p {
        body["top_p"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_k {
        body["top_k"] = serde_json::json!(v);
    }
    if let Some(v) = config.min_p {
        body["min_p"] = serde_json::json!(v);
    }
    if let Some(v) = config.repeat_penalty {
        body["repeat_penalty"] = serde_json::json!(v);
    }

    let label = effort.label();
    if effort != ReasoningEffort::Off {
        body["reasoning_effort"] = serde_json::json!(label);
        body["chat_template_kwargs"] = serde_json::json!({
            "enable_thinking": true,
            "reasoning_effort": label,
        });
    } else {
        body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
    }

    body
}

pub(super) fn parse_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
    let choice = data["choices"]
        .get(0)
        .ok_or_else(|| ProviderError::InvalidResponse("no choices in response".into()))?;
    let msg = &choice["message"];

    let mut content = msg["content"].as_str().map(|s| s.to_string());
    let mut reasoning = msg["reasoning_content"]
        .as_str()
        .or_else(|| msg["reasoning"].as_str())
        .map(|s| s.to_string());

    let mut tool_calls: Vec<ToolCall> = if let Some(tcs) = msg.get("tool_calls") {
        serde_json::from_value(tcs.clone()).unwrap_or_default()
    } else {
        vec![]
    };

    // Fallback: some backends (vLLM with reasoning+tool calling) may
    // place <tool_call> markup inside `content` or `reasoning_content`.
    if tool_calls.is_empty() {
        let (from_content, cleaned_content) = extract_tool_calls_from_text(content.as_deref());
        let (from_reasoning, cleaned_reasoning) =
            extract_tool_calls_from_text(reasoning.as_deref());
        if !from_content.is_empty() || !from_reasoning.is_empty() {
            tool_calls = from_content.into_iter().chain(from_reasoning).collect();
            content = cleaned_content;
            reasoning = cleaned_reasoning;
        }
    }

    let u = &data["usage"];
    let usage = TokenUsage {
        prompt_tokens: u["prompt_tokens"].as_u64().map(|n| n as u32),
        completion_tokens: u["completion_tokens"].as_u64().map(|n| n as u32),
        cache_read_tokens: u["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .map(|n| n as u32),
        cache_write_tokens: None,
        reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .map(|n| n as u32),
    };

    Ok(ParsedResponse {
        content,
        reasoning,
        reasoning_blocks: None,
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
        let content = non_empty(self.content);
        let reasoning = non_empty(self.reasoning);
        let tool_calls = collect_indexed_tool_calls(self.tool_calls);
        let usage = self.usage;

        if tool_calls.is_empty() {
            let (from_content, cleaned_content) = extract_tool_calls_from_text(content.as_deref());
            let (from_reasoning, cleaned_reasoning) =
                extract_tool_calls_from_text(reasoning.as_deref());
            if !from_content.is_empty() || !from_reasoning.is_empty() {
                let tool_calls: Vec<ToolCall> =
                    from_content.into_iter().chain(from_reasoning).collect();
                return ParsedResponse {
                    content: cleaned_content,
                    reasoning: cleaned_reasoning,
                    reasoning_blocks: None,
                    tool_calls,
                    usage,
                };
            }
        }

        ParsedResponse {
            content,
            reasoning,
            reasoning_blocks: None,
            tool_calls,
            usage,
        }
    }
}

/// Apply one SSE event to the accumulator. Pure (modulo `on_delta`).
pub(super) fn apply_sse_event(
    state: &mut StreamState,
    ev: &serde_json::Value,
    on_delta: &mut dyn FnMut(StreamDelta),
) {
    if let Some(u) = ev.get("usage") {
        state.usage.prompt_tokens = u["prompt_tokens"].as_u64().map(|n| n as u32);
        state.usage.completion_tokens = state
            .usage
            .completion_tokens
            .or(u["completion_tokens"].as_u64().map(|n| n as u32));
        state.usage.cache_read_tokens = u["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .map(|n| n as u32);
        state.usage.reasoning_tokens = u["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .map(|n| n as u32);
    }

    let Some(delta) = ev["choices"].get(0).and_then(|c| c.get("delta")) else {
        return;
    };

    if let Some(text) = delta["content"].as_str() {
        if !text.is_empty() {
            state.content.push_str(text);
            on_delta(StreamDelta::Text(text));
        }
    }

    if let Some(text) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        .and_then(|v| v.as_str())
    {
        if !text.is_empty() {
            state.reasoning.push_str(text);
            on_delta(StreamDelta::Thinking(text));
        }
    }

    if let Some(tcs) = delta["tool_calls"].as_array() {
        for tc in tcs {
            let idx = tc["index"].as_u64().unwrap_or(0) as usize;
            let entry = state.tool_calls.entry(idx).or_insert_with(|| {
                let id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                (id, name, String::new())
            });
            if let Some(id) = tc["id"].as_str() {
                if !id.is_empty() && entry.0.is_empty() {
                    entry.0 = id.to_string();
                }
            }
            if let Some(name) = tc["function"]["name"].as_str() {
                if !name.is_empty() && entry.1.is_empty() {
                    entry.1 = name.to_string();
                }
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
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
    use crate::test_util::{tool_msg as tool_msg_opt, user};
    use protocol::{FunctionCall, Message, ToolCall};
    use serde_json::json;

    fn cfg() -> ModelConfig {
        ModelConfig::default()
    }

    fn tool_msg(call_id: &str, output: &str) -> Message {
        tool_msg_opt(Some(call_id), output)
    }

    // ---- build_body ----

    #[test]
    fn build_body_includes_model_and_messages() {
        let body = build_body(&[user("hi")], &[], "model-x", ReasoningEffort::Off, &cfg());
        assert_eq!(body["model"], "model-x");
        assert!(body["messages"].is_array());
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn build_body_drops_is_error_field_from_messages() {
        let m = Message {
            is_error: true,
            ..user("hi")
        };
        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());
        assert!(body["messages"][0].get("is_error").is_none());
    }

    #[test]
    fn build_body_strips_reasoning_details_from_messages() {
        use protocol::ReasoningBlock;
        let mut m = user("hi");
        m.reasoning_details = Some(vec![ReasoningBlock {
            provider: ReasoningBlock::ANTHROPIC.to_string(),
            data: serde_json::json!({"type": "thinking", "thinking": "x"}),
        }]);
        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());
        assert!(body["messages"][0].get("reasoning_details").is_none());
    }

    #[test]
    fn build_body_tool_message_content_is_trimmed() {
        // trim_tool_output applies to the content string; many short lines pass through unchanged.
        let body = build_body(
            &[tool_msg("call-1", "ok")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
        );
        assert_eq!(body["messages"][0]["content"], "ok");
    }

    #[test]
    fn build_body_sanitizes_invalid_tool_call_arguments_to_empty_object_string() {
        let m = Message {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: Some(vec![ToolCall::new(
                "id".into(),
                FunctionCall {
                    name: "f".into(),
                    arguments: "not json".into(),
                },
            )]),
            tool_call_id: None,
            is_error: false,
        };
        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());
        let args = &body["messages"][0]["tool_calls"][0]["function"]["arguments"];
        assert_eq!(args, "{}");
    }

    #[test]
    fn build_body_keeps_valid_tool_call_arguments_unchanged() {
        let m = Message {
            role: Role::Assistant,
            content: None,
            reasoning_content: None,

            reasoning_details: None,
            tool_calls: Some(vec![ToolCall::new(
                "id".into(),
                FunctionCall {
                    name: "f".into(),
                    arguments: r#"{"a":1}"#.into(),
                },
            )]),
            tool_call_id: None,
            is_error: false,
        };
        let body = build_body(&[m], &[], "m", ReasoningEffort::Off, &cfg());
        let args = &body["messages"][0]["tool_calls"][0]["function"]["arguments"];
        assert_eq!(args, r#"{"a":1}"#);
    }

    #[test]
    fn build_body_serializes_tools_when_provided() {
        let tools = vec![ToolDefinition::new(FunctionSchema {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        })];
        let body = build_body(&[user("hi")], &tools, "m", ReasoningEffort::Off, &cfg());
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"][0]["function"]["name"], "f");
    }

    #[test]
    fn build_body_omits_tools_when_empty() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &cfg());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_body_threads_temperature_top_p_top_k_min_p_repeat_penalty() {
        let mut c = cfg();
        c.temperature = Some(0.5);
        c.top_p = Some(0.9);
        c.top_k = Some(40);
        c.min_p = Some(0.05);
        c.repeat_penalty = Some(1.1);
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &c);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["min_p"], 0.05);
        assert_eq!(body["repeat_penalty"], 1.1);
    }

    #[test]
    fn build_body_thinking_disabled_when_effort_off() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::Off, &cfg());
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_body_thinking_enabled_when_effort_set() {
        let body = build_body(&[user("hi")], &[], "m", ReasoningEffort::High, &cfg());
        let label = ReasoningEffort::High.label();
        assert_eq!(body["reasoning_effort"], label);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(body["chat_template_kwargs"]["reasoning_effort"], label);
    }

    // ---- parse_response ----

    #[test]
    fn parse_response_returns_error_when_no_choices() {
        let v = json!({});
        match parse_response(&v) {
            Err(ProviderError::InvalidResponse(_)) => {}
            _ => panic!("expected InvalidResponse"),
        }
    }

    #[test]
    fn parse_response_extracts_content_and_reasoning_content() {
        let v = json!({
            "choices": [{"message": {
                "content": "hello",
                "reasoning_content": "ponder",
            }}],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.content.as_deref(), Some("hello"));
        assert_eq!(r.reasoning.as_deref(), Some("ponder"));
    }

    #[test]
    fn parse_response_falls_back_to_reasoning_field_when_reasoning_content_absent() {
        let v = json!({
            "choices": [{"message": {"reasoning": "alt"}}],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("alt"));
    }

    #[test]
    fn parse_response_extracts_native_tool_calls() {
        let v = json!({
            "choices": [{"message": {
                "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "f", "arguments": "{\"a\":1}"}}
                ]
            }}],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "c1");
        assert_eq!(r.tool_calls[0].function.name, "f");
    }

    #[test]
    fn parse_response_falls_back_to_extracting_tool_calls_from_content_markup() {
        let v = json!({
            "choices": [{"message": {
                "content": "let me search\n<tool_call>\n{\"name\":\"search\",\"arguments\":{\"q\":\"x\"}}\n</tool_call>"
            }}],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].function.name, "search");
        assert_eq!(r.content.as_deref(), Some("let me search"));
    }

    #[test]
    fn parse_response_falls_back_to_extracting_tool_calls_from_reasoning_markup() {
        let v = json!({
            "choices": [{"message": {
                "reasoning_content": "thinking\n<tool_call>\n{\"name\":\"f\",\"arguments\":{}}\n</tool_call>"
            }}],
            "usage": {}
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].function.name, "f");
        assert_eq!(r.reasoning.as_deref(), Some("thinking"));
    }

    #[test]
    fn parse_response_propagates_usage() {
        let v = json!({
            "choices": [{"message": {}}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 3},
                "completion_tokens_details": {"reasoning_tokens": 1},
            }
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.usage.prompt_tokens, Some(10));
        assert_eq!(r.usage.completion_tokens, Some(5));
        assert_eq!(r.usage.cache_read_tokens, Some(3));
        assert_eq!(r.usage.reasoning_tokens, Some(1));
        assert_eq!(r.usage.cache_write_tokens, None);
    }

    // ---- apply_sse_event ----

    fn step(state: &mut StreamState, ev: serde_json::Value) {
        apply_sse_event(state, &ev, &mut |_| {});
    }

    #[test]
    fn sse_top_level_usage_populates_token_fields() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "usage": {
                    "prompt_tokens": 7,
                    "completion_tokens": 3,
                    "prompt_tokens_details": {"cached_tokens": 2},
                    "completion_tokens_details": {"reasoning_tokens": 1},
                }
            }),
        );
        assert_eq!(state.usage.prompt_tokens, Some(7));
        assert_eq!(state.usage.completion_tokens, Some(3));
        assert_eq!(state.usage.cache_read_tokens, Some(2));
        assert_eq!(state.usage.reasoning_tokens, Some(1));
    }

    #[test]
    fn sse_completion_tokens_only_set_when_unset() {
        let mut state = StreamState::default();
        state.usage.completion_tokens = Some(99);
        step(&mut state, json!({"usage": {"completion_tokens": 1}}));
        assert_eq!(state.usage.completion_tokens, Some(99));
    }

    #[test]
    fn sse_content_delta_appends_and_streams_text() {
        let mut state = StreamState::default();
        let mut got: Vec<String> = Vec::new();
        apply_sse_event(
            &mut state,
            &json!({"choices":[{"delta":{"content":"hi"}}]}),
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
    fn sse_empty_content_is_ignored() {
        let mut state = StreamState::default();
        let mut called = false;
        apply_sse_event(
            &mut state,
            &json!({"choices":[{"delta":{"content":""}}]}),
            &mut |_| called = true,
        );
        assert!(state.content.is_empty());
        assert!(!called);
    }

    #[test]
    fn sse_reasoning_content_appends_and_streams_thinking() {
        let mut state = StreamState::default();
        let mut got: Vec<String> = Vec::new();
        apply_sse_event(
            &mut state,
            &json!({"choices":[{"delta":{"reasoning_content":"why"}}]}),
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
    fn sse_reasoning_field_used_as_fallback_when_reasoning_content_absent() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({"choices":[{"delta":{"reasoning":"alt"}}]}),
        );
        assert_eq!(state.reasoning, "alt");
    }

    #[test]
    fn sse_tool_call_delta_creates_entry_and_accumulates_arguments() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "choices":[{"delta":{"tool_calls":[
                    {"index":0, "id":"c1", "function":{"name":"f", "arguments":"{\"a\":"}}
                ]}}]
            }),
        );
        step(
            &mut state,
            json!({
                "choices":[{"delta":{"tool_calls":[
                    {"index":0, "function":{"arguments":"1}"}}
                ]}}]
            }),
        );
        let r = state.finalize();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "c1");
        assert_eq!(r.tool_calls[0].function.name, "f");
        assert_eq!(r.tool_calls[0].function.arguments, "{\"a\":1}");
    }

    #[test]
    fn sse_tool_call_without_index_defaults_to_zero() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "choices":[{"delta":{"tool_calls":[
                    {"id":"c0", "function":{"name":"f", "arguments":"{}"}}
                ]}}]
            }),
        );
        assert!(state.tool_calls.contains_key(&0));
    }

    #[test]
    fn sse_event_without_choices_only_updates_usage() {
        let mut state = StreamState::default();
        step(&mut state, json!({"usage": {"prompt_tokens": 5}}));
        assert_eq!(state.usage.prompt_tokens, Some(5));
        assert!(state.content.is_empty());
    }

    // ---- finalize ----

    #[test]
    fn finalize_extracts_tool_calls_from_content_markup_when_native_missing() {
        let state = StreamState {
            content: "<tool_call>\n{\"name\":\"f\",\"arguments\":{}}\n</tool_call>".into(),
            ..Default::default()
        };
        let r = state.finalize();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].function.name, "f");
    }

    #[test]
    fn finalize_extracts_tool_calls_from_reasoning_markup_when_native_missing() {
        let state = StreamState {
            reasoning: "<tool_call>\n{\"name\":\"g\",\"arguments\":{}}\n</tool_call>".into(),
            ..Default::default()
        };
        let r = state.finalize();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].function.name, "g");
    }

    #[test]
    fn finalize_skips_text_extraction_when_native_tool_calls_present() {
        let mut state = StreamState {
            content: "<tool_call>\n{\"name\":\"FROM_CONTENT\",\"arguments\":{}}\n</tool_call>"
                .into(),
            ..Default::default()
        };
        state
            .tool_calls
            .insert(0, ("c0".into(), "native".into(), "{}".into()));
        let r = state.finalize();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].function.name, "native");
        // Content remains untouched (still contains the markup).
        assert!(r.content.as_deref().unwrap().contains("<tool_call>"));
    }

    #[test]
    fn finalize_empty_state_produces_none_fields() {
        let r = StreamState::default().finalize();
        assert!(r.content.is_none());
        assert!(r.reasoning.is_none());
        assert!(r.tool_calls.is_empty());
    }
}
