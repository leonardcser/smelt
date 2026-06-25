//! Persistent provider request audit for session introspection.

use protocol::request_log::{RequestError, RequestLogEntry, RequestResponse};

/// Append one request attempt to the session's SQLite request audit.
pub fn append(
    db: &smelt_store::SessionDb,
    ctx: RequestContext,
    info: &crate::provider::RequestAttemptInfo<'_>,
    pricing: &crate::pricing::ResolvedPricing,
    mode: crate::RequestAuditMode,
) -> Result<Option<i64>, smelt_store::StoreError> {
    let Some(payload_mode) = mode.payload_mode() else {
        return Ok(None);
    };
    let entry = build_entry(ctx, info, pricing);
    db.append_request_attempt(&entry, payload_mode).map(Some)
}

/// Static context for a logical request.
pub struct RequestContext {
    pub request_id: u64,
    pub kind: String,
    pub turn_id: Option<u64>,
    pub ask_id: Option<u64>,
    pub history_len: Option<usize>,
    pub background: bool,
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

    let error = info.result.err().map(|err| {
        provider_error_to_log_error(
            err,
            info.http_status,
            info.error_body.map(truncate_error_body),
        )
    });

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
        http_status: info.http_status,
        body: info.body.clone(),
        prompt_cache_key: info
            .body
            .get("prompt_cache_key")
            .and_then(|v| v.as_str())
            .map(ToString::to_string),
        stream: info
            .body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        system_prompt: None,
        messages: None,
        tools: None,
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

fn provider_error_to_log_error(
    err: &crate::provider::ProviderError,
    http_status: Option<u16>,
    body: Option<String>,
) -> RequestError {
    use crate::provider::ProviderError;
    let (kind, status) = match err {
        ProviderError::Cancelled => ("cancelled", http_status),
        ProviderError::RateLimited { .. } => ("rate_limited", http_status),
        ProviderError::QuotaExceeded { .. } => ("quota", http_status),
        ProviderError::Auth(_) => ("auth", http_status),
        ProviderError::NotFound(_) => ("not_found", http_status),
        ProviderError::Server { status, .. } => ("server", Some(*status)),
        ProviderError::Network(_) => ("network", http_status),
        ProviderError::Stream(_) => ("stream", http_status),
        ProviderError::InvalidResponse(_) => ("invalid_response", http_status),
        ProviderError::MaxRetries => ("max_retries", http_status),
    };
    RequestError {
        kind: kind.to_string(),
        status,
        message: err.to_string(),
        body,
    }
}

fn truncate_error_body(body: &str) -> String {
    const LIMIT: usize = 64 * 1024;
    if body.len() <= LIMIT {
        return body.to_string();
    }
    let end = body
        .char_indices()
        .map(|(idx, _)| idx)
        .take_while(|&idx| idx <= LIMIT)
        .last()
        .unwrap_or(0);
    let mut out = body[..end].to_string();
    out.push_str("\n… truncated …");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{ModelPricing, PricingSource, ResolvedPricing};
    use crate::provider::{LLMResponse, ProviderError, ProviderKind, RequestAttemptInfo};
    use protocol::TokenUsage;

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
    fn truncates_error_body_on_char_boundary() {
        let mut body = "a".repeat(64 * 1024 - 1);
        body.push('é');
        body.push_str("tail");

        let truncated = truncate_error_body(&body);

        assert!(truncated.ends_with("\n… truncated …"));
        assert!(truncated.starts_with(&"a".repeat(64 * 1024 - 1)));
    }

    #[test]
    fn request_log_off_skips_database_write() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path();
        let body = serde_json::json!({"model": "gpt-test", "messages": []});
        let resp = LLMResponse {
            content: Some("hello".into()),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: Vec::new(),
            usage: sample_usage(),
            tokens_per_sec: None,
        };
        let info = RequestAttemptInfo {
            url: "https://api.example.com/v1/chat/completions",
            provider_kind: ProviderKind::OpenAiCompatible,
            model: "gpt-test",
            body: &body,
            attempt: 1,
            elapsed_ms: 42,
            result: Ok(&resp),
            raw_response: None,
            http_status: Some(200),
            error_body: None,
        };
        let ctx = RequestContext {
            request_id: 7,
            kind: "turn".into(),
            turn_id: Some(7),
            ask_id: None,
            history_len: Some(3),
            background: false,
        };

        let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();
        let id = append(
            &db,
            ctx,
            &info,
            &zero_pricing(),
            crate::RequestAuditMode::Off,
        )
        .unwrap();

        assert!(id.is_none());
        let attempts = db
            .query_request_attempts(&smelt_store::RequestAuditQuery::default())
            .unwrap();
        assert!(attempts.is_empty());
    }

    #[test]
    fn request_log_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path();
        let db = smelt_store::SessionDb::open(session_dir.join("session.db")).unwrap();

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
            http_status: Some(200),
            error_body: None,
        };
        let ctx = RequestContext {
            request_id: 7,
            kind: "turn".into(),
            turn_id: Some(7),
            ask_id: None,
            history_len: Some(3),
            background: false,
        };
        append(
            &db,
            ctx,
            &info,
            &zero_pricing(),
            crate::RequestAuditMode::Full,
        )
        .unwrap();

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
            http_status: None,
            error_body: None,
        };
        let ctx_err = RequestContext {
            request_id: 7,
            kind: "turn".into(),
            turn_id: Some(7),
            ask_id: None,
            history_len: Some(3),
            background: false,
        };
        append(
            &db,
            ctx_err,
            &info_err,
            &zero_pricing(),
            crate::RequestAuditMode::Full,
        )
        .unwrap();

        let attempts = db
            .query_request_attempts(&smelt_store::RequestAuditQuery {
                order: smelt_store::RequestAuditOrder::OldestFirst,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(!session_dir.join("requests.jsonl").exists());

        let first = &attempts[0];
        assert_eq!(first.request_id.as_deref(), Some("7"));
        assert_eq!(first.kind.as_deref(), Some("turn"));
        assert_eq!(first.attempt, 1);
        assert_eq!(first.usage.as_ref().unwrap().prompt_tokens, Some(10));
        assert!(first.error_summary.is_none());
        let first_payloads = db.request_payloads(first.id).unwrap().unwrap();
        let response: RequestResponse =
            serde_json::from_value(first_payloads.response.unwrap()).unwrap();
        assert_eq!(response.content, Some("hello".into()));

        let second = &attempts[1];
        assert_eq!(second.attempt, 2);
        assert!(second.response_hash.is_none());
        let second_payloads = db.request_payloads(second.id).unwrap().unwrap();
        let error: RequestError = serde_json::from_value(second_payloads.error.unwrap()).unwrap();
        assert_eq!(error.kind, "network");
    }
}
