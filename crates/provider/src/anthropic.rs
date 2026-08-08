use crate::sse;
use crate::{
    collect_indexed_tool_calls, non_empty, non_empty_blocks, parse_claude_model_version,
    tool::tool_result_attachment, CacheConfig, CancellationToken, ClaudeModelFamily,
    CompletedReasoningPart, ModelConfig, ParsedResponse, ProviderError, ProviderStreamEvent,
    ReasoningStreamEvent, ToolCallStreamEvent, ToolDefinition,
};
use protocol::{
    FunctionCall, Message, ReasoningBlock, ReasoningEffort, ReasoningKind, Role, TokenUsage,
    ToolAttachment, ToolAttachmentModality, ToolCall,
};
use std::collections::{BTreeMap, HashMap};

fn cache_control_value(cache: &CacheConfig) -> serde_json::Value {
    if cache.ttl_long {
        serde_json::json!({"type": "ephemeral", "ttl": "1h"})
    } else {
        serde_json::json!({"type": "ephemeral"})
    }
}

fn stamp_cache_control(v: &mut serde_json::Value, cache: &CacheConfig) {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("cache_control".into(), cache_control_value(cache));
    }
}

/// Anthropic rejects requests with more than 4 `cache_control` markers.
fn count_cache_breakpoints(body: &serde_json::Value) -> usize {
    fn walk(v: &serde_json::Value, count: &mut usize) {
        match v {
            serde_json::Value::Object(m) => {
                if m.contains_key("cache_control") {
                    *count += 1;
                }
                for (_, child) in m {
                    walk(child, count);
                }
            }
            serde_json::Value::Array(a) => {
                for child in a {
                    walk(child, count);
                }
            }
            _ => {}
        }
    }
    let mut count = 0;
    walk(body, &mut count);
    count
}

fn supports_adaptive_thinking(model: &str) -> bool {
    let Some(version) = parse_claude_model_version(model) else {
        return false;
    };
    matches!(
        version.family,
        Some(ClaudeModelFamily::Opus | ClaudeModelFamily::Sonnet)
    ) && version.at_least(4, 6)
}

/// Default per-level thinking budgets. Mirrors pi-mono.
fn default_thinking_budgets() -> protocol::ThinkingBudgets {
    protocol::ThinkingBudgets {
        low: 2048,
        medium: 8192,
        high: 16384,
        max: 16384,
    }
}

/// Bump `max_tokens` by the thinking budget so content still has room
/// after thinking. If the cap is smaller than the budget, shrink the
/// budget to leave at least `min_output` tokens for content.
fn adjust_max_tokens_for_thinking(base: u32, budget: u32) -> (u32, u32) {
    let min_output = 1024;
    let max_tokens = base.saturating_add(budget);
    if max_tokens <= budget {
        let adjusted = max_tokens.saturating_sub(min_output);
        (max_tokens, adjusted)
    } else {
        (max_tokens, budget)
    }
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

fn sum_tokens(parts: impl IntoIterator<Item = Option<u32>>) -> Option<u32> {
    let mut total: Option<u32> = None;
    for part in parts.into_iter().flatten() {
        total = Some(total.unwrap_or(0).saturating_add(part));
    }
    total
}

fn context_tokens_from_usage(usage: &TokenUsage) -> Option<u32> {
    // Total input = input_tokens + cache_read_input_tokens + cache_creation_input_tokens.
    // Context window = total input + output_tokens.
    sum_tokens([
        usage.prompt_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.completion_tokens,
    ])
}

fn parse_usage(u: &serde_json::Value) -> TokenUsage {
    let mut usage = TokenUsage {
        prompt_tokens: u["input_tokens"].as_u64().map(|n| n as u32),
        completion_tokens: u["output_tokens"].as_u64().map(|n| n as u32),
        cache_read_tokens: u["cache_read_input_tokens"].as_u64().map(|n| n as u32),
        cache_write_tokens: parse_cache_write_tokens(u),
        reasoning_tokens: None,
        context_tokens: None,
    };
    usage.context_tokens = context_tokens_from_usage(&usage);
    usage
}

fn anthropic_image_source(url: &str) -> serde_json::Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((media_type, data)) = rest.split_once(";base64,") {
            return serde_json::json!({
                "type": "base64",
                "media_type": media_type,
                "data": data,
            });
        }
    }
    serde_json::json!({"type": "url", "url": url})
}

fn anthropic_content_blocks(content: Option<&protocol::Content>) -> Vec<serde_json::Value> {
    match content {
        Some(protocol::Content::Text(text)) => vec![serde_json::json!({
            "type": "text",
            "text": text,
        })],
        Some(protocol::Content::Parts(parts)) => parts
            .iter()
            .map(|part| match part {
                protocol::ContentPart::Text { text } => serde_json::json!({
                    "type": "text",
                    "text": text,
                }),
                protocol::ContentPart::ImageUrl { url, .. } => serde_json::json!({
                    "type": "image",
                    "source": anthropic_image_source(url),
                }),
            })
            .collect(),
        None => vec![serde_json::json!({"type": "text", "text": ""})],
    }
}

fn anthropic_file_attachment_block(attachment: ToolAttachment) -> serde_json::Value {
    let block_type = match attachment.modality {
        ToolAttachmentModality::Image => "image",
        ToolAttachmentModality::Pdf => "document",
    };
    serde_json::json!({
        "type": block_type,
        "source": anthropic_image_source(&attachment.data_url),
    })
}

fn anthropic_tool_result_content(m: &Message) -> serde_json::Value {
    let output = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
    let Some(attachment) = tool_result_attachment(m) else {
        return serde_json::Value::String(output.to_string());
    };
    serde_json::json!([
        {"type": "text", "text": output},
        anthropic_file_attachment_block(attachment),
    ])
}

pub fn build_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
    cache: &CacheConfig,
) -> serde_json::Value {
    let mut system_content: Option<String> = None;
    let mut content: Vec<serde_json::Value> = Vec::new();

    // Moving cache breakpoint: everything up through this user turn is
    // reused across in-turn assistant/tool round-trips.
    let mut last_user_idx: Option<usize> = None;

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
                    "content": anthropic_content_blocks(m.content.as_ref()),
                }));
                last_user_idx = Some(content.len() - 1);
            }
            Role::Assistant => {
                let mut message_content = Vec::new();
                // Thinking blocks (with signatures) must precede text and
                // tool_use blocks; the API rejects assistant turns that end
                // with a thinking block, but it requires the original
                // signed blocks be replayed when the turn ended with tool_use.
                if let Some(blocks) = &m.reasoning_details {
                    for block in blocks {
                        if block.provider == ReasoningBlock::ANTHROPIC {
                            message_content.push(block.data.clone());
                        }
                    }
                }
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
                let message = serde_json::json!({
                    "role": "assistant",
                    "content": message_content,
                });
                content.push(message);
            }
            Role::Tool => {
                content.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.as_deref().unwrap_or(""),
                        "content": anthropic_tool_result_content(m),
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

    // Stamp before body construction takes ownership of `content`.
    if cache.anthropic_markers {
        if let Some(idx) = last_user_idx {
            if let Some(blocks) = content
                .get_mut(idx)
                .and_then(|m| m.get_mut("content"))
                .and_then(|c| c.as_array_mut())
            {
                if let Some(last_block) = blocks.last_mut() {
                    stamp_cache_control(last_block, cache);
                }
            }
        }
    }

    let base_max = config.max_tokens.unwrap_or(4096);
    let mut max_tokens = base_max;

    let mut body = serde_json::json!({
        "model": model,
        "messages": content,
        "max_tokens": max_tokens,
    });

    if let Some(sys) = system_content {
        let mut sys_block = serde_json::json!({"type": "text", "text": sys});
        if cache.anthropic_markers {
            stamp_cache_control(&mut sys_block, cache);
        }
        body["system"] = serde_json::json!([sys_block]);
    }
    if !api_tools.is_empty() {
        let mut tools_arr = api_tools;
        if cache.anthropic_markers {
            if let Some(last) = tools_arr.last_mut() {
                stamp_cache_control(last, cache);
            }
        }
        body["tools"] = serde_json::json!(tools_arr);
    }

    debug_assert!(
        count_cache_breakpoints(&body) <= 4,
        "anthropic request exceeds 4 cache_control breakpoints"
    );
    if let Some(v) = config.temperature {
        body["temperature"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_p {
        body["top_p"] = serde_json::json!(v);
    }

    if effort != ReasoningEffort::Off {
        if supports_adaptive_thinking(model) {
            body["thinking"] = serde_json::json!({
                "type": "adaptive",
                "display": "summarized",
            });
            body["output_config"] = serde_json::json!({
                "effort": effort.label(),
            });
        } else {
            // Budget-based thinking for all non-adaptive models (Kimi, older
            // Claude, etc.). Mirrors pi-mono behaviour.
            let budgets = config
                .thinking_budgets
                .unwrap_or_else(default_thinking_budgets);
            let mut budget = budgets.for_effort(effort);
            (max_tokens, budget) = adjust_max_tokens_for_thinking(base_max, budget);
            body["max_tokens"] = serde_json::json!(max_tokens);
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": budget,
                "display": "summarized",
            });
        }
    }

    body
}

pub fn parse_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
    let mut content: Option<String> = None;
    let mut reasoning: Option<String> = None;
    let mut reasoning_parts: Vec<CompletedReasoningPart> = Vec::new();
    let mut reasoning_blocks: Vec<ReasoningBlock> = Vec::new();
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
                        if !text.is_empty() {
                            reasoning_parts.push(CompletedReasoningPart {
                                kind: ReasoningKind::Raw,
                                content: text.to_string(),
                            });
                        }
                    }
                    reasoning_blocks.push(ReasoningBlock {
                        provider: ReasoningBlock::ANTHROPIC.to_string(),
                        data: block.clone(),
                    });
                }
                Some("redacted_thinking") => {
                    reasoning_blocks.push(ReasoningBlock {
                        provider: ReasoningBlock::ANTHROPIC.to_string(),
                        data: block.clone(),
                    });
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
                    if !text.is_empty() {
                        reasoning_parts.push(CompletedReasoningPart {
                            kind: ReasoningKind::Summary,
                            content: text.to_string(),
                        });
                    }
                }
            }
        }
    }

    let usage = parse_usage(&data["usage"]);

    Ok(ParsedResponse {
        content,
        reasoning_parts,
        reasoning,
        reasoning_blocks: non_empty_blocks(reasoning_blocks),
        tool_calls,
        usage,
    })
}

/// Streaming accumulator for one thinking content block. Holds the verbatim
/// shape we will replay on the next request - text + signature for normal
/// thinking, opaque `data` for redacted_thinking.
#[derive(Default)]
struct ThinkingAccum {
    text: String,
    signature: Option<String>,
    /// Verbatim payload of a `redacted_thinking` block. When set, this block
    /// is replayed as `{"type":"redacted_thinking", "data": <payload>}`; text
    /// and signature are unused.
    redacted_data: Option<String>,
}

/// Accumulator for one streaming response. Mutated by `apply_sse_event`.
#[derive(Default)]
struct StreamState {
    content: String,
    reasoning: String,
    /// content block index -> verbatim thinking block, replayed on next turn.
    thinking_blocks: BTreeMap<usize, ThinkingAccum>,
    /// content block index -> (id, name, args-json)
    tool_calls: HashMap<usize, (String, String, String)>,
    usage: TokenUsage,
    saw_message_stop: bool,
}

impl StreamState {
    fn finalize(self) -> ParsedResponse {
        let reasoning_blocks: Vec<ReasoningBlock> = self
            .thinking_blocks
            .into_values()
            .map(|t| ReasoningBlock {
                provider: ReasoningBlock::ANTHROPIC.to_string(),
                data: match t.redacted_data {
                    Some(d) => serde_json::json!({"type": "redacted_thinking", "data": d}),
                    None => {
                        let mut obj = serde_json::json!({
                            "type": "thinking",
                            "thinking": t.text,
                        });
                        if let Some(sig) = t.signature {
                            obj["signature"] = serde_json::Value::String(sig);
                        }
                        obj
                    }
                },
            })
            .collect();
        ParsedResponse {
            content: non_empty(self.content),
            reasoning: non_empty(self.reasoning),
            reasoning_parts: Vec::new(),
            reasoning_blocks: non_empty_blocks(reasoning_blocks),
            tool_calls: collect_indexed_tool_calls(self.tool_calls),
            usage: self.usage,
        }
    }
}

fn finish_stream_state(state: StreamState) -> Result<ParsedResponse, ProviderError> {
    if !state.saw_message_stop {
        return Err(ProviderError::InvalidResponse(
            "stream ended without message_stop".into(),
        ));
    }
    Ok(state.finalize())
}

#[cfg_attr(not(any(test, feature = "fuzz")), allow(dead_code))]
pub fn parse_stream_events<'a>(
    events: impl IntoIterator<Item = &'a serde_json::Value>,
    on_delta: &mut dyn FnMut(ProviderStreamEvent),
) -> Result<ParsedResponse, ProviderError> {
    let mut state = StreamState::default();
    for ev in events {
        apply_sse_event(&mut state, ev, on_delta);
    }
    finish_stream_state(state)
}

/// Apply one SSE event to the accumulator. Pure (modulo `on_delta`).
fn apply_sse_event(
    state: &mut StreamState,
    ev: &serde_json::Value,
    on_delta: &mut dyn FnMut(ProviderStreamEvent),
) {
    let event_type = ev["type"].as_str().unwrap_or("");

    match event_type {
        "message_start" => {
            if let Some(u) = ev.get("message").and_then(|m| m.get("usage")) {
                state.usage = parse_usage(u);
            }
        }
        "content_block_start" => {
            if let Some(idx) = ev["index"].as_u64() {
                if let Some(cb) = ev.get("content_block") {
                    match cb["type"].as_str() {
                        Some("tool_use") => {
                            let id = cb["id"].as_str().unwrap_or_default().to_string();
                            let name = cb["name"].as_str().unwrap_or_default().to_string();
                            let stream_id = idx.to_string();
                            state
                                .tool_calls
                                .insert(idx as usize, (id, name, String::new()));
                            if let Some((id, name, _)) = state.tool_calls.get(&(idx as usize)) {
                                on_delta(ProviderStreamEvent::ToolCall(
                                    ToolCallStreamEvent::Started {
                                        stream_id: &stream_id,
                                        call_id: (!id.is_empty()).then_some(id.as_str()),
                                        tool_name: (!name.is_empty()).then_some(name.as_str()),
                                    },
                                ));
                            }
                        }
                        Some("thinking") => {
                            // Initial `thinking` field may already carry partial
                            // text; signature arrives via signature_delta.
                            let initial = cb["thinking"].as_str().unwrap_or("").to_string();
                            state.thinking_blocks.insert(
                                idx as usize,
                                ThinkingAccum {
                                    text: initial.clone(),
                                    ..Default::default()
                                },
                            );
                            let stream_id = idx.to_string();
                            on_delta(ProviderStreamEvent::Reasoning(
                                ReasoningStreamEvent::PartStarted {
                                    item_id: &stream_id,
                                    part_index: 0,
                                    kind: ReasoningKind::Raw,
                                },
                            ));
                            if !initial.is_empty() {
                                state.reasoning.push_str(&initial);
                                on_delta(ProviderStreamEvent::Reasoning(
                                    ReasoningStreamEvent::Delta {
                                        item_id: &stream_id,
                                        part_index: 0,
                                        kind: ReasoningKind::Raw,
                                        delta: &initial,
                                    },
                                ));
                            }
                        }
                        Some("redacted_thinking") => {
                            let data = cb["data"].as_str().unwrap_or("").to_string();
                            state.thinking_blocks.insert(
                                idx as usize,
                                ThinkingAccum {
                                    redacted_data: Some(data),
                                    ..Default::default()
                                },
                            );
                        }
                        _ => {}
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
                                on_delta(ProviderStreamEvent::TextDelta(text));
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta["thinking"].as_str() {
                            if !text.is_empty() {
                                state.reasoning.push_str(text);
                                if let Some(idx) = ev["index"].as_u64() {
                                    let entry =
                                        state.thinking_blocks.entry(idx as usize).or_default();
                                    entry.text.push_str(text);
                                }
                                let stream_id = ev["index"].as_u64().unwrap_or(0).to_string();
                                on_delta(ProviderStreamEvent::Reasoning(
                                    ReasoningStreamEvent::Delta {
                                        item_id: &stream_id,
                                        part_index: 0,
                                        kind: ReasoningKind::Raw,
                                        delta: text,
                                    },
                                ));
                            }
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(sig) = delta["signature"].as_str() {
                            if !sig.is_empty() {
                                if let Some(idx) = ev["index"].as_u64() {
                                    let entry =
                                        state.thinking_blocks.entry(idx as usize).or_default();
                                    match &mut entry.signature {
                                        Some(s) => s.push_str(sig),
                                        None => entry.signature = Some(sig.to_string()),
                                    }
                                }
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial_json) = delta["partial_json"].as_str() {
                            if !partial_json.is_empty() {
                                if let Some(idx) = ev["index"].as_u64() {
                                    if let Some(entry) = state.tool_calls.get_mut(&(idx as usize)) {
                                        let stream_id = idx.to_string();
                                        entry.2.push_str(partial_json);
                                        on_delta(ProviderStreamEvent::ToolCall(
                                            ToolCallStreamEvent::ArgsDelta {
                                                stream_id: &stream_id,
                                                call_id: (!entry.0.is_empty())
                                                    .then_some(entry.0.as_str()),
                                                tool_name: (!entry.1.is_empty())
                                                    .then_some(entry.1.as_str()),
                                                delta: partial_json,
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            if let Some(idx) = ev["index"].as_u64() {
                if state.thinking_blocks.contains_key(&(idx as usize)) {
                    let stream_id = idx.to_string();
                    on_delta(ProviderStreamEvent::Reasoning(
                        ReasoningStreamEvent::PartFinished {
                            item_id: &stream_id,
                            part_index: 0,
                            kind: ReasoningKind::Raw,
                            content: None,
                        },
                    ));
                }
            }
        }
        "message_delta" => {
            if let Some(u) = ev.get("usage") {
                state.usage.completion_tokens = u["output_tokens"].as_u64().map(|n| n as u32);
                if state.usage.prompt_tokens.is_none() {
                    state.usage.prompt_tokens = u["input_tokens"].as_u64().map(|n| n as u32);
                }
                state.usage.context_tokens = context_tokens_from_usage(&state.usage);
            }
        }
        "message_stop" => {
            for (idx, (call_id, name, args)) in &state.tool_calls {
                if call_id.is_empty() || name.is_empty() {
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
            state.saw_message_stop = true;
        }
        _ => {}
    }
}

pub async fn read_stream(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    on_delta: &(dyn Fn(ProviderStreamEvent) + Send + Sync),
) -> Result<ParsedResponse, ProviderError> {
    let mut state = StreamState::default();

    sse::read_events(resp, cancel, |ev| {
        apply_sse_event(&mut state, ev, &mut |d| on_delta(d));
    })
    .await?;

    finish_stream_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FunctionSchema;
    use protocol::{Content, ContentPart, FunctionCall, Message, Role, ToolCall};
    use serde_json::json;

    fn cfg() -> ModelConfig {
        ModelConfig::default()
    }

    fn message(role: Role, content: Option<Content>) -> Message {
        Message {
            role,
            content,
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        }
    }

    fn user(text: &str) -> Message {
        message(Role::User, Some(Content::Text(text.into())))
    }

    fn system(text: &str) -> Message {
        message(Role::System, Some(Content::Text(text.into())))
    }

    fn assistant_text(text: &str) -> Message {
        message(Role::Assistant, Some(Content::Text(text.into())))
    }

    fn assistant_calls(content: Option<&str>, calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content: content.map(|text| Content::Text(text.into())),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: Some(calls),
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        }
    }

    fn tool_msg(call_id: Option<&str>, output: &str) -> Message {
        Message {
            role: Role::Tool,
            content: Some(Content::Text(output.into())),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: None,
            tool_call_id: call_id.map(String::from),
            is_error: false,
            tool_metadata: None,
        }
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
                "type": "content_block_start",
                "index": 2,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "bash"},
            }),
            &mut on_delta,
        );
        apply_sse_event(
            &mut state,
            &json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"},
            }),
            &mut on_delta,
        );
        apply_sse_event(
            &mut state,
            &json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {"type": "input_json_delta", "partial_json": "\"echo hi\"}"},
            }),
            &mut on_delta,
        );
        apply_sse_event(&mut state, &json!({"type": "message_stop"}), &mut on_delta);

        assert_eq!(
            got,
            vec![
                "start:2:Some(\"toolu_1\"):Some(\"bash\")",
                r#"delta:2:Some("toolu_1"):Some("bash"):{"command":"#,
                r#"delta:2:Some("toolu_1"):Some("bash"):"echo hi"}"#,
                "finish:2:toolu_1:bash:{\"command\":\"echo hi\"}",
            ]
        );
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
    fn build_body_serializes_captured_tool_result_image() {
        let tool = Message::tool_with_metadata(
            "toolu_1".into(),
            "image file attached",
            false,
            Some(json!({
                "kind": "file_attachment",
                "modality": "image",
                "path": "/path/that/no/longer/exists.png",
                "mime": "image/png",
                "data_url": "data:image/png;base64,iVBORw0KGgppbWFnZS1ieXRlcw==",
                "label": "tiny.png"
            })),
        );
        let body = build_body(
            &[
                assistant_calls(
                    None,
                    vec![ToolCall::new(
                        "toolu_1".into(),
                        FunctionCall {
                            name: "read_file".into(),
                            arguments: "{}".into(),
                        },
                    )],
                ),
                tool,
            ],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        let result_content = &body["messages"][1]["content"][0]["content"];
        assert_eq!(result_content[0]["type"], "text");
        assert_eq!(result_content[1]["type"], "image");
        assert_eq!(result_content[1]["source"]["media_type"], "image/png");
        assert_eq!(result_content[1]["source"]["type"], "base64");
    }

    #[test]
    fn build_body_serializes_captured_tool_result_pdf() {
        let tool = Message::tool_with_metadata(
            "toolu_1".into(),
            "pdf file attached",
            false,
            Some(json!({
                "kind": "file_attachment",
                "modality": "pdf",
                "mime": "application/pdf",
                "data_url": "data:application/pdf;base64,JVBERi0xLjQ=",
            })),
        );
        let body = build_body(
            &[tool],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );

        let result_content = &body["messages"][0]["content"][0]["content"];
        assert_eq!(result_content[1]["type"], "document");
        assert_eq!(result_content[1]["source"]["media_type"], "application/pdf");
        assert_eq!(result_content[1]["source"]["data"], "JVBERi0xLjQ=");
    }

    #[test]
    fn build_body_joins_multiple_system_messages_with_double_newline() {
        let body = build_body(
            &[system("first"), system("second"), user("hi")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        assert_eq!(body["system"][0]["type"], "text");
        assert_eq!(body["system"][0]["text"], "first\n\nsecond");
    }

    #[test]
    fn build_body_omits_system_field_when_no_system_messages() {
        let body = build_body(
            &[user("hi")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        assert!(body.get("system").is_none());
    }

    // ---- cache_control ----

    fn cache_on() -> CacheConfig {
        CacheConfig {
            anthropic_markers: true,
            ttl_long: false,
            prompt_cache_key: None,
        }
    }

    fn cache_on_long() -> CacheConfig {
        CacheConfig {
            anthropic_markers: true,
            ttl_long: true,
            prompt_cache_key: None,
        }
    }

    #[test]
    fn cache_stamps_system_tools_and_last_user() {
        let tools = vec![
            ToolDefinition::new(FunctionSchema {
                name: "a".into(),
                description: "first tool".into(),
                parameters: json!({"type": "object"}),
            }),
            ToolDefinition::new(FunctionSchema {
                name: "b".into(),
                description: "second tool".into(),
                parameters: json!({"type": "object"}),
            }),
        ];
        let body = build_body(
            &[system("sys"), user("u1"), assistant_text("a1"), user("u2")],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        // System: last (only) block has the marker.
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        // Tools: only the LAST tool is marked.
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["tools"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        // Messages: only the last user message's last block is marked.
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        assert!(body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none());
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        // Exactly three markers; well under the 4-cap.
        assert_eq!(count_cache_breakpoints(&body), 3);
    }

    #[test]
    fn cache_disabled_emits_no_markers() {
        let tools = vec![ToolDefinition::new(FunctionSchema {
            name: "a".into(),
            description: "tool".into(),
            parameters: json!({"type": "object"}),
        })];
        let body = build_body(
            &[system("sys"), user("hi")],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        assert_eq!(count_cache_breakpoints(&body), 0);
    }

    #[test]
    fn cache_ttl_long_emits_1h() {
        let body = build_body(
            &[system("sys"), user("hi")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on_long(),
        );
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn build_body_is_deterministic() {
        // Cache hits depend on byte-stable prefixes. Re-running build_body
        // with identical inputs must produce identical JSON.
        let tools = vec![
            ToolDefinition::new(FunctionSchema {
                name: "alpha".into(),
                description: "first".into(),
                parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            }),
            ToolDefinition::new(FunctionSchema {
                name: "beta".into(),
                description: "second".into(),
                parameters: json!({"type": "object"}),
            }),
        ];
        let msgs = vec![system("sys"), user("u1"), assistant_text("a1"), user("u2")];
        let a = build_body(
            &msgs,
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        let b = build_body(
            &msgs,
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "build_body must be byte-deterministic for identical inputs"
        );
    }

    #[test]
    fn cache_marks_only_last_user_when_multiple_users() {
        let body = build_body(
            &[
                user("first"),
                assistant_text("ack"),
                user("second"),
                assistant_text("ack2"),
                user("third"),
            ],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        // No system, no tools - only the last user counts.
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        assert!(body["messages"][2]["content"][0]
            .get("cache_control")
            .is_none());
        assert_eq!(
            body["messages"][4]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(count_cache_breakpoints(&body), 1);
    }

    #[test]
    fn build_body_preserves_user_image_parts() {
        let msg = Message {
            role: Role::User,
            content: Some(Content::Parts(vec![
                ContentPart::Text {
                    text: "look".into(),
                },
                ContentPart::ImageUrl {
                    url: "data:image/png;base64,abc123".into(),
                    label: None,
                },
            ])),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        };
        let body = build_body(
            &[msg],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0], json!({"type": "text", "text": "look"}));
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "abc123");
    }

    #[test]
    fn build_body_emits_user_message_with_text_content() {
        let body = build_body(
            &[user("hello")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        let msg = &body["messages"][0];
        assert_eq!(msg["role"], "user");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], "hello");
        assert!(msg["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn thinking_replays_tool_turns_with_anthropic_reasoning_blocks() {
        let call = ToolCall::new(
            "call_1".into(),
            FunctionCall {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        );
        let assistant = Message::assistant_with_reasoning(
            None,
            Some("thought".into()),
            Some(vec![ReasoningBlock {
                provider: ReasoningBlock::ANTHROPIC.into(),
                data: json!({
                    "type": "thinking",
                    "thinking": "thought",
                    "signature": "sig",
                }),
            }]),
            Some(vec![call]),
        );
        let body = build_body(
            &[
                user("run it"),
                assistant,
                tool_msg(Some("call_1"), "done"),
                user("next"),
            ],
            &[],
            "claude-sonnet-4-5",
            ReasoningEffort::Low,
            &cfg(),
            &CacheConfig::default(),
        );

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "thinking");
        assert_eq!(messages[1]["content"][1]["type"], "tool_use");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn build_body_assistant_text_emits_text_block() {
        let body = build_body(
            &[assistant_text("yo")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
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
            &CacheConfig::default(),
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
            &CacheConfig::default(),
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
            &CacheConfig::default(),
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
            &CacheConfig::default(),
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
        let body = build_body(
            &[user("hi")],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
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
        let body = build_body(
            &[user("hi")],
            &[],
            "m",
            ReasoningEffort::Off,
            &c,
            &CacheConfig::default(),
        );
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
            &CacheConfig::default(),
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(
            body["output_config"]["effort"],
            ReasoningEffort::High.label()
        );
    }

    #[test]
    fn build_body_emits_budget_thinking_for_non_adaptive_models_with_effort() {
        let body = build_body(
            &[user("hi")],
            &[],
            "claude-haiku-4-5",
            ReasoningEffort::High,
            &cfg(),
            &CacheConfig::default(),
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert!(body["thinking"]["budget_tokens"].as_u64().unwrap() > 0);
    }

    #[test]
    fn build_body_bumps_max_tokens_for_budget_thinking() {
        let body = build_body(
            &[user("hi")],
            &[],
            "kimi-for-coding",
            ReasoningEffort::High,
            &cfg(),
            &CacheConfig::default(),
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 16384);
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["max_tokens"], 4096 + 16384);
    }

    #[test]
    fn build_body_omits_thinking_when_effort_is_off() {
        let body = build_body(
            &[user("hi")],
            &[],
            "claude-sonnet-4-6",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn build_body_sets_default_max_tokens() {
        let body = build_body(
            &[user("hi")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
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
        assert_eq!(
            r.reasoning_parts,
            vec![
                CompletedReasoningPart {
                    kind: ReasoningKind::Raw,
                    content: "ponder".into(),
                },
                CompletedReasoningPart {
                    kind: ReasoningKind::Raw,
                    content: "ing".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_response_uses_top_level_thinking_when_content_blocks_lack_it() {
        let v = json!({
            "content": [{"type": "text", "text": "hi"}],
            "thinking": [{"text": "summary one"}, {"text": "summary two"}],
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("summary onesummary two"));
        assert_eq!(
            r.reasoning_parts,
            vec![
                CompletedReasoningPart {
                    kind: ReasoningKind::Summary,
                    content: "summary one".into(),
                },
                CompletedReasoningPart {
                    kind: ReasoningKind::Summary,
                    content: "summary two".into(),
                },
            ]
        );
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
        // input is serialized via Value::to_string - no spaces in JSON.
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
        assert_eq!(r.usage.context_tokens, Some(20));
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
        assert_eq!(state.usage.context_tokens, Some(22));
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
                if let ProviderStreamEvent::TextDelta(t) = d {
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
        // No input_tokens in this event, so context is just completion.
        assert_eq!(state.usage.context_tokens, Some(12));
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
        // context_tokens = 99 (prompt) + 1 (completion); no cache fields set.
        assert_eq!(state.usage.context_tokens, Some(100));

        let mut state2 = StreamState::default();
        step(
            &mut state2,
            json!({
                "type": "message_delta",
                "usage": {"input_tokens": 5, "output_tokens": 1}
            }),
        );
        assert_eq!(state2.usage.prompt_tokens, Some(5));
        assert_eq!(state2.usage.context_tokens, Some(6));
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
        assert!(r.reasoning_blocks.is_none());
    }

    // ---- reasoning round-trip ----

    #[test]
    fn parse_response_captures_thinking_blocks_with_signature() {
        let v = json!({"content": [
            {"type": "thinking", "thinking": "ponder", "signature": "sig-1"},
            {"type": "text", "text": "answer"},
        ]});
        let r = parse_response(&v).unwrap();
        let blocks = r.reasoning_blocks.expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].provider, "anthropic");
        assert_eq!(blocks[0].data["type"], "thinking");
        assert_eq!(blocks[0].data["thinking"], "ponder");
        assert_eq!(blocks[0].data["signature"], "sig-1");
    }

    #[test]
    fn parse_response_captures_redacted_thinking_verbatim() {
        let v = json!({"content": [
            {"type": "redacted_thinking", "data": "opaque-payload"},
            {"type": "text", "text": "answer"},
        ]});
        let r = parse_response(&v).unwrap();
        let blocks = r.reasoning_blocks.expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data["type"], "redacted_thinking");
        assert_eq!(blocks[0].data["data"], "opaque-payload");
    }

    #[test]
    fn sse_thinking_block_captures_signature_via_signature_delta() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "step "}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "one"}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig-"}
            }),
        );
        step(
            &mut state,
            json!({
                "type": "content_block_delta", "index": 0,
                "delta": {"type": "signature_delta", "signature": "xyz"}
            }),
        );
        let r = state.finalize();
        let blocks = r.reasoning_blocks.expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].data["type"], "thinking");
        assert_eq!(blocks[0].data["thinking"], "step one");
        assert_eq!(blocks[0].data["signature"], "sig-xyz");
    }

    #[test]
    fn sse_redacted_thinking_block_finalizes_to_verbatim_data() {
        let mut state = StreamState::default();
        step(
            &mut state,
            json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "redacted_thinking", "data": "ciphertext"}
            }),
        );
        let r = state.finalize();
        let blocks = r.reasoning_blocks.expect("blocks");
        assert_eq!(blocks[0].data["type"], "redacted_thinking");
        assert_eq!(blocks[0].data["data"], "ciphertext");
    }

    #[test]
    fn build_body_prepends_anthropic_reasoning_blocks_before_text_and_tool_use() {
        use protocol::{Content, Message, ReasoningBlock, Role};
        let m = Message {
            role: Role::Assistant,
            content: Some(Content::Text("answer".into())),
            reasoning_content: None,
            reasoning_details: Some(vec![ReasoningBlock {
                provider: ReasoningBlock::ANTHROPIC.to_string(),
                data: json!({"type": "thinking", "thinking": "why", "signature": "s"}),
            }]),
            tool_calls: Some(vec![ToolCall::new(
                "tid".into(),
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
            &[m],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "s");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool_use");
    }

    #[test]
    fn build_body_skips_reasoning_blocks_from_other_providers() {
        use protocol::{Content, Message, ReasoningBlock, Role};
        let m = Message {
            role: Role::Assistant,
            content: Some(Content::Text("answer".into())),
            reasoning_content: None,
            reasoning_details: Some(vec![ReasoningBlock {
                provider: ReasoningBlock::OPENAI_RESPONSES.to_string(),
                data: json!({"type": "reasoning", "id": "x"}),
            }]),
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        };
        let body = build_body(
            &[m],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &CacheConfig::default(),
        );
        let content = &body["messages"][0]["content"];
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn build_body_preserves_tool_turn_when_thinking_is_enabled_later() {
        let messages = vec![
            user("inspect"),
            assistant_calls(
                None,
                vec![ToolCall::new(
                    "tc1".into(),
                    FunctionCall {
                        name: "read_file".into(),
                        arguments: json!({"path":"a"}).to_string(),
                    },
                )],
            ),
            tool_msg(Some("tc1"), "file contents"),
        ];
        let body = build_body(
            &messages,
            &[],
            "m",
            ReasoningEffort::Low,
            &cfg(),
            &CacheConfig::default(),
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    }

    // ── Cache stability regression tests ─────────────────────────────────
    //
    // These pin the invariants the rest of the codebase relies on for
    // Anthropic prompt-cache reuse. Each test names the scenario, what
    // SHOULD reuse the cache (Stable), and what is expected to invalidate
    // (Invalidated). Drifts here mean cache misses in production.

    fn two_tools() -> Vec<ToolDefinition> {
        vec![
            ToolDefinition::new(FunctionSchema {
                name: "alpha".into(),
                description: "first tool".into(),
                parameters: json!({"type": "object"}),
            }),
            ToolDefinition::new(FunctionSchema {
                name: "beta".into(),
                description: "second tool".into(),
                parameters: json!({"type": "object"}),
            }),
        ]
    }

    /// Strip the moving `cache_control` marker from every block. Anthropic
    /// keys the cache on (model, tools, system, messages-up-to-marker);
    /// the marker itself is the breakpoint, not part of the prefix. Two
    /// requests with identical fields modulo marker placement should hit
    /// the same cache slot.
    fn without_markers(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, val) in map {
                    if k == "cache_control" {
                        continue;
                    }
                    out.insert(k.clone(), without_markers(val));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(without_markers).collect())
            }
            other => other.clone(),
        }
    }

    /// Stable: a plain follow-up turn. Turn N+1's request appends an
    /// assistant message and a new user message; everything else stays
    /// byte-identical so the moving cache breakpoint on turn N's last
    /// user message still anchors a hit.
    #[test]
    fn cache_prefix_stable_across_consecutive_turns() {
        let tools = two_tools();
        let turn_n = build_body(
            &[system("sys"), user("u1")],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        let turn_n_plus_1 = build_body(
            &[system("sys"), user("u1"), assistant_text("a1"), user("u2")],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );

        // System and tools must be byte-identical: the cached prefix at
        // the system breakpoint depends on both.
        assert_eq!(
            without_markers(&turn_n["system"]),
            without_markers(&turn_n_plus_1["system"]),
            "system bytes drifted between consecutive turns",
        );
        assert_eq!(
            without_markers(&turn_n["tools"]),
            without_markers(&turn_n_plus_1["tools"]),
            "tools bytes drifted between consecutive turns",
        );

        // The first user message must be byte-identical (after stripping
        // the marker on turn N): turn N+1 must reuse the prefix that
        // turn N cached.
        assert_eq!(
            without_markers(&turn_n["messages"][0]),
            without_markers(&turn_n_plus_1["messages"][0]),
            "first user message drifted; cache prefix breaks",
        );
    }

    /// Stable: mode-change synthetic user note appends without disturbing
    /// the system block. The base prompt is mode-agnostic, so flipping
    /// modes only adds a trailing user note - system bytes match.
    #[test]
    fn cache_stable_when_mode_change_appends_user_note() {
        let tools = two_tools();
        let before = build_body(
            &[system("sys"), user("u1")],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        let after = build_body(
            &[
                system("sys"),
                user("u1"),
                assistant_text("a1"),
                user("[smelt:mode] now in plan mode."),
                user("u2"),
            ],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        assert_eq!(
            without_markers(&before["system"]),
            without_markers(&after["system"]),
        );
        assert_eq!(
            without_markers(&before["tools"]),
            without_markers(&after["tools"]),
        );
    }

    /// Invalidated (expected): editing AGENTS.md / `/reload` produces a
    /// different system prompt. The bytes diverge - by design.
    #[test]
    fn cache_invalidates_when_system_prompt_changes() {
        let before = build_body(
            &[system("sys v1"), user("u")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        let after = build_body(
            &[system("sys v2"), user("u")],
            &[],
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        assert_ne!(
            without_markers(&before["system"]),
            without_markers(&after["system"]),
        );
    }

    /// Invalidated (expected): a new plugin tool joins the registry. The
    /// tools array gains an entry; the cached prefix at the system or
    /// tool breakpoint is gone.
    #[test]
    fn cache_invalidates_when_tools_list_grows() {
        let before = build_body(
            &[system("sys"), user("u")],
            &two_tools(),
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        let mut more = two_tools();
        more.push(ToolDefinition::new(FunctionSchema {
            name: "gamma".into(),
            description: "third".into(),
            parameters: json!({"type": "object"}),
        }));
        let after = build_body(
            &[system("sys"), user("u")],
            &more,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        assert_ne!(
            without_markers(&before["tools"]),
            without_markers(&after["tools"]),
        );
    }

    /// Invalidated (expected): re-ordering tools rewrites the prefix.
    /// `agent.rs` sorts tools by name precisely so this doesn't happen
    /// in practice; this test pins the dependency on that sort.
    #[test]
    fn cache_invalidates_when_tools_reorder() {
        let mut a = two_tools();
        let mut b = two_tools();
        b.reverse();
        let before = build_body(
            &[user("u")],
            &a,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        let after = build_body(
            &[user("u")],
            &b,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        // Sanity: helpers above produce two distinct tools.
        a.sort_by(|x, y| x.function.name.cmp(&y.function.name));
        b.sort_by(|x, y| x.function.name.cmp(&y.function.name));
        assert_eq!(a.len(), b.len());
        assert_ne!(
            without_markers(&before["tools"]),
            without_markers(&after["tools"]),
            "reordering tools must perturb the prefix (agent.rs sorts to prevent this)",
        );
    }

    /// Stable: sampling params (temperature, top_p) sit outside the cached
    /// prefix. Tweaking them must not perturb `system`, `tools`, or
    /// `messages` bytes.
    #[test]
    fn cache_stable_when_temperature_changes() {
        let mut cfg_a = cfg();
        cfg_a.temperature = Some(0.2);
        let mut cfg_b = cfg();
        cfg_b.temperature = Some(0.9);
        let a = build_body(
            &[system("sys"), user("u")],
            &two_tools(),
            "m",
            ReasoningEffort::Off,
            &cfg_a,
            &cache_on(),
        );
        let b = build_body(
            &[system("sys"), user("u")],
            &two_tools(),
            "m",
            ReasoningEffort::Off,
            &cfg_b,
            &cache_on(),
        );
        assert_eq!(without_markers(&a["system"]), without_markers(&b["system"]));
        assert_eq!(without_markers(&a["tools"]), without_markers(&b["tools"]));
        assert_eq!(
            without_markers(&a["messages"]),
            without_markers(&b["messages"]),
        );
    }

    /// Stable: an EngineAsk inheriting the session sends the SAME system,
    /// tools, and message prefix as the main turn - only the trailing
    /// instruction differs. The cached prefix up to the last main-turn
    /// user message survives.
    #[test]
    fn cache_inherit_session_reuses_main_turn_prefix() {
        let tools = two_tools();
        let main_turn = build_body(
            &[system("sys"), user("u1"), assistant_text("a1"), user("u2")],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        let inherited = build_body(
            &[
                system("sys"),
                user("u1"),
                assistant_text("a1"),
                user("u2"),
                assistant_text("a2"),
                user("Summarize the conversation above as a Markdown document."),
            ],
            &tools,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        // Identical prefix from system through the original last user
        // message. Only the trailing summary instruction is fresh.
        assert_eq!(
            without_markers(&main_turn["system"]),
            without_markers(&inherited["system"]),
        );
        assert_eq!(
            without_markers(&main_turn["tools"]),
            without_markers(&inherited["tools"]),
        );
        for i in 0..main_turn["messages"].as_array().unwrap().len() {
            assert_eq!(
                without_markers(&main_turn["messages"][i]),
                without_markers(&inherited["messages"][i]),
                "inherited request must preserve main-turn message {i}",
            );
        }
    }

    /// Stable: tools registered in any order must reach the provider
    /// sorted by name. `agent.rs` and `spawn_engine_ask` both call
    /// `sort_tools_for_cache_stability` before `provider.chat`; this
    /// test pins that helper. A regression that drops the sort would
    /// have the provider see different prefix bytes every time plugin
    /// registration order shifts.
    #[test]
    fn cache_tools_arrive_sorted_by_name_regardless_of_input_order() {
        fn tool(name: &str) -> ToolDefinition {
            ToolDefinition::new(FunctionSchema {
                name: name.into(),
                description: format!("{name} tool"),
                parameters: json!({"type": "object"}),
            })
        }
        let mut shuffled_a = vec![tool("zed"), tool("alpha"), tool("midway")];
        let mut shuffled_b = vec![tool("midway"), tool("zed"), tool("alpha")];
        crate::sort_tools_for_cache_stability(&mut shuffled_a);
        crate::sort_tools_for_cache_stability(&mut shuffled_b);

        // Both orderings collapse to the same body.
        let body_a = build_body(
            &[user("u")],
            &shuffled_a,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        let body_b = build_body(
            &[user("u")],
            &shuffled_b,
            "m",
            ReasoningEffort::Off,
            &cfg(),
            &cache_on(),
        );
        assert_eq!(
            without_markers(&body_a["tools"]),
            without_markers(&body_b["tools"]),
            "sort_tools_for_cache_stability must produce identical output regardless of input order",
        );

        let tool_names: Vec<&str> = body_a["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            tool_names,
            vec!["alpha", "midway", "zed"],
            "tools field must reach the provider in alphabetical order",
        );
    }
}
