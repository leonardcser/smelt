use crate::{
    anthropic, api_key_auth, apply_response_format, chat_completions, clamp_prompt_cache_key,
    codex, copilot, endpoint_url, kimi_code, openai, retry_delay_for, unix_now, ApiKeyAuth,
    CacheConfig, CancellationToken, ChatResponse, ChatResponseMetadata, ModelConfig, ProviderError,
    ProviderKind, ProviderStreamEvent, ResponseFormat, ToolDefinition, WireApi,
};
use protocol::{Message, ReasoningEffort};
use serde_json::Value;
use std::time::Duration;

/// HTTP client wrapper for provider chat integrations.
#[derive(Clone)]
pub struct ProviderClient {
    client: reqwest::Client,
}

impl ProviderClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.client
    }

    pub async fn fetch_context_window_anthropic(
        &self,
        api_base: &str,
        api_key: &str,
        model: &str,
    ) -> Option<u32> {
        let url = format!("{}/models/{}", api_base.trim_end_matches('/'), model);
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: Value = resp.json().await.ok()?;
        data["max_input_tokens"].as_u64().map(|v| v as u32)
    }

    pub async fn fetch_context_window_openai_compatible(
        &self,
        api_base: &str,
        api_key: &str,
        model: &str,
    ) -> Option<u32> {
        let url = format!("{}/models", api_base.trim_end_matches('/'));
        let mut req = self.client.get(&url);
        if !api_key.is_empty() {
            req = req.bearer_auth(api_key);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: Value = resp.json().await.ok()?;
        let models = data["data"].as_array()?;
        let entry = models.iter().find(|m| models_entry_matches(m, model))?;
        context_window_from_models_entry(entry)
    }
    pub async fn chat(
        &self,
        request: ChatRequest<'_>,
        opts: &ChatOptions<'_>,
    ) -> Result<ChatResponse, ProviderError> {
        let provider_kind = request.provider.kind();
        let mut wire_api = provider_kind.wire_api();
        let (url, mut body) = {
            let _perf = smelt_perf::perf::begin("provider:request:build_body");
            match provider_kind {
                ProviderKind::OpenAiCompatible => {
                    let url = endpoint_url(request.api_base, "chat/completions");
                    let body = chat_completions::build_body(
                        request.messages,
                        request.tools,
                        request.model,
                        request.effort,
                        request.config,
                    );
                    (url, body)
                }
                ProviderKind::OpenAi => {
                    let url = endpoint_url(request.api_base, "responses");
                    let body = openai::build_body(
                        request.messages,
                        request.tools,
                        request.model,
                        request.effort,
                        request.config,
                    );
                    (url, body)
                }
                ProviderKind::AnthropicCompatible
                | ProviderKind::Anthropic
                | ProviderKind::KimiCode => {
                    let url = endpoint_url(request.api_base, "messages");
                    let body = anthropic::build_body(
                        request.messages,
                        request.tools,
                        request.model,
                        request.effort,
                        request.config,
                        &request.cache,
                    );
                    (url, body)
                }
                ProviderKind::Codex => {
                    let url = endpoint_url(request.api_base, "responses");
                    let body = openai::build_codex_body(
                        request.messages,
                        request.tools,
                        request.model,
                        request.effort,
                        request.config,
                    );
                    (url, body)
                }
                ProviderKind::Copilot => {
                    let Some((tokens, wire)) = request.provider.copilot_transport() else {
                        return Err(ProviderError::InvalidResponse(
                            "copilot chat requires Copilot credentials".to_string(),
                        ));
                    };
                    wire_api = wire;
                    let url = wire.copilot_url(&tokens.api_base);
                    let body = copilot_body(
                        wire,
                        request.messages,
                        request.tools,
                        request.model,
                        request.effort,
                        request.config,
                        &request.cache,
                    );
                    (url, body)
                }
            }
        };

        if let Some(fmt) = request.response_format.as_ref() {
            apply_response_format(&mut body, wire_api, fmt);
        }
        if matches!(provider_kind, ProviderKind::OpenAi | ProviderKind::Codex) {
            if let Some(ref key) = request.cache.prompt_cache_key {
                body["prompt_cache_key"] = serde_json::json!(clamp_prompt_cache_key(key));
            }
        }
        apply_fast_mode(&mut body, provider_kind, request.fast_mode);

        let use_stream = opts.on_delta.is_some() || provider_kind == ProviderKind::Codex;
        if use_stream {
            body["stream"] = serde_json::json!(true);
            if provider_kind == ProviderKind::OpenAiCompatible {
                body["stream_options"] = serde_json::json!({"include_usage": true});
            }
        }

        let mut transport = HttpChatRequest::new(url, provider_kind, wire_api, request.model, body);
        transport.use_stream = use_stream;

        match request.provider.kind {
            ChatProviderKind::ApiKey { kind, api_key } => {
                transport.api_key = Some(api_key);
                transport.api_key_auth = api_key_auth(kind, api_key);
            }
            ChatProviderKind::KimiCode {
                access_token,
                headers,
            } => {
                transport.api_key = Some(access_token);
                transport.api_key_auth = api_key_auth(ProviderKind::KimiCode, access_token);
                transport.kimi_headers = Some(headers);
            }
            ChatProviderKind::Codex { tokens, turn_state } => {
                transport.headers = codex_request_headers(Some(tokens), turn_state);
            }
            ChatProviderKind::Copilot {
                tokens,
                initiator,
                has_images,
                ..
            } => {
                transport.headers = copilot_request_headers(tokens, initiator, has_images);
            }
            ChatProviderKind::None { .. } => {}
        }

        self.chat_http(transport, opts).await
    }
}

pub fn context_window_from_models_entry(entry: &Value) -> Option<u32> {
    // vLLM / SGLang advertise the raw `max_model_len`.
    if let Some(v) = entry["max_model_len"].as_u64() {
        return Some(v as u32);
    }
    // Moonshot / Kimi / OpenRouter / DeepSeek / Together AI.
    if let Some(v) = entry["context_length"].as_u64() {
        return Some(v as u32);
    }
    // Groq + a handful of others.
    if let Some(v) = entry["context_window"].as_u64() {
        return Some(v as u32);
    }
    if let Some(v) = entry["max_context_length"].as_u64() {
        return Some(v as u32);
    }
    // llama.cpp server: published as a launcher arg pair on `status.args`.
    if let Some(args) = entry["status"]["args"].as_array() {
        for i in 0..args.len().saturating_sub(1) {
            if args[i].as_str() == Some("--ctx-size") {
                return args[i + 1].as_str()?.parse::<u32>().ok();
            }
        }
    }
    None
}

/// Kimi puts the human-facing name in `display_name` and a stable backend slug
/// in `id`, so accept either.
pub fn models_entry_matches(entry: &Value, model: &str) -> bool {
    let eq = |field: &str| {
        entry[field]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(model))
    };
    eq("id") || eq("display_name")
}

impl ProviderClient {
    async fn chat_http(
        &self,
        request: HttpChatRequest<'_>,
        opts: &ChatOptions<'_>,
    ) -> Result<ChatResponse, ProviderError> {
        let max_retries = 9;
        let max_stream_retries = 5;

        struct AttemptEvent<'a> {
            attempt: u32,
            elapsed_ms: u64,
            result: Result<&'a ChatResponse, &'a ProviderError>,
            raw_response: Option<&'a serde_json::Value>,
            http_status: Option<u16>,
            error_body: Option<&'a str>,
        }

        let emit_attempt = |event: AttemptEvent<'_>| {
            if let Some(cb) = opts.on_attempt {
                cb(RequestAttemptInfo {
                    url: &request.url,
                    provider_kind: request.provider_kind,
                    model: request.model,
                    body: &request.body,
                    attempt: event.attempt.saturating_add(1),
                    elapsed_ms: event.elapsed_ms,
                    result: event.result,
                    http_status: event.http_status,
                    error_body: event.error_body,
                    raw_response: event.raw_response,
                });
            }
        };

        for attempt in 0..=max_retries {
            let request_start = std::time::Instant::now();
            let mut req = {
                let _perf = smelt_perf::perf::begin("provider:request:serialize_json");
                self.client.post(&request.url).json(&request.body)
            };
            if let Some(headers) = request.kimi_headers {
                req = kimi_code::apply_default_headers(req, headers);
            }
            for (name, value) in &request.headers {
                req = req.header(name, value);
            }
            if let (Some(api_key), Some(auth)) = (request.api_key, request.api_key_auth) {
                match auth {
                    ApiKeyAuth::Bearer => req = req.bearer_auth(api_key),
                    ApiKeyAuth::XApiKey => req = req.header("x-api-key", api_key),
                }
            }
            if request.anthropic_wire {
                req = req.header("anthropic-version", "2023-06-01");
            }

            let resp = tokio::select! {
                biased;
                _ = opts.cancel.cancelled() => {
                    let err = ProviderError::Cancelled;
                    emit_attempt(AttemptEvent {
                        attempt: attempt as u32,
                        elapsed_ms: request_start.elapsed().as_millis() as u64,
                        result: Err(&err),
                        raw_response: None,
                        http_status: None,
                        error_body: None,
                    });
                    return Err(err);
                }
                result = req.send() => match result {
                    Ok(r) => r,
                    Err(e) => {
                        let err = ProviderError::Network(e.to_string());
                        emit_attempt(AttemptEvent {
                            attempt: attempt as u32,
                            elapsed_ms: request_start.elapsed().as_millis() as u64,
                            result: Err(&err),
                            raw_response: None,
                            http_status: None,
                            error_body: None,
                        });
                        if attempt < max_retries {
                            let delay = crate::backoff_delay(attempt);
                            emit_retry(opts, delay, attempt);
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        return Err(err);
                    }
                }
            };

            if !resp.status().is_success() {
                let code = resp.status().as_u16();
                let retry_after = crate::parse_retry_after(&resp);
                let text = resp.text().await.unwrap_or_default();
                let err = ProviderError::from_http_at(code, text.clone(), retry_after, unix_now());
                emit_attempt(AttemptEvent {
                    attempt: attempt as u32,
                    elapsed_ms: request_start.elapsed().as_millis() as u64,
                    result: Err(&err),
                    raw_response: None,
                    http_status: Some(code),
                    error_body: Some(&text),
                });

                if attempt < max_retries {
                    if let Some(delay) = retry_delay_for(&err, attempt, retry_after, unix_now()) {
                        emit_retry(opts, delay, attempt);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                }
                return Err(err);
            }

            let http_status = Some(resp.status().as_u16());
            let codex_turn_state = resp
                .headers()
                .get("x-codex-turn-state")
                .and_then(|val| val.to_str().ok())
                .map(str::to_string);
            let noop_delta: &(dyn Fn(ProviderStreamEvent<'_>) + Send + Sync) = &|_| {};
            let on_delta = opts.on_delta.unwrap_or(noop_delta);

            let parsed_result = if request.use_stream {
                (
                    request
                        .wire_api
                        .read_stream(resp, opts.cancel, on_delta, unix_now())
                        .await,
                    None,
                    http_status,
                    None,
                )
            } else {
                match resp.text().await {
                    Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(data) => (
                            request.wire_api.parse_response(&data),
                            Some(data),
                            http_status,
                            None,
                        ),
                        Err(e) => (
                            Err(ProviderError::InvalidResponse(e.to_string())),
                            None,
                            http_status,
                            Some(text),
                        ),
                    },
                    Err(e) => (
                        Err(ProviderError::InvalidResponse(e.to_string())),
                        None,
                        http_status,
                        None,
                    ),
                }
            };

            let (parsed, raw, status) = match parsed_result {
                (Ok(parsed), raw, status, _) => (parsed, raw, status),
                (Err(err), _, status, error_body) if attempt < max_stream_retries => {
                    emit_attempt(AttemptEvent {
                        attempt: attempt as u32,
                        elapsed_ms: request_start.elapsed().as_millis() as u64,
                        result: Err(&err),
                        raw_response: None,
                        http_status: status,
                        error_body: error_body.as_deref(),
                    });
                    let Some(delay) = retry_delay_for(&err, attempt, None, unix_now()) else {
                        return Err(err);
                    };
                    emit_retry(opts, delay, attempt);
                    tokio::time::sleep(delay).await;
                    continue;
                }
                (Err(err), _, status, error_body) => {
                    emit_attempt(AttemptEvent {
                        attempt: attempt as u32,
                        elapsed_ms: request_start.elapsed().as_millis() as u64,
                        result: Err(&err),
                        raw_response: None,
                        http_status: status,
                        error_body: error_body.as_deref(),
                    });
                    return Err(err);
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
            let response = ChatResponse::from_parsed_with_metadata(
                parsed,
                tokens_per_sec,
                ChatResponseMetadata { codex_turn_state },
            );
            emit_attempt(AttemptEvent {
                attempt: attempt as u32,
                elapsed_ms: elapsed.as_millis() as u64,
                result: Ok(&response),
                raw_response: raw.as_ref(),
                http_status: status,
                error_body: None,
            });
            return Ok(response);
        }

        Err(ProviderError::MaxRetries)
    }
}

fn apply_fast_mode(body: &mut Value, provider_kind: ProviderKind, enabled: bool) {
    if provider_kind == ProviderKind::Codex && enabled {
        body["service_tier"] = serde_json::json!(codex::FAST_SERVICE_TIER);
    }
}

fn codex_request_headers(
    tokens: Option<&codex::CodexTokens>,
    turn_state: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers = vec![("Accept".to_string(), "text/event-stream".to_string())];
    if let Some(tokens) = tokens {
        headers.push((
            "Authorization".to_string(),
            format!("Bearer {}", tokens.access_token),
        ));
        headers.push(("originator".to_string(), "smelt".to_string()));
        if let Some(account_id) = tokens.account_id.as_deref() {
            headers.push(("ChatGPT-Account-ID".to_string(), account_id.to_string()));
        }
        if let Some(ts) = turn_state {
            headers.push(("x-codex-turn-state".to_string(), ts.to_string()));
        }
    }
    headers
}

fn copilot_request_headers(
    tokens: &copilot::CopilotTokens,
    initiator: CopilotInitiator,
    has_images: bool,
) -> Vec<(String, String)> {
    let mut headers = vec![(
        "Authorization".to_string(),
        format!("Bearer {}", tokens.access_token),
    )];
    headers.extend(
        copilot::base_headers()
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string())),
    );
    headers.push(("X-Initiator".to_string(), initiator.as_header().to_string()));
    headers.push((
        "Openai-Intent".to_string(),
        "conversation-edits".to_string(),
    ));
    if has_images {
        headers.push(("Copilot-Vision-Request".to_string(), "true".to_string()));
    }
    headers
}

fn copilot_chat_completions_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    let mut body = chat_completions::build_body(messages, tools, model, effort, config);
    if let Some(obj) = body.as_object_mut() {
        obj.remove("chat_template_kwargs");
        obj.remove("reasoning_effort");
    }
    body
}

fn copilot_body(
    wire: WireApi,
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
    cache: &CacheConfig,
) -> serde_json::Value {
    match wire {
        WireApi::ChatCompletions => {
            copilot_chat_completions_body(messages, tools, model, effort, config)
        }
        WireApi::OpenAiResponses => {
            let mut body = openai::build_body(messages, tools, model, effort, config);
            body["store"] = serde_json::json!(false);
            body
        }
        WireApi::AnthropicMessages => {
            anthropic::build_body(messages, tools, model, effort, config, cache)
        }
    }
}

pub struct ChatRequest<'a> {
    pub provider: ChatProvider<'a>,
    pub api_base: &'a str,
    pub model: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDefinition],
    pub effort: ReasoningEffort,
    pub config: &'a ModelConfig,
    pub cache: CacheConfig,
    pub response_format: Option<ResponseFormat>,
    pub fast_mode: bool,
}

#[derive(Clone, Copy)]
pub struct ChatProvider<'a> {
    kind: ChatProviderKind<'a>,
}

#[derive(Clone, Copy)]
enum ChatProviderKind<'a> {
    ApiKey {
        kind: ProviderKind,
        api_key: &'a str,
    },
    KimiCode {
        access_token: &'a str,
        headers: &'a kimi_code::KimiHeaders,
    },
    Codex {
        tokens: &'a codex::CodexTokens,
        turn_state: Option<&'a str>,
    },
    Copilot {
        tokens: &'a copilot::CopilotTokens,
        wire: WireApi,
        initiator: CopilotInitiator,
        has_images: bool,
    },
    None {
        kind: ProviderKind,
    },
}

impl<'a> ChatProvider<'a> {
    pub fn api_key(kind: ProviderKind, api_key: &'a str) -> Self {
        Self {
            kind: ChatProviderKind::ApiKey { kind, api_key },
        }
    }

    pub fn kimi_code(access_token: &'a str, headers: &'a kimi_code::KimiHeaders) -> Self {
        Self {
            kind: ChatProviderKind::KimiCode {
                access_token,
                headers,
            },
        }
    }

    pub fn codex(tokens: &'a codex::CodexTokens, turn_state: Option<&'a str>) -> Self {
        Self {
            kind: ChatProviderKind::Codex { tokens, turn_state },
        }
    }

    pub fn copilot(
        tokens: &'a copilot::CopilotTokens,
        model: &str,
        model_metadata: Option<&copilot::CopilotModel>,
        initiator: CopilotInitiator,
        has_images: bool,
    ) -> Self {
        Self {
            kind: ChatProviderKind::Copilot {
                tokens,
                wire: copilot::select_wire_api(model, model_metadata),
                initiator,
                has_images,
            },
        }
    }

    pub fn none(kind: ProviderKind) -> Self {
        Self {
            kind: ChatProviderKind::None { kind },
        }
    }

    fn kind(&self) -> ProviderKind {
        match self.kind {
            ChatProviderKind::ApiKey { kind, .. } | ChatProviderKind::None { kind } => kind,
            ChatProviderKind::KimiCode { .. } => ProviderKind::KimiCode,
            ChatProviderKind::Codex { .. } => ProviderKind::Codex,
            ChatProviderKind::Copilot { .. } => ProviderKind::Copilot,
        }
    }

    fn copilot_transport(&self) -> Option<(&'a copilot::CopilotTokens, WireApi)> {
        match self.kind {
            ChatProviderKind::Copilot { tokens, wire, .. } => Some((tokens, wire)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopilotInitiator {
    User,
    Agent,
}

impl CopilotInitiator {
    fn as_header(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
        }
    }
}

#[derive(Clone, Default)]
pub struct ChatRequestOptions {
    pub response_format: Option<ResponseFormat>,
    pub cache: CacheConfig,
    pub fast_mode: bool,
}

pub struct ChatOptions<'a> {
    pub cancel: &'a CancellationToken,
    pub on_retry: Option<&'a (dyn Fn(Duration, u32) + Send + Sync)>,
    pub on_delta: Option<&'a (dyn Fn(ProviderStreamEvent<'_>) + Send + Sync)>,
    pub on_attempt: Option<&'a (dyn Fn(RequestAttemptInfo<'_>) + Send + Sync)>,
}

struct HttpChatRequest<'a> {
    pub url: String,
    pub provider_kind: ProviderKind,
    pub wire_api: WireApi,
    pub model: &'a str,
    pub body: serde_json::Value,
    pub use_stream: bool,
    pub headers: Vec<(String, String)>,
    pub kimi_headers: Option<&'a kimi_code::KimiHeaders>,
    pub api_key: Option<&'a str>,
    pub api_key_auth: Option<ApiKeyAuth>,
    pub anthropic_wire: bool,
}

impl<'a> HttpChatRequest<'a> {
    fn new(
        url: String,
        provider_kind: ProviderKind,
        wire_api: WireApi,
        model: &'a str,
        body: serde_json::Value,
    ) -> Self {
        Self {
            url,
            provider_kind,
            wire_api,
            model,
            body,
            use_stream: false,
            headers: Vec::new(),
            kimi_headers: None,
            api_key: None,
            api_key_auth: None,
            anthropic_wire: wire_api.is_anthropic(),
        }
    }
}

impl<'a> ChatOptions<'a> {
    pub fn new(cancel: &'a CancellationToken) -> Self {
        Self {
            cancel,
            on_retry: None,
            on_delta: None,
            on_attempt: None,
        }
    }
}

pub fn emit_retry(opts: &ChatOptions<'_>, delay: Duration, attempt: usize) {
    if let Some(f) = opts.on_retry {
        f(delay, (attempt + 1) as u32);
    }
}

/// Snapshot of one provider request attempt, delivered to request audit hooks
/// after the attempt finishes.
pub struct RequestAttemptInfo<'a> {
    pub url: &'a str,
    pub provider_kind: ProviderKind,
    pub model: &'a str,
    pub body: &'a serde_json::Value,
    /// One-based number of this attempt within the logical request.
    pub attempt: u32,
    pub elapsed_ms: u64,
    pub result: Result<&'a ChatResponse, &'a ProviderError>,
    /// HTTP status returned by the provider, when a response was received.
    pub http_status: Option<u16>,
    /// Raw HTTP error body, capped by the request log writer.
    pub error_body: Option<&'a str>,
    /// Verbatim non-streaming response body, available only for non-streaming
    /// requests.
    pub raw_response: Option<&'a serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Content, Role};
    use serde_json::json;

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: Some(Content::Text(text.into())),
            reasoning_content: None,
            reasoning_details: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            tool_metadata: None,
        }
    }

    fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn emit_retry_reports_one_based_retry_count() {
        let cancel = CancellationToken::new();
        let seen = std::sync::Mutex::new(Vec::new());
        let on_retry = |delay, attempt| seen.lock().unwrap().push((delay, attempt));
        let mut opts = ChatOptions::new(&cancel);
        opts.on_retry = Some(&on_retry);

        emit_retry(&opts, Duration::from_secs(2), 0);
        emit_retry(&opts, Duration::from_secs(4), 1);

        assert_eq!(
            *seen.lock().unwrap(),
            vec![(Duration::from_secs(2), 1), (Duration::from_secs(4), 2)]
        );
    }

    async fn capture_codex_request(fast_mode: bool) -> (Value, Vec<u32>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            let header_end = loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "request ended before headers");
                request.extend_from_slice(&chunk[..read]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert_eq!(
                headers.lines().next(),
                Some("POST /codex/responses HTTP/1.1")
            );
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while request.len() < header_end + content_length {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "request ended before body");
                request.extend_from_slice(&chunk[..read]);
            }
            let body =
                serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap();
            let event = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                event.len(),
                event
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            body
        });

        let tokens = codex::CodexTokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: u64::MAX,
            account_id: Some("acct".into()),
            last_refresh: 0,
        };
        let messages = [user_msg("hi")];
        let cancel = CancellationToken::new();
        let attempts = std::sync::Mutex::new(Vec::new());
        {
            let on_attempt = |info: RequestAttemptInfo<'_>| {
                attempts.lock().unwrap().push(info.attempt);
            };
            let mut opts = ChatOptions::new(&cancel);
            opts.on_attempt = Some(&on_attempt);
            ProviderClient::new(reqwest::Client::new())
                .chat(
                    ChatRequest {
                        provider: ChatProvider::codex(&tokens, None),
                        api_base: &format!("http://{addr}/codex"),
                        model: "gpt-test",
                        messages: &messages,
                        tools: &[],
                        effort: ReasoningEffort::Off,
                        config: &ModelConfig::default(),
                        cache: CacheConfig::default(),
                        response_format: None,
                        fast_mode,
                    },
                    &opts,
                )
                .await
                .unwrap();
        }
        (
            server.await.unwrap(),
            attempts.into_inner().expect("attempt callback mutex"),
        )
    }

    #[tokio::test]
    async fn codex_http_request_normalizes_endpoint_and_emits_one_based_attempts() {
        let (fast_body, fast_attempts) = capture_codex_request(true).await;
        assert_eq!(fast_body["service_tier"], "priority");
        assert_eq!(fast_attempts, vec![1]);

        let (standard_body, standard_attempts) = capture_codex_request(false).await;
        assert!(standard_body.get("service_tier").is_none());
        assert_eq!(standard_attempts, vec![1]);
    }

    #[test]
    fn codex_request_headers_include_auth_originator_and_turn_state() {
        let tokens = codex::CodexTokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: 123,
            account_id: Some("acct".into()),
            last_refresh: 0,
        };
        let headers = codex_request_headers(Some(&tokens), Some("turn"));

        assert_eq!(header_value(&headers, "Accept"), Some("text/event-stream"));
        assert_eq!(
            header_value(&headers, "Authorization"),
            Some("Bearer access")
        );
        assert_eq!(header_value(&headers, "ChatGPT-Account-ID"), Some("acct"));
        assert_eq!(header_value(&headers, "originator"), Some("smelt"));
        assert_eq!(header_value(&headers, "x-codex-turn-state"), Some("turn"));
    }

    #[test]
    fn copilot_chat_completions_body_drops_local_reasoning_fields() {
        let body = copilot_body(
            WireApi::ChatCompletions,
            &[user_msg("hi")],
            &[],
            "gpt-4.1",
            ReasoningEffort::High,
            &ModelConfig::default(),
            &CacheConfig::default(),
        );

        assert_eq!(body["model"], "gpt-4.1");
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn context_window_from_models_entry_reads_common_provider_fields() {
        assert_eq!(
            context_window_from_models_entry(&json!({"max_model_len": 32768})),
            Some(32768)
        );
        assert_eq!(
            context_window_from_models_entry(&json!({"context_length": 256000})),
            Some(256000)
        );
        assert_eq!(
            context_window_from_models_entry(&json!({"context_window": 131072})),
            Some(131072)
        );
        assert_eq!(
            context_window_from_models_entry(&json!({"status": {"args": ["--ctx-size", "4096"]}})),
            Some(4096)
        );
        assert_eq!(context_window_from_models_entry(&json!({})), None);
    }

    #[test]
    fn models_entry_matches_id_or_display_name_case_insensitively() {
        assert!(models_entry_matches(
            &json!({"id": "kimi-for-coding", "display_name": "Kimi K2.6"}),
            "KIMI-FOR-CODING"
        ));
        assert!(models_entry_matches(
            &json!({"id": "backend-slug", "display_name": "Kimi K2.6"}),
            "kimi k2.6"
        ));
        assert!(!models_entry_matches(
            &json!({"id": "a", "display_name": "b"}),
            "c"
        ));
    }
}
