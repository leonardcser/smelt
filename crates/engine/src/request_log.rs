//! Persistent provider request audit for session introspection.

use protocol::request_log::{RequestError, RequestLogEntry, RequestResponse};
use smelt_provider::{ProviderError, RequestAttemptInfo};
use std::io::Write;

#[derive(Default)]
struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("serialized request length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn serialized_json_len(value: &impl serde::Serialize) -> usize {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value)
        .map(|()| counter.0)
        .unwrap_or_default()
}

/// Build one request audit entry and choose its storage payload mode.
pub fn entry(
    ctx: RequestContext,
    info: &RequestAttemptInfo<'_>,
    pricing: &smelt_provider::ResolvedPricing,
    mode: protocol::RequestAuditMode,
) -> Option<(RequestLogEntry, smelt_store::RequestAuditPayloadMode)> {
    let _perf = smelt_perf::perf::begin("engine:request_audit:build_entry");
    let (payload_mode, include_payloads) = match mode {
        protocol::RequestAuditMode::Off => return None,
        protocol::RequestAuditMode::Summary => (
            smelt_store::RequestAuditPayloadMode::Summary {
                raw_body_size: Some(serialized_json_len(info.body) as u64),
            },
            false,
        ),
        protocol::RequestAuditMode::Full => (smelt_store::RequestAuditPayloadMode::Full, true),
    };
    Some((
        build_entry(ctx, info, pricing, include_payloads),
        payload_mode,
    ))
}

/// Append one request attempt to the session's request audit.
#[cfg(test)]
pub fn append(
    writer: &mut smelt_store::OwnedLineageWriter,
    ctx: RequestContext,
    info: &RequestAttemptInfo<'_>,
    pricing: &smelt_provider::ResolvedPricing,
    mode: protocol::RequestAuditMode,
) -> Result<Option<i64>, smelt_store::StoreError> {
    let Some((entry, payload_mode)) = entry(ctx, info, pricing, mode) else {
        return Ok(None);
    };
    writer
        .append_request_attempt(&entry, payload_mode)
        .map(Some)
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
    info: &RequestAttemptInfo<'_>,
    pricing: &smelt_provider::ResolvedPricing,
    include_payloads: bool,
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
                content: resp.content.as_deref().map(|content| {
                    if include_payloads {
                        content.to_string()
                    } else {
                        bounded_text(content, 512)
                    }
                }),
                reasoning: resp.reasoning_content.as_deref().map(|reasoning| {
                    if include_payloads {
                        reasoning.to_string()
                    } else {
                        bounded_text(reasoning, 512)
                    }
                }),
                tool_calls: if include_payloads && !resp.tool_calls.is_empty() {
                    Some(resp.tool_calls.clone())
                } else {
                    None
                },
                raw: include_payloads
                    .then(|| info.raw_response.cloned())
                    .flatten(),
            });
            (response, usage, cost, resp.tokens_per_sec)
        }
        Err(_) => (None, None, None, None),
    };

    let error = info.result.err().map(|err| {
        provider_error_to_log_error(
            err,
            info.http_status,
            include_payloads
                .then(|| info.error_body.map(truncate_error_body))
                .flatten(),
        )
    });

    RequestLogEntry {
        request_id: ctx.request_id,
        kind: ctx.kind,
        turn_id: ctx.turn_id,
        ask_id: ctx.ask_id,
        history_len: ctx.history_len,
        timestamp_ms,
        provider_kind: info.provider_kind.as_config_str().to_string(),
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
        body: if include_payloads {
            info.body.clone()
        } else {
            serde_json::Value::Null
        },
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
    err: &ProviderError,
    http_status: Option<u16>,
    body: Option<String>,
) -> RequestError {
    let (kind, status) = match err {
        ProviderError::Cancelled => ("cancelled", http_status),
        ProviderError::RateLimited { .. } => ("rate_limited", http_status),
        ProviderError::QuotaExceeded { .. } => ("quota", http_status),
        ProviderError::Auth(_) => ("auth", http_status),
        ProviderError::NotFound(_) => ("not_found", http_status),
        ProviderError::CyberPolicy { .. } => ("cyber_policy", http_status),
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

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    smelt_buffer::text::slice(text, 0..max_bytes).to_string()
}

fn truncate_error_body(body: &str) -> String {
    const LIMIT: usize = 64 * 1024;
    if body.len() <= LIMIT {
        return body.to_string();
    }
    let mut out = smelt_buffer::text::slice(body, 0..LIMIT).to_string();
    out.push_str("\n… truncated …");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::TokenUsage;
    use smelt_provider::{ChatResponse, ProviderError, ProviderKind, RequestAttemptInfo};
    use smelt_provider::{ModelPricing, PricingSource, ResolvedPricing};

    const SESSION_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn open_writer(root: &std::path::Path) -> smelt_store::OwnedLineageWriter {
        let mut writer = smelt_store::OwnedLineageWriter::open(root, SESSION_ID).unwrap();
        writer
            .commit_session(&smelt_store::SessionCommit {
                session_id: SESSION_ID.into(),
                expected: smelt_store::StoreHead::default(),
                identity: smelt_store::SessionIdentity {
                    id: SESSION_ID.into(),
                    created_at: 1,
                    parent_id: None,
                },
                metadata: smelt_store::SessionMetadata {
                    title: None,
                    slug: None,
                    first_user_message: None,
                    cwd: None,
                    mode: None,
                    reasoning_effort: None,
                    model: None,
                    fast_mode: None,
                    accounting_json: None,
                    checkpoint_json: None,
                    checkpoint_events_json: None,
                    context_tokens: None,
                    context_tokens_history_len: None,
                    display_context_tokens: None,
                    session_cost_usd: smelt_store::SessionCostUsd::new(0.0).unwrap(),
                    updated_at: 1,
                },
                history: smelt_store::HistorySuffix {
                    start: smelt_store::HistoryIndex::ZERO,
                    final_len: smelt_store::HistoryLen::ZERO,
                    items: Vec::new(),
                },
                side_tables: smelt_store::SideTableSuffixes::default(),
                transcript_records: None,
            })
            .unwrap();
        writer
    }

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
    fn counting_serialization_matches_buffered_serialization() {
        let value = serde_json::json!({
            "messages": [
                {"role": "user", "content": "héllo"},
                {"role": "assistant", "content": [1, 2, 3]},
            ],
            "stream": true,
        });

        assert_eq!(
            serialized_json_len(&value),
            serde_json::to_vec(&value).unwrap().len()
        );
    }

    #[test]
    fn summary_entry_keeps_size_without_retaining_full_payloads() {
        let body = serde_json::json!({
            "model": "gpt-test",
            "messages": [{"role": "user", "content": "x".repeat(8 * 1024)}],
            "prompt_cache_key": "session",
            "stream": true,
        });
        let response_text = format!("{}é-tail", "x".repeat(511));
        let raw_response = serde_json::json!({"content": response_text});
        let response = ChatResponse {
            content: raw_response["content"].as_str().map(ToString::to_string),
            reasoning_content: Some("reasoning".repeat(128)),
            reasoning_parts: Vec::new(),
            reasoning_details: None,
            tool_calls: Vec::new(),
            usage: sample_usage(),
            tokens_per_sec: Some(42.0),
            metadata: Default::default(),
        };
        let info = RequestAttemptInfo {
            url: "https://api.example.com/v1/chat/completions",
            provider_kind: ProviderKind::OpenAiCompatible,
            model: "gpt-test",
            body: &body,
            attempt: 1,
            elapsed_ms: 42,
            result: Ok(&response),
            raw_response: Some(&raw_response),
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

        let (entry, mode) = entry(
            ctx,
            &info,
            &zero_pricing(),
            protocol::RequestAuditMode::Summary,
        )
        .unwrap();

        assert_eq!(entry.body, serde_json::Value::Null);
        assert_eq!(entry.prompt_cache_key.as_deref(), Some("session"));
        assert!(entry.stream);
        let response = entry.response.unwrap();
        assert_eq!(response.content.unwrap(), "x".repeat(511));
        assert_eq!(response.reasoning.as_ref().map(String::len), Some(512));
        assert!(response.tool_calls.is_none());
        assert!(response.raw.is_none());
        assert_eq!(
            mode,
            smelt_store::RequestAuditPayloadMode::Summary {
                raw_body_size: Some(serde_json::to_vec(&body).unwrap().len() as u64),
            }
        );
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
        let mut writer = open_writer(tmp.path());
        let body = serde_json::json!({"model": "gpt-test", "messages": []});
        let resp = ChatResponse {
            content: Some("hello".into()),
            reasoning_content: None,
            reasoning_parts: Vec::new(),
            reasoning_details: None,
            tool_calls: Vec::new(),
            usage: sample_usage(),
            tokens_per_sec: None,
            metadata: Default::default(),
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

        let id = append(
            &mut writer,
            ctx,
            &info,
            &zero_pricing(),
            protocol::RequestAuditMode::Off,
        )
        .unwrap();

        assert!(id.is_none());
        writer.release().unwrap();
        let reader =
            smelt_store::LineageSessionReader::open_existing(tmp.path(), SESSION_ID).unwrap();
        let attempts = reader
            .query_request_attempts(&smelt_store::RequestAuditQuery::default())
            .unwrap();
        assert!(attempts.is_empty());
    }

    #[test]
    fn request_log_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut writer = open_writer(tmp.path());

        let body = serde_json::json!({"model": "gpt-test", "messages": []});
        let url = "https://api.example.com/v1/chat/completions";
        let resp = ChatResponse {
            content: Some("hello".into()),
            reasoning_content: None,
            reasoning_parts: Vec::new(),
            reasoning_details: None,
            tool_calls: Vec::new(),
            usage: sample_usage(),
            tokens_per_sec: Some(123.0),
            metadata: Default::default(),
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
            &mut writer,
            ctx,
            &info,
            &zero_pricing(),
            protocol::RequestAuditMode::Full,
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
            &mut writer,
            ctx_err,
            &info_err,
            &zero_pricing(),
            protocol::RequestAuditMode::Full,
        )
        .unwrap();

        writer.release().unwrap();
        let reader =
            smelt_store::LineageSessionReader::open_existing(tmp.path(), SESSION_ID).unwrap();
        let attempts = reader
            .query_request_attempts(&smelt_store::RequestAuditQuery {
                order: smelt_store::RequestAuditOrder::OldestFirst,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(attempts.len(), 2);

        let first = &attempts[0];
        assert_eq!(first.request_id.as_deref(), Some("7"));
        assert_eq!(first.kind.as_deref(), Some("turn"));
        assert_eq!(first.attempt, 1);
        assert_eq!(first.usage.as_ref().unwrap().prompt_tokens, Some(10));
        assert!(first.error_summary.is_none());
        let first_payloads = reader.request_payloads(first.id).unwrap().unwrap();
        let response: RequestResponse =
            serde_json::from_value(first_payloads.response.unwrap()).unwrap();
        assert_eq!(response.content, Some("hello".into()));

        let second = &attempts[1];
        assert_eq!(second.attempt, 2);
        assert!(second.response_hash.is_none());
        let second_payloads = reader.request_payloads(second.id).unwrap().unwrap();
        let error: RequestError = serde_json::from_value(second_payloads.error.unwrap()).unwrap();
        assert_eq!(error.kind, "network");
    }
}
