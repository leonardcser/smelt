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
    /// Raw chat-completions finish reason, if supplied by the provider.
    pub finish_reason: Option<String>,
}

impl ChatResponse {
    pub fn from_parsed(parsed: ParsedResponse, tokens_per_sec: Option<f64>) -> Self {
        Self::from_parsed_with_metadata(parsed, tokens_per_sec, ChatResponseMetadata::default())
    }

    pub fn from_parsed_with_metadata(
        parsed: ParsedResponse,
        tokens_per_sec: Option<f64>,
        mut metadata: ChatResponseMetadata,
    ) -> Self {
        metadata.finish_reason = parsed.finish_reason;
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
    /// Raw chat-completions finish reason, if supplied by the provider.
    pub finish_reason: Option<String>,
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_parts: Vec<CompletedReasoningPart>,
    /// Provider-shaped reasoning blocks to round-trip on the next request.
    pub reasoning_blocks: Option<Vec<ReasoningBlock>>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
}

impl ParsedResponse {
    pub fn malformed(&self, issue: impl Into<String>) -> crate::ProviderError {
        crate::ProviderError::MalformedResponse {
            issue: issue.into(),
            finish_reason: self.finish_reason.clone(),
            usage: self.usage.clone(),
        }
    }

    /// Validate the entire batch before any call can reach a tool executor.
    pub fn validate(&self) -> Result<(), crate::ProviderError> {
        let mut ids = std::collections::HashSet::new();
        for (index, call) in self.tool_calls.iter().enumerate() {
            let issue = if call.id.is_empty() || call.function.name.is_empty() {
                Some("incomplete tool-call metadata")
            } else if !ids.insert(&call.id) {
                Some("duplicate tool-call id")
            } else {
                match serde_json::from_str::<serde_json::Value>(&call.function.arguments) {
                    Ok(serde_json::Value::Object(_)) => None,
                    _ => Some("invalid or incomplete tool-call arguments"),
                }
            };
            if let Some(issue) = issue {
                return Err(self.malformed(format!("{issue} (index={index})")));
            }
        }
        if matches!(
            self.finish_reason.as_deref(),
            Some("tool_calls" | "tool_use")
        ) && self.tool_calls.is_empty()
        {
            return Err(self.malformed("tool-call finish reason without tool calls"));
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn response(tool_calls: Vec<ToolCall>) -> ParsedResponse {
        ParsedResponse {
            finish_reason: Some("tool_calls".into()),
            content: None,
            reasoning: None,
            reasoning_parts: Vec::new(),
            reasoning_blocks: None,
            tool_calls,
            usage: TokenUsage {
                prompt_tokens: Some(17),
                completion_tokens: Some(5),
                ..Default::default()
            },
        }
    }

    fn call(id: &str, arguments: &str) -> ToolCall {
        ToolCall::new(
            id.into(),
            FunctionCall {
                name: "test".into(),
                arguments: arguments.into(),
            },
        )
    }

    #[test]
    fn malformed_batch_preserves_usage_and_finish_reason_without_arguments() {
        for arguments in [
            "{\"sensitive-fixture\":",
            "[]",
            "null",
            "1",
            "\"sensitive-fixture\"",
        ] {
            let parsed = response(vec![call("valid", "{}"), call("invalid", arguments)]);
            let error = parsed.validate().unwrap_err();
            assert!(!format!("{error:?}").contains("sensitive-fixture"));
            let crate::ProviderError::MalformedResponse {
                issue,
                finish_reason,
                usage,
            } = error
            else {
                panic!("expected malformed response")
            };
            assert!(issue.contains("index=1"));
            assert_eq!(finish_reason.as_deref(), Some("tool_calls"));
            assert_eq!(usage, parsed.usage);
        }
    }

    #[test]
    fn validates_metadata_duplicate_ids_and_empty_batch() {
        for calls in [
            vec![call("", "{}")],
            vec![call("duplicate", "{}"), call("duplicate", "{}")],
            vec![],
        ] {
            assert!(matches!(
                response(calls).validate(),
                Err(crate::ProviderError::MalformedResponse { .. })
            ));
        }
        let mut missing_name = call("id", "{}");
        missing_name.function.name.clear();
        assert!(response(vec![missing_name]).validate().is_err());
        assert!(response(vec![call("id", "{}")]).validate().is_ok());
    }
}
