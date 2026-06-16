//! Persistent `requests.jsonl` sidecar for session introspection.

use protocol::request_log::{RequestError, RequestLogEntry, RequestResponse};
use protocol::{Message, ToolDef};
use std::io::Write;
use std::path::Path;

/// Append one request attempt to the session's `requests.jsonl`.
pub fn append(
    session_dir: &Path,
    ctx: RequestContext,
    info: &crate::provider::RequestAttemptInfo<'_>,
    pricing: &crate::pricing::ResolvedPricing,
) {
    let path = session_dir.join("requests.jsonl");
    let Some(parent) = path.parent() else { return };
    let _ = std::fs::create_dir_all(parent);

    let entry = build_entry(ctx, info, pricing);
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = writeln!(f, "{line}");
}

/// Static context for a logical request.
pub struct RequestContext {
    pub request_id: u64,
    pub kind: String,
    pub turn_id: Option<u64>,
    pub ask_id: Option<u64>,
    pub history_len: Option<usize>,
    pub background: bool,
    pub system_prompt: Option<String>,
    pub messages: Option<Vec<Message>>,
    pub tools: Option<Vec<ToolDef>>,
}

fn build_entry(
    ctx: RequestContext,
    info: &crate::provider::RequestAttemptInfo<'_>,
    pricing: &crate::pricing::ResolvedPricing,
) -> RequestLogEntry {
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let (response, usage, cost_usd, tokens_per_sec) = match info.result {
        Ok(resp) => {
            let usage = Some(resp.usage.clone());
            let cost = if pricing.pricing.is_zero() {
                None
            } else {
                Some(pricing.pricing.cost(&resp.usage))
            };
            let response = Some(RequestResponse {
                content: resp.content.clone(),
                reasoning: resp.reasoning_content.clone(),
                tool_calls: if resp.tool_calls.is_empty() {
                    None
                } else {
                    Some(resp.tool_calls.clone())
                },
                raw: info.raw_response.cloned(),
            });
            (response, usage, cost, resp.tokens_per_sec)
        }
        Err(_) => (None, None, None, None),
    };

    let error = info.result.err().map(provider_error_to_log_error);

    RequestLogEntry {
        request_id: ctx.request_id,
        kind: ctx.kind,
        turn_id: ctx.turn_id,
        ask_id: ctx.ask_id,
        history_len: ctx.history_len,
        timestamp_ms,
        provider_kind: info.provider_kind.as_str().to_string(),
        api_base: info
            .url
            .split("?")
            .next()
            .unwrap_or(info.url)
            .trim_end_matches('/')
            .to_string(),
        model: info.model.to_string(),
        url: info.url.to_string(),
        body: info.body.clone(),
        system_prompt: ctx.system_prompt,
        messages: ctx.messages,
        tools: ctx.tools,
        response,
        usage,
        cost_usd,
        tokens_per_sec,
        elapsed_ms: Some(info.elapsed_ms),
        attempt: info.attempt,
        error,
        background: ctx.background,
    }
}

fn provider_error_to_log_error(err: &crate::provider::ProviderError) -> RequestError {
    use crate::provider::ProviderError;
    let (kind, status) = match err {
        ProviderError::Cancelled => ("cancelled", None),
        ProviderError::RateLimited { .. } => ("rate_limited", None),
        ProviderError::QuotaExceeded(_) => ("quota", None),
        ProviderError::Auth(_) => ("auth", None),
        ProviderError::NotFound(_) => ("not_found", None),
        ProviderError::Server { status, .. } => ("server", Some(*status)),
        ProviderError::Network(_) => ("network", None),
        ProviderError::Stream(_) => ("stream", None),
        ProviderError::InvalidResponse(_) => ("invalid_response", None),
        ProviderError::MaxRetries => ("max_retries", None),
    };
    RequestError {
        kind: kind.to_string(),
        status,
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{ModelPricing, PricingSource, ResolvedPricing};
    use crate::provider::{LLMResponse, ProviderError, ProviderKind, RequestAttemptInfo};
    use protocol::{Message, TokenUsage};
    use std::io::Read;

    fn zero_pricing() -> ResolvedPricing {
        ResolvedPricing {
            pricing: ModelPricing {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            source: PricingSource::None,
        }
    }

    fn sample_usage() -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            context_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn request_log_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path();

        let body = serde_json::json!({"model": "gpt-test", "messages": []});
        let url = "https://api.example.com/v1/chat/completions";
        let resp = LLMResponse {
            content: Some("hello".into()),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: Vec::new(),
            usage: sample_usage(),
            tokens_per_sec: Some(123.0),
        };
        let info = RequestAttemptInfo {
            url,
            provider_kind: ProviderKind::OpenAiCompatible,
            model: "gpt-test",
            body: &body,
            attempt: 1,
            elapsed_ms: 42,
            result: Ok(&resp),
            raw_response: Some(&body),
        };
        let ctx = RequestContext {
            request_id: 7,
            kind: "turn".into(),
            turn_id: Some(7),
            ask_id: None,
            history_len: Some(3),
            background: false,
            system_prompt: Some("sys".into()),
            messages: Some(vec![Message::user(protocol::Content::text("hi"))]),
            tools: None,
        };
        append(session_dir, ctx, &info, &zero_pricing());

        let err = ProviderError::Network("timeout".into());
        let info_err = RequestAttemptInfo {
            url,
            provider_kind: ProviderKind::OpenAiCompatible,
            model: "gpt-test",
            body: &body,
            attempt: 2,
            elapsed_ms: 100,
            result: Err(&err),
            raw_response: None,
        };
        let ctx_err = RequestContext {
            request_id: 7,
            kind: "turn".into(),
            turn_id: Some(7),
            ask_id: None,
            history_len: Some(3),
            background: false,
            system_prompt: None,
            messages: None,
            tools: None,
        };
        append(session_dir, ctx_err, &info_err, &zero_pricing());

        let path = session_dir.join("requests.jsonl");
        let mut file = std::fs::File::open(&path).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: RequestLogEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.request_id, 7);
        assert_eq!(first.kind, "turn");
        assert_eq!(first.attempt, 1);
        assert_eq!(
            first.response.as_ref().unwrap().content,
            Some("hello".into())
        );
        assert_eq!(first.usage.as_ref().unwrap().prompt_tokens, Some(10));
        assert!(first.error.is_none());

        let second: RequestLogEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second.attempt, 2);
        assert!(second.response.is_none());
        assert_eq!(second.error.as_ref().unwrap().kind, "network");
    }
}
