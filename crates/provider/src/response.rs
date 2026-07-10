use protocol::{FunctionCall, ReasoningBlock, ReasoningKind, TokenUsage, ToolCall};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedReasoningPart {
    pub kind: ReasoningKind,
    pub content: String,
}

/// Provider chat response normalized across wire APIs.
#[derive(Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub reasoning_parts: Vec<CompletedReasoningPart>,
    pub reasoning_details: Option<Vec<ReasoningBlock>>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub tokens_per_sec: Option<f64>,
    pub metadata: ChatResponseMetadata,
}

#[derive(Clone, Default)]
pub struct ChatResponseMetadata {
    pub codex_turn_state: Option<String>,
}

impl ChatResponse {
    pub fn from_parsed(parsed: ParsedResponse, tokens_per_sec: Option<f64>) -> Self {
        Self::from_parsed_with_metadata(parsed, tokens_per_sec, ChatResponseMetadata::default())
    }

    pub fn from_parsed_with_metadata(
        parsed: ParsedResponse,
        tokens_per_sec: Option<f64>,
        metadata: ChatResponseMetadata,
    ) -> Self {
        Self {
            content: parsed.content,
            reasoning_content: parsed.reasoning,
            reasoning_parts: parsed.reasoning_parts,
            reasoning_details: parsed.reasoning_blocks,
            tool_calls: parsed.tool_calls,
            usage: parsed.usage,
            tokens_per_sec,
            metadata,
        }
    }
}

/// Internal parsed fields from an API response.
pub struct ParsedResponse {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_parts: Vec<CompletedReasoningPart>,
    /// Provider-shaped reasoning blocks to round-trip on the next request.
    pub reasoning_blocks: Option<Vec<ReasoningBlock>>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
}

pub fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn non_empty_blocks(v: Vec<ReasoningBlock>) -> Option<Vec<ReasoningBlock>> {
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

pub fn collect_indexed_tool_calls(map: HashMap<usize, (String, String, String)>) -> Vec<ToolCall> {
    let mut vec: Vec<(usize, ToolCall)> = map
        .into_iter()
        .map(|(idx, (id, name, args))| {
            (
                idx,
                ToolCall::new(
                    id,
                    FunctionCall {
                        name,
                        arguments: args,
                    },
                ),
            )
        })
        .collect();
    vec.sort_by_key(|(idx, _)| *idx);
    vec.into_iter().map(|(_, tc)| tc).collect()
}

/// Ensure `tool_calls[].function.arguments` is valid JSON; some models emit malformed strings.
pub fn sanitize_tool_call_arguments(obj: &mut serde_json::Map<String, serde_json::Value>) {
    if let Some(tcs) = obj.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
        for tc in tcs {
            if let Some(args) = tc.get_mut("function").and_then(|f| f.get_mut("arguments")) {
                if let Some(s) = args.as_str() {
                    if serde_json::from_str::<serde_json::Value>(s).is_err() {
                        *args = serde_json::json!("{}");
                    }
                }
            }
        }
    }
}
