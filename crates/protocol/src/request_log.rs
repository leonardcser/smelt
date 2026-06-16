//! Persistent log of provider requests/responses for session introspection.
//!
//! Written by the engine as a `requests.jsonl` sidecar next to `meta.json`
//! and `history.jsonl`. One entry per logical request attempt, so retries and
//! auxiliary `EngineAsk` calls are all captured.

use crate::message::{Message, ToolCall};
use crate::usage::TokenUsage;
use serde::{Deserialize, Serialize};

/// One provider request attempt, written as a single JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogEntry {
    /// Stable id: `turn_id` for main turns, `ask_id` for auxiliary requests.
    pub request_id: u64,
    /// `"turn"` or `"engine_ask"`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_len: Option<usize>,
    pub timestamp_ms: u64,
    pub provider_kind: String,
    pub api_base: String,
    pub model: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub body: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<crate::event::ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<RequestResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    pub attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RequestError>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

/// Parsed response summary for the request log. Streaming responses do not
/// retain the raw SSE body, only the final parsed content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Verbatim non-streaming response body, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// Provider-facing error captured for the request log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestError {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}
