mod anthropic;
mod auth_storage;
mod chat_completions;
pub mod codex;
pub mod copilot;
mod extract;
mod openai;
mod sse;

use crate::cancel::CancellationToken;
use crate::log;
pub(crate) use protocol::TokenUsage;
use protocol::{Content, Message, ReasoningEffort, ToolCall};
use reqwest::Client;
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    def_type: AlwaysFunctionDef,
    pub(crate) function: FunctionSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
struct AlwaysFunctionDef;

impl Serialize for AlwaysFunctionDef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("function")
    }
}

impl ToolDefinition {
    pub fn new(function: FunctionSchema) -> Self {
        Self {
            def_type: AlwaysFunctionDef,
            function,
        }
    }
}

/// Internal parsed fields from an API response.
pub(crate) struct ParsedResponse {
    pub(crate) content: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) usage: TokenUsage,
}

impl ParsedResponse {
    pub(crate) fn into_response(self, tokens_per_sec: Option<f64>) -> LLMResponse {
        LLMResponse {
            content: self.content,
            reasoning_content: self.reasoning,
            tool_calls: self.tool_calls,
            usage: self.usage,
            tokens_per_sec,
        }
    }
}

pub(crate) fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub(crate) fn collect_indexed_tool_calls(
    map: std::collections::HashMap<usize, (String, String, String)>,
) -> Vec<ToolCall> {
    let mut vec: Vec<(usize, ToolCall)> = map
        .into_iter()
        .map(|(idx, (id, name, args))| {
            (
                idx,
                ToolCall::new(
                    id,
                    protocol::FunctionCall {
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

/// A streaming delta from the LLM.
pub(crate) enum StreamDelta<'a> {
    Text(&'a str),
    Thinking(&'a str),
    ToolArgs {
        call_id: &'a str,
        tool_name: &'a str,
        delta: &'a str,
    },
}

pub(crate) struct LLMResponse {
    pub(crate) content: Option<String>,
    pub(crate) reasoning_content: Option<String>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) usage: TokenUsage,
    pub(crate) tokens_per_sec: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProviderError {
    #[error("cancelled")]
    Cancelled,
    #[error("{}", format_rate_limit(resets_at))]
    RateLimited { resets_at: Option<u64> },
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("max retries exceeded")]
    MaxRetries,
}

fn format_rate_limit(resets_at: &Option<u64>) -> String {
    let Some(epoch) = resets_at else {
        return "rate limited".to_string();
    };
    let time_str = format_epoch_local(*epoch);
    format!("rate limited — try again at {time_str}")
}

fn format_epoch_local(epoch_secs: u64) -> String {
    #[cfg(unix)]
    {
        let t = epoch_secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };

        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let month = MONTHS[tm.tm_mon as usize % 12];
        let day = tm.tm_mday;
        let year = tm.tm_year + 1900;
        let suffix = match day % 10 {
            1 if day != 11 => "st",
            2 if day != 12 => "nd",
            3 if day != 13 => "rd",
            _ => "th",
        };
        let (hour12, ampm) = match tm.tm_hour {
            0 => (12, "AM"),
            1..=11 => (tm.tm_hour, "AM"),
            12 => (12, "PM"),
            _ => (tm.tm_hour - 12, "PM"),
        };
        format!(
            "{month} {day}{suffix}, {year} {hour12}:{:02} {ampm}",
            tm.tm_min
        )
    }
    #[cfg(not(unix))]
    {
        let _ = epoch_secs;
        "later".to_string()
    }
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ProviderError {
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProviderError::Server { .. } | ProviderError::Network(_)
        )
    }

    fn from_http(code: u16, body: String, retry_after: Option<Duration>) -> Self {
        let is_quota = body.contains("insufficient_quota")
            || body.contains("billing_not_active")
            || body.contains("credit balance is too low")
            || (code == 429 && body.contains("exceeded"));

        match code {
            _ if is_quota => ProviderError::QuotaExceeded(body),
            400 => ProviderError::InvalidResponse(body),
            401 | 403 => ProviderError::Auth(body),
            404 => ProviderError::NotFound(body),
            429 => ProviderError::RateLimited {
                resets_at: parse_resets_at(&body)
                    .or_else(|| retry_after.map(|d| unix_now() + d.as_secs())),
            },
            _ => ProviderError::Server { status: code, body },
        }
    }
}

fn parse_resets_at(body: &str) -> Option<u64> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    json.get("error")
        .and_then(|e| e.get("resets_at"))
        .and_then(json_as_u64)
}

pub(crate) fn json_as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_i64().map(|i| i as u64))
}

pub(crate) fn parse_retry_from_body(body: &str) -> Option<Duration> {
    let lower = body.to_ascii_lowercase();
    let idx = lower.find("try again in")?;
    let after = &lower[idx + "try again in".len()..];
    let trimmed = after.trim_start();

    let end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let value: f64 = trimmed[..end].parse().ok()?;

    let unit = trimmed[end..].trim_start();
    if unit.starts_with("ms") {
        Some(Duration::from_millis(value as u64))
    } else {
        Some(Duration::from_secs_f64(value))
    }
}

fn backoff_delay(attempt: usize) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt as u32))
}

fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let val = resp.headers().get("retry-after")?.to_str().ok()?;
    val.parse::<f64>()
        .ok()
        .filter(|&s| s > 0.0)
        .map(Duration::from_secs_f64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiCompatible,
    OpenAi,
    Codex,
    AnthropicCompatible,
    Anthropic,
    Copilot,
}

impl ProviderKind {
    pub fn default_reasoning_cycle(self) -> &'static [ReasoningEffort] {
        match self {
            Self::OpenAiCompatible => &[
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            Self::OpenAi
            | Self::Codex
            | Self::AnthropicCompatible
            | Self::Anthropic
            | Self::Copilot => &[
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ],
        }
    }

    pub fn from_config(provider_type: &str) -> Self {
        match provider_type {
            "openai" => Self::OpenAi,
            "codex" => Self::Codex,
            "anthropic-compatible" => Self::AnthropicCompatible,
            "anthropic" => Self::Anthropic,
            "copilot" | "github-copilot" => Self::Copilot,
            _ => Self::OpenAiCompatible,
        }
    }

    pub fn detect_from_url(api_base: &str) -> Self {
        if api_base.contains("api.kimi.com/coding") {
            Self::AnthropicCompatible
        } else if api_base.contains("api.anthropic.com") {
            Self::Anthropic
        } else if api_base.contains("api.openai.com") {
            Self::OpenAi
        } else if api_base.contains("chatgpt.com") {
            Self::Codex
        } else if api_base.contains("githubcopilot.com") {
            Self::Copilot
        } else {
            Self::OpenAiCompatible
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::OpenAi => "openai",
            Self::Codex => "codex",
            Self::AnthropicCompatible => "anthropic-compatible",
            Self::Anthropic => "anthropic",
            Self::Copilot => "copilot",
        }
    }
}

/// Structured JSON output schema. Each provider adapter maps this to its native field.
#[derive(Clone)]
pub(crate) struct ResponseFormat {
    pub(crate) name: String,
    pub(crate) schema: serde_json::Value,
}

pub(crate) struct ChatOptions<'a> {
    pub(crate) cancel: &'a CancellationToken,
    pub(crate) on_retry: Option<&'a (dyn Fn(Duration, u32) + Send + Sync)>,
    pub(crate) on_delta: Option<&'a (dyn Fn(StreamDelta<'_>) + Send + Sync)>,
    pub(crate) response_format: Option<ResponseFormat>,
}

impl<'a> ChatOptions<'a> {
    pub(crate) fn new(cancel: &'a CancellationToken) -> Self {
        Self {
            cancel,
            on_retry: None,
            on_delta: None,
            response_format: None,
        }
    }
}

#[derive(Clone)]
pub struct Provider {
    api_base: String,
    api_key: String,
    client: Client,
    kind: ProviderKind,
    model_config: crate::config::ModelConfig,
    /// Sticky routing token for Codex: set from the first response, echoed on subsequent requests within the same turn.
    turn_state: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    clock: std::sync::Arc<dyn crate::clock::Clock>,
}

/// Ensure `tool_calls[].function.arguments` is valid JSON; some models emit malformed strings.
pub(crate) fn sanitize_tool_call_arguments(obj: &mut serde_json::Map<String, serde_json::Value>) {
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

impl Provider {
    pub fn new(
        api_base: String,
        api_key: String,
        provider_type: &str,
        client: Client,
        clock: std::sync::Arc<dyn crate::clock::Clock>,
    ) -> Self {
        let api_base = api_base.trim_end_matches('/').to_string();
        let kind = ProviderKind::from_config(provider_type);
        Self {
            api_base,
            api_key,
            client,
            kind,
            model_config: Default::default(),
            turn_state: std::sync::Arc::new(std::sync::Mutex::new(None)),
            clock,
        }
    }

    pub(crate) fn reset_turn_state(&self) {
        *self.turn_state.lock().unwrap() = None;
    }

    #[cfg(test)]
    pub(crate) fn api_base(&self) -> &str {
        &self.api_base
    }

    #[cfg(test)]
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    #[cfg(test)]
    pub(crate) fn model_config_for_test(&self) -> &crate::config::ModelConfig {
        &self.model_config
    }

    pub(crate) fn with_model_config(mut self, config: crate::config::ModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub(crate) fn tool_calling(&self) -> bool {
        self.model_config.tool_calling()
    }

    pub(crate) async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        effort: ReasoningEffort,
        opts: &ChatOptions<'_>,
    ) -> Result<LLMResponse, ProviderError> {
        let is_anthropic =
            self.kind == ProviderKind::Anthropic || self.kind == ProviderKind::AnthropicCompatible;
        let is_codex = self.kind == ProviderKind::Codex;
        let is_copilot = self.kind == ProviderKind::Copilot;

        let mut codex_auth = if is_codex {
            Some(
                codex::ensure_access_token_full(&self.client)
                    .await
                    .map_err(ProviderError::Auth)?,
            )
        } else {
            None
        };
        let mut codex_401_retried = false;

        let mut copilot_auth = if is_copilot {
            Some(
                copilot::ensure_access_token_full(&self.client)
                    .await
                    .map_err(ProviderError::Auth)?,
            )
        } else {
            None
        };
        let mut copilot_401_retried = false;

        let (mut url, mut body) = match self.kind {
            ProviderKind::OpenAiCompatible => {
                let url = format!("{}/chat/completions", self.api_base);
                let body = chat_completions::build_body(
                    messages,
                    tools,
                    model,
                    effort,
                    &self.model_config,
                );
                (url, body)
            }
            ProviderKind::OpenAi => {
                let url = format!("{}/responses", self.api_base);
                let body = openai::build_body(messages, tools, model, effort, &self.model_config);
                (url, body)
            }
            ProviderKind::Codex => {
                let url = codex::CODEX_API_ENDPOINT.to_string();
                let mut body =
                    openai::build_body(messages, tools, model, effort, &self.model_config);
                body["store"] = serde_json::json!(false);
                if let Some(obj) = body.as_object_mut() {
                    obj.remove("temperature");
                    obj.remove("top_p");
                }
                (url, body)
            }
            ProviderKind::AnthropicCompatible | ProviderKind::Anthropic => {
                let url = format!("{}/messages", self.api_base);
                let body =
                    anthropic::build_body(messages, tools, model, effort, &self.model_config);
                (url, body)
            }
            ProviderKind::Copilot => {
                // Base URL comes from the Copilot token's proxy-ep claim.
                let base = copilot_auth
                    .as_ref()
                    .map(|t| t.api_base.as_str())
                    .unwrap_or(copilot::DEFAULT_COPILOT_API_BASE);
                let url = format!("{}/chat/completions", base.trim_end_matches('/'));
                let body = chat_completions::build_body(
                    messages,
                    tools,
                    model,
                    effort,
                    &self.model_config,
                );
                (url, body)
            }
        };

        let copilot_initiator: &'static str = if is_copilot {
            match messages.last().map(|m| m.role) {
                Some(protocol::Role::User) | None => "user",
                _ => "agent",
            }
        } else {
            "user"
        };
        let copilot_has_images = is_copilot && messages_have_images(messages);

        if let Some(fmt) = opts.response_format.as_ref() {
            apply_response_format(&mut body, self.kind, fmt);
        }

        let use_stream = opts.on_delta.is_some() || is_codex;
        if use_stream {
            body["stream"] = serde_json::json!(true);
            if self.kind == ProviderKind::OpenAiCompatible {
                body["stream_options"] = serde_json::json!({"include_usage": true});
            }
        }

        if log::Level::Debug.enabled() {
            log::entry(
                log::Level::Debug,
                "request",
                &serde_json::json!({
                    "url": url,
                    "provider_kind": format!("{:?}", self.kind),
                    "body": body,
                }),
            );
        }

        let max_retries = 9;

        for attempt in 0..=max_retries {
            let request_start = self.clock.instant_now();

            let mut req = self.client.post(&url).json(&body);
            if is_codex {
                if let Some(ref tokens) = codex_auth {
                    req = req.bearer_auth(&tokens.access_token);
                    if let Some(id) = &tokens.account_id {
                        req = req.header("ChatGPT-Account-Id", id);
                    }
                    req = req.header("originator", "smelt");
                    if let Some(ref ts) = *self.turn_state.lock().unwrap() {
                        req = req.header("x-codex-turn-state", ts.as_str());
                    }
                }
            } else if is_copilot {
                if let Some(ref tokens) = copilot_auth {
                    req = req.bearer_auth(&tokens.access_token);
                }
                for (k, v) in copilot::base_headers() {
                    req = req.header(k, v);
                }
                req = req
                    .header("X-Initiator", copilot_initiator)
                    .header("Openai-Intent", "conversation-edits");
                if copilot_has_images {
                    req = req.header("Copilot-Vision-Request", "true");
                }
            } else if !self.api_key.is_empty() {
                if is_anthropic {
                    req = req.header("x-api-key", &self.api_key);
                } else {
                    req = req.bearer_auth(&self.api_key);
                }
            }
            if is_anthropic {
                req = req.header("anthropic-version", "2023-06-01");
            }

            let resp = tokio::select! {
                biased;
                _ = opts.cancel.cancelled() => {
                    return Err(ProviderError::Cancelled);
                }
                result = req.send() => match result {
                    Ok(r) => r,
                    Err(e) => {
                        let err = ProviderError::Network(e.to_string());
                        log::entry(log::Level::Warn, "request_error", &serde_json::json!({
                            "attempt": attempt,
                            "error": format!("{e:?}"),
                        }));
                        if attempt < max_retries {
                            let delay = backoff_delay(attempt);
                            if attempt > 0 {
                                if let Some(f) = opts.on_retry { f(delay, attempt as u32); }
                            }
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        return Err(err);
                    }
                }
            };

            if !resp.status().is_success() {
                let code = resp.status().as_u16();
                let retry_after = parse_retry_after(&resp);
                let text = resp.text().await.unwrap_or_default();

                let err = ProviderError::from_http(code, text, retry_after);

                log::entry(
                    log::Level::Warn,
                    "request_error",
                    &serde_json::json!({
                        "attempt": attempt,
                        "status": code,
                        "retry_after_secs": retry_after.map(|d| d.as_secs_f64()),
                        "error": err.to_string(),
                    }),
                );

                if is_codex && matches!(err, ProviderError::Auth(_)) && !codex_401_retried {
                    codex_401_retried = true;
                    if let Some(ref stale) = codex_auth {
                        if let Ok(refreshed) =
                            codex::refresh_tokens(&self.client, &stale.refresh_token).await
                        {
                            log::entry(
                                log::Level::Info,
                                "codex_401_recovery",
                                &serde_json::json!({ "account_id": refreshed.account_id }),
                            );
                            codex_auth = Some(refreshed);
                            continue;
                        }
                    }
                }

                // Copilot: the short-lived access token may expire mid-flight; refresh once.
                if is_copilot && matches!(err, ProviderError::Auth(_)) && !copilot_401_retried {
                    copilot_401_retried = true;
                    if let Some(ref stale) = copilot_auth {
                        if let Ok(refreshed) =
                            copilot::refresh_tokens(&self.client, &stale.refresh_token).await
                        {
                            log::entry(
                                log::Level::Info,
                                "copilot_401_recovery",
                                &serde_json::json!({ "expires_at": refreshed.expires_at }),
                            );
                            url = format!(
                                "{}/chat/completions",
                                refreshed.api_base.trim_end_matches('/')
                            );
                            copilot_auth = Some(refreshed);
                            continue;
                        }
                    }
                }

                if err.is_retryable() && attempt < max_retries {
                    let backoff = backoff_delay(attempt);
                    let delay = retry_after.map_or(backoff, |ra| ra.max(backoff));
                    if attempt > 0 {
                        if let Some(f) = opts.on_retry {
                            f(delay, attempt as u32);
                        }
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(err);
            }

            if is_codex && self.turn_state.lock().unwrap().is_none() {
                if let Some(val) = resp.headers().get("x-codex-turn-state") {
                    if let Ok(s) = val.to_str() {
                        *self.turn_state.lock().unwrap() = Some(s.to_string());
                    }
                }
            }

            let noop_delta: &(dyn Fn(StreamDelta<'_>) + Send + Sync) = &|_| {};
            let on_delta = opts.on_delta.unwrap_or(noop_delta);

            let parsed = if use_stream {
                match self.kind {
                    ProviderKind::OpenAiCompatible | ProviderKind::Copilot => {
                        chat_completions::read_stream(resp, opts.cancel, on_delta).await
                    }
                    ProviderKind::OpenAi | ProviderKind::Codex => {
                        openai::read_stream(resp, opts.cancel, on_delta).await
                    }
                    ProviderKind::AnthropicCompatible | ProviderKind::Anthropic => {
                        anthropic::read_stream(resp, opts.cancel, on_delta).await
                    }
                }?
            } else {
                let data: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

                if log::Level::Debug.enabled() {
                    log::entry(
                        log::Level::Debug,
                        "raw_response",
                        &serde_json::json!({
                            "url": url,
                            "provider_kind": format!("{:?}", self.kind),
                            "data": data,
                        }),
                    );
                }

                match self.kind {
                    ProviderKind::OpenAiCompatible | ProviderKind::Copilot => {
                        chat_completions::parse_response(&data)?
                    }
                    ProviderKind::OpenAi | ProviderKind::Codex => openai::parse_response(&data)?,
                    ProviderKind::AnthropicCompatible | ProviderKind::Anthropic => {
                        anthropic::parse_response(&data)?
                    }
                }
            };

            let elapsed = request_start.elapsed();
            let tokens_per_sec = parsed.usage.completion_tokens.and_then(|c| {
                if c > 0 && elapsed.as_secs_f64() >= 0.001 {
                    Some(c as f64 / elapsed.as_secs_f64())
                } else {
                    None
                }
            });

            if log::Level::Debug.enabled() {
                log::entry(
                    log::Level::Debug,
                    "response",
                    &serde_json::json!({
                        "content": parsed.content,
                        "reasoning_content": parsed.reasoning,
                        "tool_calls": parsed.tool_calls,
                        "prompt_tokens": parsed.usage.prompt_tokens,
                    }),
                );
            }

            return Ok(parsed.into_response(tokens_per_sec));
        }

        Err(ProviderError::MaxRetries)
    }

    pub async fn fetch_context_window(&self, model: &str) -> Option<u32> {
        let result = match self.kind {
            ProviderKind::OpenAiCompatible => {
                self.fetch_context_window_openai_compatible(model).await
            }
            ProviderKind::OpenAi => None,
            ProviderKind::Codex => codex::cached_context_window(model),
            ProviderKind::AnthropicCompatible | ProviderKind::Anthropic => {
                self.fetch_context_window_anthropic(model).await
            }
            ProviderKind::Copilot => copilot::cached_context_window(model),
        };
        crate::log::entry(
            crate::log::Level::Info,
            "fetch_context_window",
            &serde_json::json!({
                "model": model,
                "provider": format!("{:?}", self.kind),
                "result": result,
            }),
        );
        result
    }

    async fn fetch_context_window_anthropic(&self, model: &str) -> Option<u32> {
        let url = format!("{}/models/{}", self.api_base, model);
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: serde_json::Value = resp.json().await.ok()?;
        data["max_input_tokens"].as_u64().map(|v| v as u32)
    }

    async fn fetch_context_window_openai_compatible(&self, model: &str) -> Option<u32> {
        let url = format!("{}/models", self.api_base);
        let mut req = self.client.get(&url);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: serde_json::Value = resp.json().await.ok()?;
        let models = data["data"].as_array()?;
        let entry = models.iter().find(|m| m["id"].as_str() == Some(model))?;

        if let Some(v) = entry["max_model_len"].as_u64() {
            return Some(v as u32);
        }

        if let Some(args) = entry["status"]["args"].as_array() {
            for i in 0..args.len().saturating_sub(1) {
                if args[i].as_str() == Some("--ctx-size") {
                    return args[i + 1].as_str()?.parse::<u32>().ok();
                }
            }
        }

        None
    }

    async fn complete_simple(
        &self,
        messages: &[Message],
        model: &str,
        response_format: Option<ResponseFormat>,
    ) -> Result<(String, TokenUsage), ProviderError> {
        let cancel = CancellationToken::new();
        let mut opts = ChatOptions::new(&cancel);
        opts.response_format = response_format;
        let resp = self
            .chat(messages, &[], model, ReasoningEffort::Off, &opts)
            .await?;
        let usage = resp.usage;
        let text = resp.content.unwrap_or_default().trim().to_string();
        if text.is_empty() {
            Err(ProviderError::InvalidResponse("empty response".into()))
        } else {
            Ok((text, usage))
        }
    }

    async fn complete_short(
        &self,
        prompt: &str,
        model: &str,
        response_format: Option<ResponseFormat>,
    ) -> Result<(String, TokenUsage), ProviderError> {
        let messages = vec![
            Message::system("Reasoning: low".to_string()),
            Message::user(Content::text(prompt)),
        ];
        self.complete_simple(&messages, model, response_format)
            .await
    }

    pub(crate) async fn complete_title(
        &self,
        last_user_message: &str,
        assistant_tail: &str,
        model: &str,
    ) -> Result<((String, String), TokenUsage), ProviderError> {
        let assistant_block = if assistant_tail.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nAssistant response (tail):\n{}", assistant_tail)
        };
        let prompt = format!(
            "Generate a concise session title and git-branch-style slug for a coding session.\n\
             \n\
             Title: 3-6 words, sentence case (capitalize only the first word and proper nouns, not Title Case), \
             clear enough that the user can recognize the session in a list.\n\
             Slug: 1-5 lowercase words separated by dashes, like a git branch name.\n\
             \n\
             Respond with a single JSON object, no markdown fences, no prose: \
             {{\"title\": \"...\", \"slug\": \"...\"}}\n\
             \n\
             Good examples:\n\
             {{\"title\": \"Fix login button on mobile\", \"slug\": \"fix-mobile-login\"}}\n\
             {{\"title\": \"Add OAuth authentication\", \"slug\": \"add-oauth\"}}\n\
             {{\"title\": \"Debug failing CI tests\", \"slug\": \"debug-ci-tests\"}}\n\
             {{\"title\": \"Refactor API client error handling\", \"slug\": \"refactor-api-errors\"}}\n\
             \n\
             Bad (too vague): {{\"title\": \"Code changes\", \"slug\": \"changes\"}}\n\
             Bad (too long): {{\"title\": \"Investigate and fix the issue where the login button does not respond on mobile\", \"slug\": \"fix\"}}\n\
             Bad (wrong case): {{\"title\": \"Fix Login Button On Mobile\", \"slug\": \"fix-login\"}}\n\
             \n\
             User message:\n{}{}",
            last_user_message, assistant_block
        );

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "title": {"type": "string"},
                "slug": {"type": "string"},
            },
            "required": ["title", "slug"],
            "additionalProperties": false,
        });
        let fmt = ResponseFormat {
            name: "session_title".to_string(),
            schema,
        };
        let (raw, usage) = self.complete_short(&prompt, model, Some(fmt)).await?;
        let (title, slug) = parse_title_and_slug(&raw);

        Ok(((title, slug), usage))
    }
}

#[derive(serde::Deserialize)]
struct TitleSlug {
    #[serde(default)]
    title: String,
    #[serde(default)]
    slug: String,
}

fn parse_title_and_slug(raw: &str) -> (String, String) {
    let (mut title, mut slug) = extract_json_title_slug(raw).unwrap_or_default();

    if title.is_empty() {
        title = normalize_short(raw);
    }
    if slug.is_empty() {
        slug = slugify(&title);
    }

    slug = slug
        .split('-')
        .filter(|w| !w.is_empty())
        .take(5)
        .collect::<Vec<_>>()
        .join("-");

    if title.len() > 64 {
        title.truncate(title.floor_char_boundary(64));
        title = title.trim().to_string();
    }

    (title, slug)
}

fn apply_response_format(body: &mut serde_json::Value, kind: ProviderKind, fmt: &ResponseFormat) {
    match kind {
        ProviderKind::OpenAiCompatible | ProviderKind::Copilot => {
            body["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": {
                    "name": fmt.name,
                    "schema": fmt.schema,
                    "strict": true,
                }
            });
        }
        ProviderKind::OpenAi | ProviderKind::Codex => {
            body["text"] = serde_json::json!({
                "format": {
                    "type": "json_schema",
                    "name": fmt.name,
                    "schema": fmt.schema,
                    "strict": true,
                }
            });
        }
        ProviderKind::AnthropicCompatible | ProviderKind::Anthropic => {
            // Older models (Haiku 3.5, Sonnet 3.7, etc.) 400 if this field is sent.
            let model = body["model"].as_str().unwrap_or("");
            if !anthropic_supports_structured_output(model) {
                return;
            }
            let format_val = serde_json::json!({
                "type": "json_schema",
                "schema": fmt.schema,
            });
            match body.get_mut("output_config") {
                Some(v) if v.is_object() => {
                    v["format"] = format_val;
                }
                _ => {
                    body["output_config"] = serde_json::json!({ "format": format_val });
                }
            }
        }
    }
}

fn anthropic_supports_structured_output(model: &str) -> bool {
    model.contains("-4-5") || model.contains("-4-6") || model.contains("mythos")
}

fn extract_json_title_slug(raw: &str) -> Option<(String, String)> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let parsed: TitleSlug = serde_json::from_str(&raw[start..=end]).ok()?;
    Some((
        parsed.title.trim().to_string(),
        parsed.slug.trim().to_string(),
    ))
}

fn messages_have_images(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m.role {
        protocol::Role::User | protocol::Role::Tool => {
            m.content.as_ref().is_some_and(|c| c.image_count() > 0)
        }
        _ => false,
    })
}

pub(crate) fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn normalize_short(raw: &str) -> String {
    let mut t = raw.trim().trim_matches('"').trim_matches('\'').to_string();
    t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.len() > 64 {
        t.truncate(t.floor_char_boundary(64));
        t = t.trim().to_string();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Content, FunctionCall, Role, ToolCall};
    use serde_json::json;

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: Some(Content::Text(text.into())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        }
    }

    // ---- non_empty ----

    #[test]
    fn non_empty_returns_none_for_empty_string() {
        assert_eq!(non_empty(String::new()), None);
    }

    #[test]
    fn non_empty_returns_some_for_non_empty_string() {
        assert_eq!(non_empty("hi".into()), Some("hi".to_string()));
    }

    // ---- collect_indexed_tool_calls ----

    #[test]
    fn collect_indexed_tool_calls_returns_sorted_by_index() {
        let mut map = std::collections::HashMap::new();
        map.insert(2, ("c".into(), "C".into(), "{}".into()));
        map.insert(0, ("a".into(), "A".into(), "{}".into()));
        map.insert(1, ("b".into(), "B".into(), "{}".into()));
        let calls = collect_indexed_tool_calls(map);
        let names: Vec<_> = calls.iter().map(|c| c.function.name.clone()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn collect_indexed_tool_calls_empty_input_returns_empty_vec() {
        let calls = collect_indexed_tool_calls(std::collections::HashMap::new());
        assert!(calls.is_empty());
    }

    // ---- json_as_u64 ----

    #[test]
    fn json_as_u64_handles_positive_u64() {
        assert_eq!(json_as_u64(&json!(42)), Some(42));
    }

    #[test]
    fn json_as_u64_handles_negative_i64_via_cast() {
        let v = json!(-1);
        // -1 as i64 cast to u64 wraps to u64::MAX.
        assert_eq!(json_as_u64(&v), Some(u64::MAX));
    }

    #[test]
    fn json_as_u64_returns_none_for_non_numeric() {
        assert_eq!(json_as_u64(&json!("nope")), None);
        assert_eq!(json_as_u64(&json!(null)), None);
    }

    // ---- parse_retry_from_body ----

    #[test]
    fn parse_retry_from_body_seconds_default_unit() {
        let d = parse_retry_from_body("you exceeded quota, try again in 5 seconds").unwrap();
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn parse_retry_from_body_ms_unit_uses_milliseconds() {
        let d = parse_retry_from_body("try again in 250ms please").unwrap();
        assert_eq!(d, Duration::from_millis(250));
    }

    #[test]
    fn parse_retry_from_body_fractional_seconds() {
        let d = parse_retry_from_body("try again in 1.5s").unwrap();
        assert!((d.as_secs_f64() - 1.5).abs() < 1e-6);
    }

    #[test]
    fn parse_retry_from_body_returns_none_when_phrase_absent() {
        assert_eq!(parse_retry_from_body("rate limited"), None);
    }

    #[test]
    fn parse_retry_from_body_returns_none_when_no_number_follows() {
        assert_eq!(parse_retry_from_body("try again in soon"), None);
    }

    #[test]
    fn parse_retry_from_body_case_insensitive() {
        assert!(parse_retry_from_body("TRY AGAIN IN 3 SECONDS").is_some());
    }

    // ---- parse_resets_at ----

    #[test]
    fn parse_resets_at_extracts_from_error_object() {
        let body = r#"{"error": {"resets_at": 1700000000}}"#;
        assert_eq!(parse_resets_at(body), Some(1700000000));
    }

    #[test]
    fn parse_resets_at_returns_none_for_invalid_json() {
        assert_eq!(parse_resets_at("not json"), None);
    }

    #[test]
    fn parse_resets_at_returns_none_when_field_absent() {
        assert_eq!(parse_resets_at(r#"{"error": {}}"#), None);
    }

    // ---- ProviderError::is_retryable ----

    #[test]
    fn is_retryable_true_for_network_and_server_errors() {
        assert!(ProviderError::Network("x".into()).is_retryable());
        assert!(ProviderError::Server {
            status: 500,
            body: "".into()
        }
        .is_retryable());
    }

    #[test]
    fn is_retryable_false_for_auth_quota_and_invalid_errors() {
        assert!(!ProviderError::Auth("x".into()).is_retryable());
        assert!(!ProviderError::QuotaExceeded("x".into()).is_retryable());
        assert!(!ProviderError::InvalidResponse("x".into()).is_retryable());
        assert!(!ProviderError::NotFound("x".into()).is_retryable());
        assert!(!ProviderError::Cancelled.is_retryable());
        assert!(!ProviderError::MaxRetries.is_retryable());
        assert!(!ProviderError::RateLimited { resets_at: None }.is_retryable());
    }

    // ---- ProviderError::from_http ----

    #[test]
    fn from_http_400_is_invalid_response() {
        let err = ProviderError::from_http(400, "bad".into(), None);
        assert!(matches!(err, ProviderError::InvalidResponse(_)));
    }

    #[test]
    fn from_http_401_and_403_are_auth() {
        assert!(matches!(
            ProviderError::from_http(401, "no".into(), None),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            ProviderError::from_http(403, "no".into(), None),
            ProviderError::Auth(_)
        ));
    }

    #[test]
    fn from_http_404_is_not_found() {
        assert!(matches!(
            ProviderError::from_http(404, "gone".into(), None),
            ProviderError::NotFound(_)
        ));
    }

    #[test]
    fn from_http_500_is_server() {
        match ProviderError::from_http(500, "boom".into(), None) {
            ProviderError::Server { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "boom");
            }
            e => panic!("expected Server, got {e:?}"),
        }
    }

    #[test]
    fn from_http_429_is_rate_limited_with_resets_from_body() {
        let body = r#"{"error":{"resets_at": 999}}"#;
        match ProviderError::from_http(429, body.into(), None) {
            ProviderError::RateLimited { resets_at } => assert_eq!(resets_at, Some(999)),
            e => panic!("expected RateLimited, got {e:?}"),
        }
    }

    #[test]
    fn from_http_429_falls_back_to_retry_after_when_body_lacks_resets() {
        match ProviderError::from_http(429, "no body".into(), Some(Duration::from_secs(10))) {
            ProviderError::RateLimited { resets_at: Some(_) } => {}
            e => panic!("expected RateLimited with resets_at, got {e:?}"),
        }
    }

    #[test]
    fn from_http_quota_strings_promote_to_quota_exceeded_regardless_of_status() {
        let cases = [
            (500, "insufficient_quota"),
            (500, "billing_not_active"),
            (500, "your credit balance is too low"),
            (429, "request rate exceeded"),
        ];
        for (code, body) in cases {
            let err = ProviderError::from_http(code, body.into(), None);
            assert!(
                matches!(err, ProviderError::QuotaExceeded(_)),
                "expected QuotaExceeded for ({code}, {body:?})"
            );
        }
    }

    // ---- format_rate_limit ----

    #[test]
    fn format_rate_limit_without_resets_returns_plain_message() {
        assert_eq!(format_rate_limit(&None), "rate limited");
    }

    #[test]
    fn format_rate_limit_with_resets_includes_try_again_phrase() {
        let msg = format_rate_limit(&Some(1_700_000_000));
        assert!(msg.starts_with("rate limited"));
        assert!(msg.contains("try again at"));
    }

    // ---- format_epoch_local ----

    #[cfg(unix)]
    #[test]
    fn format_epoch_local_produces_month_day_year_time_ampm() {
        let s = format_epoch_local(1_700_000_000);
        // Loose checks (depends on local TZ): contains a month abbreviation, a year, and AM/PM.
        let has_month = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
        .iter()
        .any(|m| s.contains(m));
        assert!(has_month, "{s}");
        assert!(s.contains("2023"));
        assert!(s.contains("AM") || s.contains("PM"));
    }

    // ---- backoff_delay ----

    #[test]
    fn backoff_delay_doubles_each_attempt() {
        assert_eq!(backoff_delay(0), Duration::from_millis(500));
        assert_eq!(backoff_delay(1), Duration::from_millis(1000));
        assert_eq!(backoff_delay(2), Duration::from_millis(2000));
        assert_eq!(backoff_delay(3), Duration::from_millis(4000));
    }

    // ---- ProviderKind ----

    #[test]
    fn provider_kind_from_config_recognizes_known_types() {
        assert_eq!(ProviderKind::from_config("openai"), ProviderKind::OpenAi);
        assert_eq!(ProviderKind::from_config("codex"), ProviderKind::Codex);
        assert_eq!(
            ProviderKind::from_config("anthropic"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            ProviderKind::from_config("anthropic-compatible"),
            ProviderKind::AnthropicCompatible
        );
        assert_eq!(ProviderKind::from_config("copilot"), ProviderKind::Copilot);
        assert_eq!(
            ProviderKind::from_config("github-copilot"),
            ProviderKind::Copilot
        );
    }

    #[test]
    fn provider_kind_from_config_unknown_defaults_to_openai_compatible() {
        assert_eq!(
            ProviderKind::from_config("gibberish"),
            ProviderKind::OpenAiCompatible
        );
        assert_eq!(
            ProviderKind::from_config(""),
            ProviderKind::OpenAiCompatible
        );
    }

    #[test]
    fn provider_kind_detect_from_url_matches_host_substrings() {
        assert_eq!(
            ProviderKind::detect_from_url("https://api.kimi.com/coding"),
            ProviderKind::AnthropicCompatible
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.anthropic.com"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.openai.com/v1"),
            ProviderKind::OpenAi
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://chatgpt.com/backend"),
            ProviderKind::Codex
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://api.githubcopilot.com"),
            ProviderKind::Copilot
        );
        assert_eq!(
            ProviderKind::detect_from_url("https://local.host"),
            ProviderKind::OpenAiCompatible
        );
    }

    #[test]
    fn provider_kind_as_config_str_round_trips() {
        for k in [
            ProviderKind::OpenAi,
            ProviderKind::Codex,
            ProviderKind::Anthropic,
            ProviderKind::AnthropicCompatible,
            ProviderKind::Copilot,
            ProviderKind::OpenAiCompatible,
        ] {
            let s = k.as_config_str();
            assert_eq!(ProviderKind::from_config(s), k);
        }
    }

    #[test]
    fn default_reasoning_cycle_openai_compatible_excludes_max() {
        let cycle = ProviderKind::OpenAiCompatible.default_reasoning_cycle();
        assert!(!cycle.contains(&ReasoningEffort::Max));
        assert!(cycle.contains(&ReasoningEffort::Off));
        assert!(cycle.contains(&ReasoningEffort::High));
    }

    #[test]
    fn default_reasoning_cycle_other_kinds_include_max() {
        for k in [
            ProviderKind::OpenAi,
            ProviderKind::Codex,
            ProviderKind::Anthropic,
            ProviderKind::AnthropicCompatible,
            ProviderKind::Copilot,
        ] {
            assert!(k.default_reasoning_cycle().contains(&ReasoningEffort::Max));
        }
    }

    // ---- sanitize_tool_call_arguments ----

    #[test]
    fn sanitize_replaces_invalid_argument_string_with_empty_object_string() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tool_calls".into(),
            json!([{"function": {"arguments": "not json"}}]),
        );
        sanitize_tool_call_arguments(&mut obj);
        assert_eq!(obj["tool_calls"][0]["function"]["arguments"], "{}");
    }

    #[test]
    fn sanitize_keeps_valid_argument_strings() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tool_calls".into(),
            json!([{"function": {"arguments": "{\"a\":1}"}}]),
        );
        sanitize_tool_call_arguments(&mut obj);
        assert_eq!(obj["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
    }

    #[test]
    fn sanitize_is_noop_when_tool_calls_absent() {
        let mut obj = serde_json::Map::new();
        obj.insert("other".into(), json!("data"));
        sanitize_tool_call_arguments(&mut obj);
        assert_eq!(obj["other"], "data");
    }

    #[test]
    fn sanitize_ignores_arguments_that_are_not_strings() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tool_calls".into(),
            json!([{"function": {"arguments": {"a": 1}}}]),
        );
        sanitize_tool_call_arguments(&mut obj);
        // Non-string arguments are left untouched.
        assert!(obj["tool_calls"][0]["function"]["arguments"].is_object());
    }

    // ---- apply_response_format ----

    fn fmt() -> ResponseFormat {
        ResponseFormat {
            name: "out".into(),
            schema: json!({"type":"object"}),
        }
    }

    #[test]
    fn apply_response_format_openai_compatible_writes_response_format_json_schema() {
        let mut body = json!({});
        apply_response_format(&mut body, ProviderKind::OpenAiCompatible, &fmt());
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "out");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn apply_response_format_copilot_uses_same_shape_as_openai_compatible() {
        let mut body = json!({});
        apply_response_format(&mut body, ProviderKind::Copilot, &fmt());
        assert_eq!(body["response_format"]["json_schema"]["name"], "out");
    }

    #[test]
    fn apply_response_format_openai_writes_text_format_block() {
        let mut body = json!({});
        apply_response_format(&mut body, ProviderKind::OpenAi, &fmt());
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["name"], "out");
    }

    #[test]
    fn apply_response_format_codex_writes_text_format_block() {
        let mut body = json!({});
        apply_response_format(&mut body, ProviderKind::Codex, &fmt());
        assert_eq!(body["text"]["format"]["name"], "out");
    }

    #[test]
    fn apply_response_format_anthropic_modern_model_creates_output_config_format() {
        let mut body = json!({"model": "claude-sonnet-4-6"});
        apply_response_format(&mut body, ProviderKind::Anthropic, &fmt());
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn apply_response_format_anthropic_merges_into_existing_output_config_object() {
        let mut body = json!({"model": "claude-opus-4-6", "output_config": {"effort": "high"}});
        apply_response_format(&mut body, ProviderKind::Anthropic, &fmt());
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn apply_response_format_anthropic_legacy_model_does_not_write_field() {
        let mut body = json!({"model": "claude-3-5-sonnet"});
        apply_response_format(&mut body, ProviderKind::Anthropic, &fmt());
        assert!(body.get("output_config").is_none());
    }

    // ---- anthropic_supports_structured_output ----

    #[test]
    fn anthropic_supports_structured_output_recognizes_4_5_4_6_and_mythos() {
        assert!(anthropic_supports_structured_output("claude-haiku-4-5"));
        assert!(anthropic_supports_structured_output("claude-opus-4-6"));
        assert!(anthropic_supports_structured_output("mythos-1"));
    }

    #[test]
    fn anthropic_supports_structured_output_rejects_older_models() {
        assert!(!anthropic_supports_structured_output("claude-3-5-sonnet"));
        assert!(!anthropic_supports_structured_output("claude-3-7-sonnet"));
    }

    // ---- extract_json_title_slug ----

    #[test]
    fn extract_json_title_slug_pulls_json_from_surrounding_prose() {
        let raw = r#"sure! here it is: {"title":"Add OAuth","slug":"add-oauth"} thanks"#;
        let (t, s) = extract_json_title_slug(raw).unwrap();
        assert_eq!(t, "Add OAuth");
        assert_eq!(s, "add-oauth");
    }

    #[test]
    fn extract_json_title_slug_returns_none_without_braces() {
        assert!(extract_json_title_slug("no json").is_none());
    }

    #[test]
    fn extract_json_title_slug_returns_none_when_braces_misordered() {
        assert!(extract_json_title_slug("} {").is_none());
    }

    // ---- parse_title_and_slug ----

    #[test]
    fn parse_title_and_slug_returns_from_json_object() {
        let (t, s) = parse_title_and_slug(r#"{"title":"Fix login","slug":"fix-login"}"#);
        assert_eq!(t, "Fix login");
        assert_eq!(s, "fix-login");
    }

    #[test]
    fn parse_title_and_slug_caps_slug_at_five_words() {
        let raw = r#"{"title":"T","slug":"a-b-c-d-e-f-g"}"#;
        let (_, s) = parse_title_and_slug(raw);
        assert_eq!(s, "a-b-c-d-e");
    }

    #[test]
    fn parse_title_and_slug_derives_slug_from_title_when_missing() {
        let raw = r#"{"title":"Fix Login Button"}"#;
        let (_, s) = parse_title_and_slug(raw);
        assert_eq!(s, "fix-login-button");
    }

    #[test]
    fn parse_title_and_slug_falls_back_to_raw_text_when_no_json() {
        let (t, s) = parse_title_and_slug("just some text");
        assert_eq!(t, "just some text");
        assert_eq!(s, "just-some-text");
    }

    #[test]
    fn parse_title_and_slug_truncates_long_titles() {
        let long = "a".repeat(100);
        let (t, _) = parse_title_and_slug(&long);
        assert!(t.len() <= 64);
    }

    // ---- slugify ----

    #[test]
    fn slugify_lowercases_and_replaces_nonalnum_with_dashes() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
    }

    #[test]
    fn slugify_collapses_consecutive_separators() {
        assert_eq!(slugify("a---b   c"), "a-b-c");
    }

    #[test]
    fn slugify_empty_string_returns_empty() {
        assert_eq!(slugify(""), "");
    }

    // ---- normalize_short ----

    #[test]
    fn normalize_short_trims_quotes_and_collapses_whitespace() {
        assert_eq!(normalize_short("  \"hello  world\"  "), "hello world");
        assert_eq!(normalize_short("'single quoted'"), "single quoted");
    }

    #[test]
    fn normalize_short_truncates_to_64_bytes() {
        let s = normalize_short(&"x".repeat(200));
        assert!(s.len() <= 64);
    }

    // ---- messages_have_images ----

    #[test]
    fn messages_have_images_returns_false_when_only_text() {
        assert!(!messages_have_images(&[user_msg("hi")]));
    }

    #[test]
    fn messages_have_images_returns_true_when_user_has_image_part() {
        let m = Message {
            role: Role::User,
            content: Some(Content::Parts(vec![protocol::ContentPart::ImageUrl {
                url: "x".into(),
                label: None,
            }])),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        };
        assert!(messages_have_images(&[m]));
    }

    #[test]
    fn messages_have_images_ignores_assistant_and_system_roles() {
        let m = Message {
            role: Role::Assistant,
            content: Some(Content::Parts(vec![protocol::ContentPart::ImageUrl {
                url: "x".into(),
                label: None,
            }])),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
        };
        assert!(!messages_have_images(&[m]));
    }

    // ---- Provider basics ----

    fn http_client() -> Client {
        Client::new()
    }

    #[test]
    fn provider_new_strips_trailing_slashes_from_api_base() {
        let p = Provider::new(
            "https://x/".into(),
            "k".into(),
            "openai",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert_eq!(p.api_base, "https://x");
    }

    #[test]
    fn provider_new_resolves_kind_from_provider_type() {
        let p = Provider::new(
            "https://x".into(),
            "k".into(),
            "anthropic",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert_eq!(p.kind, ProviderKind::Anthropic);
    }

    #[test]
    fn provider_tool_calling_default_is_true() {
        let p = Provider::new(
            "".into(),
            "".into(),
            "openai",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert!(p.tool_calling());
    }

    #[test]
    fn provider_with_model_config_overrides_default() {
        let cfg = crate::config::ModelConfig {
            tool_calling: Some(false),
            ..Default::default()
        };
        let p = Provider::new(
            "".into(),
            "".into(),
            "openai",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        )
        .with_model_config(cfg);
        assert!(!p.tool_calling());
    }

    #[test]
    fn model_config_with_overrides_threads_each_field() {
        let cfg =
            crate::config::ModelConfig::default().with_overrides(&protocol::ModelConfigOverrides {
                temperature: Some(0.1),
                top_p: Some(0.2),
                top_k: Some(3),
                min_p: Some(0.4),
                repeat_penalty: Some(1.5),
            });
        assert_eq!(cfg.temperature, Some(0.1));
        assert_eq!(cfg.top_p, Some(0.2));
        assert_eq!(cfg.top_k, Some(3));
        assert_eq!(cfg.min_p, Some(0.4));
        assert_eq!(cfg.repeat_penalty, Some(1.5));
    }

    #[test]
    fn model_config_with_overrides_preserves_unset_override_fields() {
        let base = crate::config::ModelConfig {
            temperature: Some(0.5),
            top_p: Some(0.8),
            ..Default::default()
        };
        let cfg = base.with_overrides(&protocol::ModelConfigOverrides {
            top_k: Some(42),
            ..Default::default()
        });
        assert_eq!(cfg.temperature, Some(0.5));
        assert_eq!(cfg.top_p, Some(0.8));
        assert_eq!(cfg.top_k, Some(42));
    }

    #[test]
    fn provider_reset_turn_state_clears_to_none() {
        let p = Provider::new(
            "".into(),
            "".into(),
            "codex",
            http_client(),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        *p.turn_state.lock().unwrap() = Some("abc".into());
        p.reset_turn_state();
        assert!(p.turn_state.lock().unwrap().is_none());
    }

    // ---- ToolDefinition serialization ----

    #[test]
    fn tool_definition_serializes_with_function_type_tag() {
        let t = ToolDefinition::new(FunctionSchema {
            name: "f".into(),
            description: "d".into(),
            parameters: json!({}),
        });
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "f");
    }

    // ---- ParsedResponse::into_response ----

    #[test]
    fn parsed_into_response_propagates_fields() {
        let p = ParsedResponse {
            content: Some("c".into()),
            reasoning: Some("r".into()),
            tool_calls: vec![ToolCall::new(
                "id".into(),
                FunctionCall {
                    name: "n".into(),
                    arguments: "{}".into(),
                },
            )],
            usage: TokenUsage::default(),
        };
        let r = p.into_response(Some(12.5));
        assert_eq!(r.content.as_deref(), Some("c"));
        assert_eq!(r.reasoning_content.as_deref(), Some("r"));
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tokens_per_sec, Some(12.5));
    }
}
