use crate::extract::extract_tool_calls_from_text;
use crate::sse;
use crate::{
    collect_indexed_tool_calls, non_empty, sanitize_tool_call_arguments, CancellationToken,
    CompletedReasoningPart, ModelConfig, ParsedResponse, ProviderError, ProviderStreamEvent,
    ReasoningStreamEvent, ToolCallStreamEvent, ToolDefinition,
};
use base64::Engine as _;
use protocol::{
    Message, ReasoningBlock, ReasoningEffort, ReasoningKind, Role, TokenUsage, ToolCall,
};
use sha2::{Digest, Sha256};

use std::collections::HashMap;

fn add_tokens(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_usage(u: &serde_json::Value) -> TokenUsage {
    let total_prompt = u["prompt_tokens"].as_u64().map(|n| n as u32);
    let cached = u["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .map(|n| n as u32);
    let completion = u["completion_tokens"].as_u64().map(|n| n as u32);
    let total = u["total_tokens"].as_u64().map(|n| n as u32);
    TokenUsage {
        context_tokens: total.or_else(|| add_tokens(total_prompt, completion)),
        prompt_tokens: match (total_prompt, cached) {
            (Some(t), Some(c)) => Some(t.saturating_sub(c)),
            (t, _) => t,
        },
        completion_tokens: completion,
        cache_read_tokens: cached,
        cache_write_tokens: None,
        reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .map(|n| n as u32),
    }
}

const REASONING_FIELDS: &[&str] = &["reasoning_content", "reasoning", "reasoning_text"];

pub(crate) struct Target<'a> {
    pub model: &'a str,
    pub origin: serde_json::Value,
}

impl<'a> Target<'a> {
    pub fn new(provider: crate::ProviderKind, endpoint: &str, model: &'a str) -> Self {
        Self {
            model,
            origin: reasoning_origin(provider, endpoint, model),
        }
    }
}

/// Bind raw reasoning to its transport and model without persisting credentials in URLs.
pub(crate) fn reasoning_origin(
    provider: crate::ProviderKind,
    endpoint: &str,
    model: &str,
) -> serde_json::Value {
    serde_json::json!({
        "provider": provider.as_config_str(),
        "endpoint_sha256": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(endpoint.as_bytes())),
        "model": model,
    })
}

/// All turns remain available to the server's chat template. Thinking-generation
/// settings do not invalidate reasoning already produced during a tool flow.
fn replay_field<'a>(message: &'a Message, origin: &serde_json::Value) -> Option<&'a str> {
    if message.role != Role::Assistant {
        return None;
    }
    message
        .reasoning_details
        .iter()
        .flatten()
        .find_map(|block| {
            if block.provider != ReasoningBlock::CHAT_COMPLETIONS || block.data["origin"] != *origin
            {
                return None;
            }
            block.data["field"]
                .as_str()
                .filter(|field| REASONING_FIELDS.contains(field))
        })
}

fn incoming_reasoning(message: &serde_json::Value) -> Option<(&'static str, &str)> {
    // Prefer nonempty fields; some endpoints return multiple aliases in one delta.
    let fields = || {
        REASONING_FIELDS
            .iter()
            .filter_map(|&field| message[field].as_str().map(|text| (field, text)))
    };
    fields()
        .find(|(_, text)| !text.is_empty())
        .or_else(|| fields().next())
}

fn raw_reasoning_blocks(field: Option<&str>) -> Option<Vec<ReasoningBlock>> {
    field.map(|field| {
        vec![ReasoningBlock {
            provider: ReasoningBlock::CHAT_COMPLETIONS.into(),
            data: serde_json::json!({ "field": field }),
        }]
    })
}

fn sanitize_message_for_chat_completions(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    message: &Message,
    origin: &serde_json::Value,
) {
    let Some(role) = obj.get("role").and_then(|v| v.as_str()) else {
        return;
    };
    let allowed: &[&str] = match role {
        "system" | "user" => &["role", "content"],
        "assistant" => &["role", "content", "tool_calls"],
        "tool" => &["role", "content", "tool_call_id"],
        _ => &["role", "content"],
    };
    let reasoning = obj.remove("reasoning_content");
    obj.retain(|key, _| allowed.contains(&key.as_str()));
    if let Some(field) = replay_field(message, origin) {
        obj.insert(
            field.into(),
            reasoning.unwrap_or_else(|| serde_json::json!("")),
        );
    }
}

pub fn build_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    target: &Target<'_>,
    effort: ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    let api_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let mut v = serde_json::to_value(m).unwrap();
            if let Some(obj) = v.as_object_mut() {
                sanitize_tool_call_arguments(obj);
                sanitize_message_for_chat_completions(obj, m, &target.origin);
            }
            v
        })
        .collect();

    let mut body = serde_json::json!({ "model": target.model, "messages": api_messages });

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
    if let Some(v) = config.max_tokens {
        body["max_tokens"] = serde_json::json!(v);
    }
    if let Some(v) = config.thinking_token_budget {
        body["thinking_token_budget"] = serde_json::json!(v);
    }
    if let Some(kwargs) = &config.chat_template_kwargs {
        body["chat_template_kwargs"] = serde_json::json!(kwargs);
    }

    if effort != ReasoningEffort::Off {
        body["reasoning_effort"] = serde_json::json!(effort.label());
    } else if config.supports_reasoning == Some(true) {
        // Omitting the parameter leaves thinking-enabled servers at their default.
        body["reasoning_effort"] = serde_json::json!("none");
    }

    body
}

fn raw_reasoning_parts(reasoning: Option<&str>) -> Vec<CompletedReasoningPart> {
    reasoning
        .map(|content| {
            vec![CompletedReasoningPart {
                kind: ReasoningKind::Raw,
                content: content.to_string(),
            }]
        })
        .unwrap_or_default()
}

pub fn parse_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
    let choice = data["choices"]
        .get(0)
        .ok_or_else(|| ProviderError::InvalidResponse("no choices in response".into()))?;
    let msg = &choice["message"];

    let mut content = msg["content"].as_str().map(|s| s.to_string());
    let incoming = incoming_reasoning(msg);
    let mut reasoning = incoming.map(|(_, text)| text.to_owned());
    let reasoning_blocks = raw_reasoning_blocks(incoming.map(|(field, _)| field));

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

    let usage = parse_usage(&data["usage"]);

    Ok(ParsedResponse {
        finish_reason: choice["finish_reason"].as_str().map(str::to_owned),
        content,
        reasoning_parts: raw_reasoning_parts(reasoning.as_deref()),
        reasoning,
        reasoning_blocks,
        tool_calls,
        usage,
    })
}

/// Accumulator for one streaming response. Mutated by `apply_sse_event`.
#[derive(Default)]
struct StreamState {
    content: String,
    reasoning: String,
    reasoning_field: Option<&'static str>,
    /// content block index -> (id, name, args-json)
    tool_calls: HashMap<usize, (String, String, String)>,
    usage: TokenUsage,
    finish_reason: Option<String>,
    emitted_tool_finishes: bool,
}

impl StreamState {
    fn finalize(self) -> ParsedResponse {
        let content = non_empty(self.content);
        let reasoning = non_empty(self.reasoning);
        let reasoning_blocks = raw_reasoning_blocks(self.reasoning_field);
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
                    finish_reason: self.finish_reason,
                    content: cleaned_content,
                    reasoning_parts: raw_reasoning_parts(cleaned_reasoning.as_deref()),
                    reasoning: cleaned_reasoning,
                    reasoning_blocks,
                    tool_calls,
                    usage,
                };
            }
        }

        ParsedResponse {
            finish_reason: self.finish_reason,
            content,
            reasoning_parts: raw_reasoning_parts(reasoning.as_deref()),
            reasoning,
            reasoning_blocks,
            tool_calls,
            usage,
        }
    }
}

fn finish_stream_state(
    state: StreamState,
    summary: sse::StreamSummary,
) -> Result<ParsedResponse, ProviderError> {
    if state.finish_reason.is_none() {
        return Err(ProviderError::InvalidResponse(format!(
            "stream ended without finish_reason (data_events={}, done={})",
            summary.data_events, summary.saw_done
        )));
    }
    for (index, (id, name, args)) in &state.tool_calls {
        if id.is_empty() || name.is_empty() {
            return Err(ProviderError::InvalidResponse(format!(
                "incomplete tool-call metadata (index={index})"
            )));
        }
        if serde_json::from_str::<serde_json::Value>(args).is_err() {
            return Err(ProviderError::InvalidResponse(format!(
                "invalid or incomplete tool-call arguments (index={index}, bytes={})",
                args.len()
            )));
        }
    }
    Ok(state.finalize())
}

#[cfg_attr(not(any(test, feature = "fuzz")), allow(dead_code))]
pub fn parse_stream_events<'a>(
    events: impl IntoIterator<Item = &'a serde_json::Value>,
    on_delta: &mut dyn FnMut(ProviderStreamEvent),
) -> Result<ParsedResponse, ProviderError> {
    let mut state = StreamState::default();
    let mut summary = sse::StreamSummary::default();
    for ev in events {
        summary.data_events += 1;
        crate::error::check_openai_stream_error(ev)?;
        apply_sse_event(&mut state, ev, on_delta);
    }
    finish_stream_state(state, summary)
}

/// Apply one SSE event to the accumulator. Pure (modulo `on_delta`).
fn apply_sse_event(
    state: &mut StreamState,
    ev: &serde_json::Value,
    on_delta: &mut dyn FnMut(ProviderStreamEvent),
) {
    if let Some(u) = ev.get("usage") {
        let parsed = parse_usage(u);
        state.usage.context_tokens = parsed.context_tokens.or(state.usage.context_tokens);
        state.usage.prompt_tokens = parsed.prompt_tokens.or(state.usage.prompt_tokens);
        state.usage.completion_tokens = state.usage.completion_tokens.or(parsed.completion_tokens);
        state.usage.cache_read_tokens = parsed.cache_read_tokens.or(state.usage.cache_read_tokens);
        state.usage.reasoning_tokens = parsed.reasoning_tokens.or(state.usage.reasoning_tokens);
    }

    let choice = ev["choices"].get(0);
    let mut saw_finish_reason = false;
    if let Some(reason) = choice.and_then(|c| c["finish_reason"].as_str()) {
        state.finish_reason = Some(reason.to_owned());
        saw_finish_reason = true;
    }

    let Some(delta) = choice.and_then(|c| c.get("delta")) else {
        emit_tool_finishes(state, on_delta, saw_finish_reason);
        return;
    };

    // A single delta can contain the reasoning tail and the answer prefix.
    // Emit reasoning first so downstream consumers see the channel transition
    // in semantic order, independent of how the provider batches tokens.
    if let Some((field, text)) = incoming_reasoning(delta) {
        if state.reasoning.is_empty() {
            state.reasoning_field = Some(field);
        }
        if !text.is_empty() {
            state.reasoning.push_str(text);
            on_delta(ProviderStreamEvent::Reasoning(
                ReasoningStreamEvent::Delta {
                    item_id: "reasoning",
                    part_index: 0,
                    kind: ReasoningKind::Raw,
                    delta: text,
                },
            ));
        }
    }

    if let Some(text) = delta["content"].as_str() {
        if !text.is_empty() {
            state.content.push_str(text);
            on_delta(ProviderStreamEvent::TextDelta(text));
        }
    }

    if let Some(tcs) = delta["tool_calls"].as_array() {
        for tc in tcs {
            let idx = tc["index"].as_u64().unwrap_or(0) as usize;
            let stream_id = idx.to_string();
            let mut started = false;
            let entry = state.tool_calls.entry(idx).or_insert_with(|| {
                started = true;
                let id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                (id, name, String::new())
            });
            let old_call_id_empty = entry.0.is_empty();
            let old_name_empty = entry.1.is_empty();
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
            let metadata_changed = (old_call_id_empty && !entry.0.is_empty())
                || (old_name_empty && !entry.1.is_empty());
            if started || metadata_changed {
                on_delta(ProviderStreamEvent::ToolCall(
                    ToolCallStreamEvent::Started {
                        stream_id: &stream_id,
                        call_id: (!entry.0.is_empty()).then_some(entry.0.as_str()),
                        tool_name: (!entry.1.is_empty()).then_some(entry.1.as_str()),
                    },
                ));
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                if !args.is_empty() {
                    entry.2.push_str(args);
                    on_delta(ProviderStreamEvent::ToolCall(
                        ToolCallStreamEvent::ArgsDelta {
                            stream_id: &stream_id,
                            call_id: (!entry.0.is_empty()).then_some(entry.0.as_str()),
                            tool_name: (!entry.1.is_empty()).then_some(entry.1.as_str()),
                            delta: args,
                        },
                    ));
                }
            }
        }
    }
    emit_tool_finishes(state, on_delta, saw_finish_reason);
}

fn emit_tool_finishes(
    state: &mut StreamState,
    on_delta: &mut dyn FnMut(ProviderStreamEvent),
    saw_finish_reason: bool,
) {
    if !saw_finish_reason || state.emitted_tool_finishes {
        return;
    }
    state.emitted_tool_finishes = true;
    for (idx, (call_id, name, args)) in &state.tool_calls {
        if call_id.is_empty()
            || name.is_empty()
            || serde_json::from_str::<serde_json::Value>(args).is_err()
        {
            continue;
        }
        let stream_id = idx.to_string();
        on_delta(ProviderStreamEvent::ToolCall(
            ToolCallStreamEvent::Finished {
                stream_id: &stream_id,
                call_id,
                tool_name: name,
                arguments: args,
            },
        ));
    }
}

pub async fn read_stream(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    on_delta: &(dyn Fn(ProviderStreamEvent) + Send + Sync),
) -> Result<ParsedResponse, ProviderError> {
    let mut state = StreamState::default();

    let summary = sse::read_events(resp, cancel, |event| {
        let ev = crate::error::parse_openai_stream_event(event)?;
        apply_sse_event(&mut state, &ev, &mut |d| on_delta(d));
        Ok(())
    })
    .await?;

    finish_stream_state(state, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FunctionSchema;
    use protocol::{Content, FunctionCall, Message, Role, ToolCall};
    use serde_json::json;

    fn cfg() -> ModelConfig {
        ModelConfig::default()
    }

    fn user(content: &str) -> Message {
        Message::user(Content::text(content))
    }

    fn tool_msg(call_id: &str, output: &str) -> Message {
        Message {
            role: Role::Tool,
            content: Some(Content::text(output)),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
            is_error: false,
            tool_metadata: None,
        }
    }

    fn message_keys(message: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = message
            .as_object()
            .unwrap()
            .keys()
            .map(|key| key.to_string())
            .collect();
        keys.sort();
        keys
    }

    fn replay_assistant(field: &str, origin: &serde_json::Value) -> Message {
        let mut response = json!({"choices": [{"message": {"content": "answer"}}]});
        response["choices"][0]["message"][field] = json!("prior thinking");
        let mut parsed = parse_response(&response).unwrap();
        for block in parsed.reasoning_blocks.iter_mut().flatten() {
            block.data["origin"] = origin.clone();
        }
        Message::assistant_with_reasoning(
            parsed.content.map(Content::text),
            parsed.reasoning,
            parsed.reasoning_blocks,
            None,
        )
    }

    fn target(model: &str) -> Target<'_> {
        Target::new(
            crate::ProviderKind::OpenAiCompatible,
            "http://local/chat/completions",
            model,
        )
    }

    fn replay_body(messages: &[Message], origin: &serde_json::Value) -> serde_json::Value {
        let target = Target {
            model: origin["model"].as_str().unwrap(),
            origin: origin.clone(),
        };
        build_body(messages, &[], &target, ReasoningEffort::Off, &cfg())
    }

    #[test]
    fn reasoning_replay_preserves_received_field_and_excludes_internal_metadata() {
        let origin = reasoning_origin(
            crate::ProviderKind::OpenAiCompatible,
            "http://local/v1/chat/completions",
            "custom-model",
        );
        for &field in REASONING_FIELDS {
            let mut assistant = replay_assistant(field, &origin);
            assistant.tool_calls = Some(vec![ToolCall::new(
                "call".into(),
                FunctionCall {
                    name: "probe".into(),
                    arguments: "{}".into(),
                },
            )]);
            assistant.is_error = true;
            assistant.tool_metadata = Some(json!({"summary": "internal"}));
            assistant.reasoning_details.as_mut().unwrap().push(ReasoningBlock {
                provider: ReasoningBlock::ANTHROPIC.into(),
                data: json!({"type": "thinking", "signature": "native-secret", "thinking": "native"}),
            });
            let mut user = user("hi");
            user.reasoning_content = Some("user metadata".into());
            user.reasoning_details = assistant.reasoning_details.clone();
            let mut tool = tool_msg("call", "ok");
            tool.reasoning_content = Some("tool metadata".into());
            tool.reasoning_details = assistant.reasoning_details.clone();
            tool.tool_metadata = Some(json!({"summary": "internal"}));
            let body = replay_body(&[user, assistant, tool], &origin);
            assert_eq!(body["messages"][1][field], "prior thinking");
            let mut expected = vec!["content", "role", "tool_calls", field];
            expected.sort();
            assert_eq!(message_keys(&body["messages"][1]), expected);
            assert_eq!(message_keys(&body["messages"][0]), ["content", "role"]);
            assert_eq!(
                message_keys(&body["messages"][2]),
                ["content", "role", "tool_call_id"]
            );
            assert!(!body.to_string().contains("native-secret"));
            assert!(!body.to_string().contains("endpoint_sha256"));
        }
    }

    #[test]
    fn reasoning_origin_does_not_persist_url_credentials() {
        let origin = reasoning_origin(
            crate::ProviderKind::OpenAiCompatible,
            "https://user:synthetic-password@local/chat/completions?key=synthetic-key",
            "custom-model",
        );
        let encoded = origin.to_string();
        assert!(!encoded.contains("synthetic-password"));
        assert!(!encoded.contains("synthetic-key"));
    }

    #[test]
    fn reasoning_replay_rejects_switches_unknown_origin_and_unrecognized_fields() {
        let kind = crate::ProviderKind::OpenAiCompatible;
        let origin = reasoning_origin(kind, "http://local/chat/completions", "custom-model");
        let assistant = replay_assistant("reasoning_content", &origin);
        for switched in [
            reasoning_origin(kind, "http://other/chat/completions", "custom-model"),
            reasoning_origin(kind, "http://local/chat/completions", "other-model"),
            reasoning_origin(
                crate::ProviderKind::Copilot,
                "http://local/chat/completions",
                "custom-model",
            ),
        ] {
            let body = replay_body(std::slice::from_ref(&assistant), &switched);
            assert_eq!(message_keys(&body["messages"][0]), ["content", "role"]);
        }
        for details in [
            None,
            raw_reasoning_blocks(Some("reasoning_content")),
            Some(vec![ReasoningBlock {
                provider: ReasoningBlock::ANTHROPIC.into(),
                data: json!({"field": "reasoning_content", "origin": origin}),
            }]),
            Some(vec![ReasoningBlock {
                provider: ReasoningBlock::OPENAI_RESPONSES.into(),
                data: json!({"field": "reasoning_content", "origin": origin}),
            }]),
            Some(vec![ReasoningBlock {
                provider: ReasoningBlock::CHAT_COMPLETIONS.into(),
                data: json!({"field": "tool_metadata", "origin": origin}),
            }]),
        ] {
            let mut assistant = assistant.clone();
            assistant.reasoning_details = details;
            let body = replay_body(&[assistant], &origin);
            assert_eq!(message_keys(&body["messages"][0]), ["content", "role"]);
        }
    }

    #[test]
    fn reasoning_replay_keeps_older_turns_after_history_round_trip_and_thinking_off() {
        let origin = reasoning_origin(
            crate::ProviderKind::OpenAiCompatible,
            "http://local/chat/completions",
            "custom-model",
        );
        let messages = vec![
            user("first question"),
            replay_assistant("reasoning_content", &origin),
            user("new question"),
            replay_assistant("reasoning", &origin),
        ];
        let history = protocol::history_from_messages(messages);
        let stored = serde_json::to_value(history).unwrap();
        let restored: Vec<protocol::HistoryItem> = serde_json::from_value(stored).unwrap();
        let messages = protocol::history_to_messages(&restored);
        let config = ModelConfig {
            supports_reasoning: Some(true),
            chat_template_kwargs: Some(serde_json::Map::from_iter([(
                "enable_thinking".into(),
                json!(false),
            )])),
            ..Default::default()
        };
        let target = Target {
            model: "custom-model",
            origin,
        };
        let body = build_body(&messages, &[], &target, ReasoningEffort::Off, &config);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["messages"][1]["reasoning_content"], "prior thinking");
        assert_eq!(body["messages"][3]["reasoning"], "prior thinking");
    }

    #[test]
    fn reasoning_replay_records_stream_and_nonstream_fields_including_empty_reasoning() {
        let origin = reasoning_origin(
            crate::ProviderKind::OpenAiCompatible,
            "http://local/chat/completions",
            "custom-model",
        );
        for &field in REASONING_FIELDS {
            for text in ["", "thinking"] {
                let mut message = json!({"content": "answer"});
                message[field] = json!(text);
                let response = json!({"choices": [{"message": message}]});
                let events = [json!({"choices": [{"delta": message, "finish_reason": "stop"}]})];
                for mut parsed in [
                    parse_response(&response).unwrap(),
                    parse_stream_events(&events, &mut |_| {}).unwrap(),
                ] {
                    let blocks = parsed.reasoning_blocks.as_mut().unwrap();
                    assert_eq!(blocks[0].data["field"], field);
                    blocks[0].data["origin"] = origin.clone();
                    let assistant = Message::assistant_with_reasoning(
                        parsed.content.map(Content::text),
                        parsed.reasoning,
                        parsed.reasoning_blocks,
                        None,
                    );
                    assert_eq!(
                        replay_body(&[assistant], &origin)["messages"][0][field],
                        text
                    );
                }
            }
        }
    }

    #[test]
    fn chat_reasoning_is_not_replayed_as_anthropic_or_responses_reasoning() {
        let origin = reasoning_origin(
            crate::ProviderKind::OpenAiCompatible,
            "http://local/chat/completions",
            "custom-model",
        );
        let messages = [replay_assistant("reasoning_content", &origin)];
        let anthropic = crate::anthropic::build_body(
            &messages,
            &[],
            "claude",
            ReasoningEffort::High,
            &cfg(),
            &crate::CacheConfig::default(),
        );
        let responses =
            crate::openai::build_body(&messages, &[], "gpt", ReasoningEffort::High, &cfg());
        for body in [anthropic, responses] {
            assert!(!body.to_string().contains("prior thinking"));
            assert!(!body.to_string().contains("endpoint_sha256"));
            assert!(!body.to_string().contains("reasoning_content"));
        }
    }

    #[test]
    fn reasoning_parser_uses_nonempty_alias_without_duplicating_reasoning() {
        let events = [
            json!({"choices": [{"delta": {"reasoning_content": "", "reasoning": "first"}}]}),
            json!({"choices": [{"delta": {"reasoning": " second", "reasoning_text": " second"}, "finish_reason": "stop"}]}),
        ];
        let parsed = parse_stream_events(&events, &mut |_| {}).unwrap();
        assert_eq!(parsed.reasoning.as_deref(), Some("first second"));
        assert_eq!(
            parsed.reasoning_blocks.unwrap()[0].data["field"],
            "reasoning"
        );
    }

    // ---- build_body ----

    #[test]
    fn build_body_includes_model_and_messages() {
        let body = build_body(
            &[user("hi")],
            &[],
            &target("model-x"),
            ReasoningEffort::Off,
            &cfg(),
        );
        assert_eq!(body["model"], "model-x");
        assert!(body["messages"].is_array());
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn build_body_preserves_explicit_thinking_token_budget() {
        for budget in [None, Some(0), Some(8192), Some(u32::MAX)] {
            for effort in [ReasoningEffort::Off, ReasoningEffort::High] {
                let config = ModelConfig {
                    max_tokens: Some(32768),
                    thinking_token_budget: budget,
                    ..Default::default()
                };
                let body = build_body(&[user("hi")], &[], &target("m"), effort, &config);
                assert_eq!(body["max_tokens"], 32768);
                match budget {
                    Some(value) => assert_eq!(body["thinking_token_budget"], value),
                    None => assert!(body.get("thinking_token_budget").is_none()),
                }
            }
        }
    }

    #[test]
    fn build_body_preserves_explicit_chat_template_options() {
        for enabled in [true, false] {
            let kwargs = serde_json::Map::from_iter([
                ("enable_thinking".into(), json!(enabled)),
                ("template_option".into(), json!({"value": "custom"})),
            ]);
            let config = ModelConfig {
                chat_template_kwargs: Some(kwargs.clone()),
                ..Default::default()
            };
            let body = build_body(
                &[user("hi")],
                &[],
                &target("m"),
                ReasoningEffort::Off,
                &config,
            );
            assert_eq!(body["chat_template_kwargs"], json!(kwargs));
            assert!(body.get("reasoning_effort").is_none());
        }
        let body = build_body(
            &[user("hi")],
            &[],
            &target("m"),
            ReasoningEffort::Off,
            &cfg(),
        );
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn build_body_drops_is_error_field_from_messages() {
        let m = Message {
            is_error: true,
            ..user("hi")
        };
        let body = build_body(&[m], &[], &target("m"), ReasoningEffort::Off, &cfg());
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
        let body = build_body(&[m], &[], &target("m"), ReasoningEffort::Off, &cfg());
        assert!(body["messages"][0].get("reasoning_details").is_none());
    }

    #[test]
    fn build_body_preserves_prepared_tool_message_content() {
        let body = build_body(
            &[tool_msg("call-1", "ok")],
            &[],
            &target("m"),
            ReasoningEffort::Off,
            &cfg(),
        );
        assert_eq!(body["messages"][0]["content"], "ok");
    }

    #[test]
    fn build_body_strips_internal_message_fields() {
        let mut user_msg = user("hi");
        user_msg.reasoning_content = Some("internal reasoning".into());
        user_msg.tool_metadata = Some(json!({"summary": "internal display metadata"}));
        user_msg.is_error = true;

        let assistant = Message {
            role: Role::Assistant,
            content: None,
            reasoning_content: Some("prior thinking".into()),
            reasoning_details: None,
            tool_calls: Some(vec![ToolCall::new(
                "id".into(),
                FunctionCall {
                    name: "f".into(),
                    arguments: "{}".into(),
                },
            )]),
            tool_call_id: None,
            is_error: true,
            tool_metadata: Some(json!({"summary": "internal assistant metadata"})),
        };

        let mut tool = tool_msg("id", "ok");
        tool.tool_metadata = Some(json!({"summary": "internal tool metadata"}));
        tool.is_error = true;

        let body = build_body(
            &[user_msg, assistant, tool],
            &[],
            &target("m"),
            ReasoningEffort::Off,
            &cfg(),
        );

        assert_eq!(message_keys(&body["messages"][0]), ["content", "role"]);
        assert_eq!(message_keys(&body["messages"][1]), ["role", "tool_calls"]);
        assert_eq!(
            message_keys(&body["messages"][2]),
            ["content", "role", "tool_call_id"]
        );
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "id");
        assert_eq!(body["messages"][2]["tool_call_id"], "id");
        assert_eq!(body["messages"][2]["content"], "ok");
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
            tool_metadata: None,
        };
        let body = build_body(&[m], &[], &target("m"), ReasoningEffort::Off, &cfg());
        let args = &body["messages"][0]["tool_calls"][0]["function"]["arguments"];
        assert_eq!(args, "{}");
    }

    #[test]
    fn build_body_preserves_tool_call_history_without_reasoning_fields() {
        let assistant = Message {
            role: Role::Assistant,
            content: None,
            reasoning_content: Some("prior thinking".into()),

            reasoning_details: None,
            tool_calls: Some(vec![ToolCall::new(
                "id".into(),
                FunctionCall {
                    name: "f".into(),
                    arguments: "{}".into(),
                },
            )]),
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        };
        let body = build_body(
            &[
                user("before compaction"),
                assistant,
                tool_msg("id", "tool output"),
                user("continue with thinking on"),
            ],
            &[],
            &target("m"),
            ReasoningEffort::Low,
            &cfg(),
        );
        assert!(body["messages"][1].get("reasoning_content").is_none());
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "id");
        assert_eq!(body["messages"][2]["tool_call_id"], "id");
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
            tool_metadata: None,
        };
        let body = build_body(&[m], &[], &target("m"), ReasoningEffort::Off, &cfg());
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
        let body = build_body(
            &[user("hi")],
            &tools,
            &target("m"),
            ReasoningEffort::Off,
            &cfg(),
        );
        assert!(body["tools"].is_array());
        assert_eq!(body["tools"][0]["function"]["name"], "f");
    }

    #[test]
    fn build_body_omits_tools_when_empty() {
        let body = build_body(
            &[user("hi")],
            &[],
            &target("m"),
            ReasoningEffort::Off,
            &cfg(),
        );
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
        let body = build_body(&[user("hi")], &[], &target("m"), ReasoningEffort::Off, &c);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["min_p"], 0.05);
        assert_eq!(body["repeat_penalty"], 1.1);
    }

    #[test]
    fn build_body_omits_thinking_fields_when_effort_off() {
        let body = build_body(
            &[user("hi")],
            &[],
            &target("m"),
            ReasoningEffort::Off,
            &cfg(),
        );
        assert!(body.get("chat_template_kwargs").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn build_body_explicitly_disables_supported_reasoning() {
        for supports_reasoning in [None, Some(false), Some(true)] {
            let config = ModelConfig {
                supports_reasoning,
                ..cfg()
            };
            let body = build_body(
                &[user("hi")],
                &[],
                &target("custom-model"),
                ReasoningEffort::Off,
                &config,
            );
            if supports_reasoning == Some(true) {
                assert_eq!(body["reasoning_effort"], "none");
            } else {
                assert!(body.get("reasoning_effort").is_none());
            }
            assert!(body.get("chat_template_kwargs").is_none());
        }
    }

    #[test]
    fn build_body_sets_reasoning_effort_when_effort_set() {
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Custom("persistent".into()),
        ] {
            let label = effort.label().to_string();
            let body = build_body(&[user("hi")], &[], &target("m"), effort, &cfg());
            assert_eq!(body["reasoning_effort"], label);
            assert!(body.get("chat_template_kwargs").is_none());
        }
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
        assert_eq!(r.usage.context_tokens, Some(15));
        assert_eq!(r.usage.prompt_tokens, Some(7));
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
        assert_eq!(state.usage.context_tokens, Some(10));
        assert_eq!(state.usage.prompt_tokens, Some(5));
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
                if let ProviderStreamEvent::TextDelta(t) = d {
                    got.push(t.into())
                }
            },
        );
        assert_eq!(state.content, "hi");
        assert_eq!(got, vec!["hi"]);
    }

    #[test]
    fn sse_coalesced_reasoning_precedes_text_and_tool_calls() {
        for reasoning_field in ["reasoning_content", "reasoning"] {
            let mut state = StreamState::default();
            let mut got = Vec::new();
            apply_sse_event(
                &mut state,
                &json!({"choices": [{"delta": {
                    "content": "The",
                    reasoning_field: " lighthouse story.",
                    "tool_calls": [{
                        "index": 0, "id": "c1",
                        "function": {"name": "read_file", "arguments": "{}"}
                    }]
                }}]}),
                &mut |event| match event {
                    ProviderStreamEvent::Reasoning(ReasoningStreamEvent::Delta {
                        delta, ..
                    }) => {
                        got.push(("thinking", delta.to_string()));
                    }
                    ProviderStreamEvent::TextDelta(text) => got.push(("text", text.to_string())),
                    ProviderStreamEvent::ToolCall(ToolCallStreamEvent::Started { .. }) => {
                        got.push(("tool", String::new()));
                    }
                    _ => {}
                },
            );
            assert_eq!(
                got,
                vec![
                    ("thinking", " lighthouse story.".into()),
                    ("text", "The".into()),
                    ("tool", String::new()),
                ],
                "{reasoning_field}"
            );
            assert_eq!(state.reasoning, " lighthouse story.");
            assert_eq!(state.content, "The");
        }
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
                if let ProviderStreamEvent::Reasoning(ReasoningStreamEvent::Delta {
                    delta, ..
                }) = d
                {
                    got.push(delta.into())
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
    fn sse_tool_call_streams_lifecycle_events() {
        let mut state = StreamState::default();
        let mut got = Vec::new();
        let mut on_delta = |event: ProviderStreamEvent<'_>| {
            if let ProviderStreamEvent::ToolCall(event) = event {
                got.push(match event {
                    ToolCallStreamEvent::Started {
                        stream_id,
                        call_id,
                        tool_name,
                    } => format!("start:{stream_id}:{call_id:?}:{tool_name:?}"),
                    ToolCallStreamEvent::ArgsDelta {
                        stream_id,
                        call_id,
                        tool_name,
                        delta,
                    } => format!("delta:{stream_id}:{call_id:?}:{tool_name:?}:{delta}"),
                    ToolCallStreamEvent::Finished {
                        stream_id,
                        call_id,
                        tool_name,
                        arguments,
                    } => format!("finish:{stream_id}:{call_id}:{tool_name}:{arguments}"),
                });
            }
        };

        apply_sse_event(
            &mut state,
            &json!({
                "choices":[{"delta":{"tool_calls":[
                    {"index":0, "id":"c1", "function":{"name":"bash", "arguments":"{\"command\":"}}
                ]}}]
            }),
            &mut on_delta,
        );
        apply_sse_event(
            &mut state,
            &json!({
                "choices":[{"delta":{"tool_calls":[
                    {"index":0, "function":{"arguments":"\"echo hi\"}"}}
                ]}}]
            }),
            &mut on_delta,
        );
        apply_sse_event(
            &mut state,
            &json!({"choices":[{"finish_reason":"tool_calls"}]}),
            &mut on_delta,
        );

        assert_eq!(
            got,
            vec![
                "start:0:Some(\"c1\"):Some(\"bash\")",
                r#"delta:0:Some("c1"):Some("bash"):{"command":"#,
                r#"delta:0:Some("c1"):Some("bash"):"echo hi"}"#,
                "finish:0:c1:bash:{\"command\":\"echo hi\"}",
            ]
        );
    }

    #[test]
    fn sse_tool_call_metadata_updates_after_start_emit_started_upsert() {
        let mut state = StreamState::default();
        let mut got = Vec::new();
        let mut on_delta = |event: ProviderStreamEvent<'_>| {
            if let ProviderStreamEvent::ToolCall(ToolCallStreamEvent::Started {
                stream_id,
                call_id,
                tool_name,
            }) = event
            {
                got.push(format!("start:{stream_id}:{call_id:?}:{tool_name:?}"));
            }
        };

        apply_sse_event(
            &mut state,
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0}]}}]}),
            &mut on_delta,
        );
        apply_sse_event(
            &mut state,
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0, "function":{"name":"bash"}}]}}]}),
            &mut on_delta,
        );
        apply_sse_event(
            &mut state,
            &json!({"choices":[{"delta":{"tool_calls":[{"index":0, "id":"c1"}]}}]}),
            &mut on_delta,
        );

        assert_eq!(
            got,
            vec![
                "start:0:None:None",
                "start:0:None:Some(\"bash\")",
                "start:0:Some(\"c1\"):Some(\"bash\")",
            ]
        );
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

    #[test]
    fn incomplete_tool_arguments_never_emit_finished_or_finalize() {
        let events = [
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "c1", "function": {
                    "name": "f", "arguments": "{\"sensitive-fixture\":"
                }
            }]}}]}),
            json!({"choices": [{"finish_reason": "tool_calls"}]}),
        ];
        let mut finishes = 0;
        let error = parse_stream_events(&events, &mut |event| {
            if matches!(
                event,
                ProviderStreamEvent::ToolCall(ToolCallStreamEvent::Finished { .. })
            ) {
                finishes += 1;
            }
        })
        .err()
        .expect("stream must fail")
        .to_string();
        assert_eq!(finishes, 0);
        assert!(error.contains("tool-call arguments"));
        assert!(!error.contains("sensitive-fixture"));
    }

    #[test]
    fn preserves_finish_reasons_in_batch_and_streaming_responses() {
        for reason in [
            "stop",
            "length",
            "tool_calls",
            "content_filter",
            "provider_specific",
        ] {
            let batch = parse_response(&json!({"choices": [{
                "message": {"reasoning_content": "thinking"}, "finish_reason": reason
            }]}))
            .unwrap();
            assert_eq!(batch.finish_reason.as_deref(), Some(reason));
            let events = [
                json!({"choices": [{"delta": {"reasoning_content": "thinking"}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": reason}]}),
                json!({"choices": [], "usage": {"completion_tokens": 10}}),
            ];
            let streamed = parse_stream_events(&events, &mut |_| {}).unwrap();
            assert_eq!(streamed.finish_reason.as_deref(), Some(reason));
            assert_eq!(streamed.reasoning.as_deref(), Some("thinking"));
            assert_eq!(streamed.usage.completion_tokens, Some(10));
        }
    }

    #[test]
    fn finish_reason_does_not_hide_upstream_error() {
        let events = [
            json!({"choices": [{"delta": {"content": "partial"}, "finish_reason": "stop"}]}),
            json!({"error": {"message": "sensitive-fixture"}}),
        ];
        let error = parse_stream_events(&events, &mut |_| {})
            .err()
            .expect("stream must fail")
            .to_string();
        assert!(error.contains("upstream error event"));
        assert!(!error.contains("sensitive-fixture"));
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
