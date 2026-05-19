use crate::log;
use crate::provider::{self, ChatOptions, FunctionSchema, Provider, ProviderError, ToolDefinition};
use crate::tools::{ToolContext, ToolDispatcher, ToolResult};
#[cfg(test)]
use crate::ModelConfig;
use crate::{ApiConfig, EngineConfig};
use protocol::Decision;
use protocol::{
    AgentMode, AskModel, Content, EngineAskError, EngineAskErrorKind, EngineEvent, Message,
    ReasoningEffort, Role, ToolOutcome, TurnMeta, UiCommand,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::Instant;
use tokio::sync::mpsc;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Main engine task. Runs in a tokio::spawn and processes commands/events.
pub(crate) async fn engine_task(
    mut config: EngineConfig,
    dispatcher: Box<dyn crate::tools::ToolDispatcher>,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    host_tx: mpsc::UnboundedSender<crate::host::HostCall>,
) {
    // Phase A: `host_tx` is plumbed into each `Turn` for provider
    // request/response hooks. Phase B migrates the existing
    // ToolDispatch / ToolHooks / RequestPermission RPCs onto it and
    // removes their `UiCommand::*Response` variants.
    // Some openai-compatible endpoints gate on User-Agent (e.g. api.kimi.com).
    // Per-request header() calls (Copilot, Codex) still override this.
    let client = reqwest::Client::builder()
        .user_agent(concat!("smelt/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    crate::catalog::spawn_fetch(client.clone());

    let _ = event_tx.send(EngineEvent::Ready);

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::StartTurn(payload) => {
                        let protocol::StartTurnPayload {
                            turn_id, content: input_content, mode, model, reasoning_effort,
                            history, api_base, api_key, session_id, session_dir: _,
                            model_config_overrides, permission_overrides: _,
                            system_prompt: tui_system_prompt, tools,
                        } = *payload;

                        let provider = build_provider(
                            &config.api, &client,
                            api_base.as_deref(), api_key.as_deref(),
                            model_config_overrides.as_ref(),
                            std::sync::Arc::clone(&config.clock),
                        );
                        let system_prompt = tui_system_prompt
                            .or_else(|| config.system_prompt_override.clone())
                            .unwrap_or_else(|| {
                                crate::build_system_prompt_full(
                                    &config.cwd,
                                    config.instructions.as_deref(),
                                    config.skill_section.as_deref(),
                                )
                            });
                        let mut turn = Turn {
                            provider,
                            dispatcher: &*dispatcher,
                            cmd_rx: &mut cmd_rx,
                            event_tx: &event_tx,
                            host_tx: &host_tx,
                            config: &config,
                            http_client: &client,
                            cancel: crate::cancel::CancellationToken::new(),
                            messages: Vec::new(),
                            mode,
                            reasoning_effort,
                            turn_id,
                            model,
                            system_prompt,
                            tools,
                            session_id,
                            started_at: config.clock.instant_now(),
                            tps_samples: Vec::new(),
                            tool_elapsed: HashMap::new(),
                        };
                        turn.run(input_content, history).await;
                    }
                    UiCommand::EngineAsk {
                        id,
                        system,
                        messages,
                        model,
                        response_format,
                        reasoning_effort,
                        tools,
                        session_id,
                    } => {
                        spawn_engine_ask(
                            &config,
                            &client,
                            AskTask {
                                id,
                                system,
                                messages,
                                model,
                                response_format,
                                reasoning_effort,
                                tools,
                                session_id,
                            },
                            &event_tx,
                        );
                    }
                    UiCommand::SetModel { model, api_base, api_key, provider_type } => {
                        config.api.base = api_base;
                        config.api.key = api_key;
                        config.api.provider_type = provider_type;
                        config.model = model;
                    }
                    UiCommand::ReloadAgentConfig {
                        instructions,
                        skill_section,
                        system_prompt_override,
                    } => {
                        config.instructions = instructions;
                        config.skill_section = skill_section;
                        config.system_prompt_override = system_prompt_override;
                    }
                    _ => {} // Steer, Cancel, etc. only relevant during a turn
                }
            }
            else => break,
        }
    }

    let _ = event_tx.send(EngineEvent::Shutdown { reason: None });
}

/// Resolve an `AskModel` override (or the primary) into `(ApiConfig, model_name)`.
fn resolve_ask_target(config: &EngineConfig, model: Option<AskModel>) -> (ApiConfig, String) {
    match model {
        Some(m) => (
            ApiConfig {
                base: m.api_base,
                key: m.api_key,
                key_env: String::new(),
                provider_type: m.provider_type,
                model_config: config.api.model_config.clone(),
            },
            m.model,
        ),
        None => (config.api.clone(), config.model.clone()),
    }
}

/// One pending out-of-band LLM call. Mirrors the fields plumbed through
/// `UiCommand::EngineAsk`; bundles them so the spawn surface stays a
/// two-arg function (config + task) instead of an ever-growing arg list.
pub(crate) struct AskTask {
    pub id: u64,
    pub system: String,
    pub messages: Vec<protocol::Message>,
    pub model: Option<AskModel>,
    pub response_format: Option<protocol::AskResponseFormat>,
    pub reasoning_effort: ReasoningEffort,
    /// Optional tool list. When non-empty AND matching the main session's
    /// tools byte-for-byte, the request reuses the main session's
    /// Anthropic prefix cache.
    pub tools: Vec<protocol::ToolDef>,
    /// Session id forwarded as `prompt_cache_key` to OpenAI / Codex so
    /// the EngineAsk hits the same cache shard as the main turn.
    pub session_id: String,
}

fn spawn_engine_ask(
    config: &EngineConfig,
    client: &reqwest::Client,
    task: AskTask,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    let AskTask {
        id,
        system,
        mut messages,
        model,
        response_format,
        reasoning_effort,
        tools,
        session_id,
    } = task;
    let (api, model_name) = resolve_ask_target(config, model);
    let provider = build_provider(
        &api,
        client,
        None,
        None,
        None,
        std::sync::Arc::clone(&config.clock),
    );
    let pricing = PricingContext::from_api(&api);
    let tx = event_tx.clone();
    let cache_ttl_long = config.cache_ttl_long;
    tokio::spawn(async move {
        let cancel = crate::cancel::CancellationToken::new();
        messages.insert(0, protocol::Message::system(&system));

        let mut opts = ChatOptions::new(&cancel);
        opts.cache = provider.default_cache_config(cache_ttl_long);
        if !session_id.is_empty() {
            opts.cache.prompt_cache_key = Some(session_id);
        }
        if let Some(fmt) = response_format {
            opts.response_format = Some(crate::provider::ResponseFormat {
                name: fmt.name,
                schema: fmt.schema,
            });
        }

        // Reuse the main-turn tool format (sorted by name) so an EngineAsk
        // that inherits the session's tool list produces a byte-identical
        // tools section and hits the same Anthropic prefix cache slot.
        let mut tool_defs: Vec<ToolDefinition> = tools
            .into_iter()
            .map(|t| {
                ToolDefinition::new(FunctionSchema {
                    name: t.name,
                    description: t.description,
                    parameters: t.parameters,
                })
            })
            .collect();
        tool_defs.sort_by(|a, b| a.function.name.cmp(&b.function.name));

        let result = provider
            .chat(&messages, &tool_defs, &model_name, reasoning_effort, &opts)
            .await;

        match result {
            Ok(resp) => {
                pricing.emit(&tx, &model_name, resp.usage);
                let _ = tx.send(EngineEvent::EngineAskResponse {
                    id,
                    content: resp.content.unwrap_or_default(),
                    error: None,
                });
            }
            Err(e) => {
                let kind = classify_provider_error(&e);
                let message = e.to_string().replace('\n', " ");
                let _ = tx.send(EngineEvent::EngineAskResponse {
                    id,
                    content: String::new(),
                    error: Some(EngineAskError { kind, message }),
                });
            }
        }
    });
}

/// Map a provider error to a stable `EngineAskErrorKind` for Lua callers.
fn classify_provider_error(e: &ProviderError) -> EngineAskErrorKind {
    match e {
        ProviderError::Cancelled => EngineAskErrorKind::Cancelled,
        ProviderError::QuotaExceeded(_) => EngineAskErrorKind::Quota,
        ProviderError::Network(_) => EngineAskErrorKind::Network,
        ProviderError::RateLimited { .. } => EngineAskErrorKind::RateLimited,
        ProviderError::Server { .. } => {
            if is_context_window_error(e) {
                EngineAskErrorKind::ContextWindow
            } else {
                EngineAskErrorKind::Network
            }
        }
        ProviderError::InvalidResponse(_) => {
            if is_context_window_error(e) {
                EngineAskErrorKind::ContextWindow
            } else {
                EngineAskErrorKind::InvalidResponse
            }
        }
        ProviderError::Auth(_) | ProviderError::NotFound(_) | ProviderError::MaxRetries => {
            EngineAskErrorKind::Other
        }
    }
}

/// True when the error indicates the model's context window was exceeded.
/// Mirrors the body-substring check previously living in `compact.rs`.
fn is_context_window_error(e: &ProviderError) -> bool {
    let body = match e {
        ProviderError::InvalidResponse(b) => b.as_str(),
        ProviderError::Server { body, .. } => body.as_str(),
        _ => return false,
    };
    let lower = body.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("too many tokens")
}

fn build_provider(
    api: &ApiConfig,
    client: &reqwest::Client,
    api_base: Option<&str>,
    api_key: Option<&str>,
    model_overrides: Option<&protocol::ModelConfigOverrides>,
    clock: std::sync::Arc<dyn crate::clock::Clock>,
) -> Provider {
    let model_config = match model_overrides {
        Some(o) => api.model_config.clone().with_overrides(o),
        None => api.model_config.clone(),
    };
    Provider::new(
        api_base.unwrap_or(&api.base).to_string(),
        api_key.unwrap_or(&api.key).to_string(),
        &api.provider_type,
        client.clone(),
        clock,
    )
    .with_model_config(model_config)
}

// ── Turn ────────────────────────────────────────────────────────────────────

struct ToolSlot<'a> {
    tc: &'a protocol::ToolCall,
    args: HashMap<String, Value>,
    confirm_msg: Option<String>,
    start: Instant,
}

struct ToolExecutionPlan<'a> {
    slots: Vec<ToolSlot<'a>>,
    ready: Vec<usize>,
    pending_perms: Vec<(usize, u64)>,
    pending_tools: Vec<(u64, String, Instant)>,
    sequential_tools: Vec<(&'a protocol::ToolCall, HashMap<String, Value>, Instant)>,
    pending_tool_hooks: Vec<(u64, PendingToolCall<'a>)>,
    pending_tool_perms: Vec<(u64, PendingToolCall<'a>)>,
}

struct PendingToolCall<'a> {
    tc: &'a protocol::ToolCall,
    args: HashMap<String, Value>,
    tool_start: Instant,
    is_sequential: bool,
}

struct Turn<'a> {
    provider: Provider,
    dispatcher: &'a dyn ToolDispatcher,
    cmd_rx: &'a mut mpsc::UnboundedReceiver<UiCommand>,
    event_tx: &'a mpsc::UnboundedSender<EngineEvent>,
    /// Host-callback channel. Used for `smelt.provider.middleware` round-trips
    /// (and, in Phase B, the legacy `ToolDispatch`/`ToolHooks`/`Permission`
    /// RPCs once they're migrated off `EngineEvent::*Request`/`UiCommand::*Response`).
    host_tx: &'a mpsc::UnboundedSender<crate::host::HostCall>,
    config: &'a EngineConfig,
    http_client: &'a reqwest::Client,
    cancel: crate::cancel::CancellationToken,
    messages: Vec<Message>,
    mode: AgentMode,
    reasoning_effort: ReasoningEffort,
    turn_id: u64,
    model: String,
    system_prompt: String,
    tools: Vec<protocol::ToolDef>,
    /// Stable per-session identifier sent as OpenAI's `prompt_cache_key`
    /// to anchor cache routing across all turns in this session.
    session_id: String,
    started_at: Instant,
    tps_samples: Vec<f64>,
    tool_elapsed: HashMap<String, u64>,
}

impl<'a> Turn<'a> {
    fn emit(&self, event: EngineEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Fire a `HostCall` and await its `oneshot::Sender<Reply>`. Returns
    /// `None` if the host dropped the reply (channel closed) — callers
    /// treat that as "no mutation, proceed with the original value".
    async fn host_call<Reply, F>(&self, build: F) -> Option<Reply>
    where
        F: FnOnce(tokio::sync::oneshot::Sender<Reply>) -> crate::host::HostCall,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.host_tx.send(build(tx)).is_err() {
            return None;
        }
        rx.await.ok()
    }

    /// Run `smelt.provider.middleware{on_response=...}` hooks against
    /// the assembled assistant `Message`. Returns the replacement when
    /// any hook produced one; otherwise the original.
    async fn apply_response_hooks(&self, message: Message) -> Message {
        match self
            .host_call(|reply| crate::host::HostCall::ProviderResponse {
                message: message.clone(),
                reply,
            })
            .await
        {
            Some(Some(replacement)) => replacement,
            _ => message,
        }
    }

    /// Milliseconds since `start` measured against the injected clock.
    fn elapsed_ms_since(&self, start: Instant) -> u64 {
        self.config
            .clock
            .instant_now()
            .duration_since(start)
            .as_millis() as u64
    }

    /// Scrubs user/tool content when redact_secrets is enabled; passes assistant/system through.
    fn push_message(&mut self, mut msg: Message) {
        if self.config.redact_secrets && matches!(msg.role, Role::User | Role::Tool) {
            crate::redact::redact_message(&mut msg);
        }
        self.messages.push(msg);
    }

    /// Rebuilds the system prompt after `/reload`. Mode changes alone
    /// don't reach here: the base prompt is mode-agnostic so its bytes
    /// stay stable across `/mode` switches, preserving the cache.
    fn regenerate_system_prompt(&mut self) {
        let new = self
            .config
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| {
                crate::build_system_prompt_full(
                    &self.config.cwd,
                    self.config.instructions.as_deref(),
                    self.config.skill_section.as_deref(),
                )
            });
        self.system_prompt = new;
        if let Some(first) = self.messages.first_mut() {
            if matches!(first.role, Role::System) {
                *first = Message::system(&self.system_prompt);
            }
        }
    }

    fn emit_messages_snapshot(&self) {
        let mut messages = self.messages.clone();
        if messages
            .first()
            .is_some_and(|m| matches!(m.role, Role::System))
        {
            messages.remove(0);
        }
        self.emit(EngineEvent::Messages {
            turn_id: self.turn_id,
            messages,
        });
    }

    fn commit_partial_assistant(&mut self, text: String, reasoning: String) {
        let content = if text.trim().is_empty() {
            None
        } else {
            Some(Content::text(text))
        };
        let reasoning = if reasoning.trim().is_empty() {
            None
        } else {
            Some(reasoning)
        };
        if content.is_some() || reasoning.is_some() {
            self.messages
                .push(Message::assistant(content, reasoning, None));
        }
    }

    fn emit_turn_complete(&mut self, interrupted: bool) {
        let meta = self.build_meta(interrupted);
        self.messages.remove(0);
        let msgs = std::mem::take(&mut self.messages);
        self.emit(EngineEvent::TurnComplete {
            turn_id: self.turn_id,
            messages: msgs,
            meta: Some(meta),
        });
    }

    fn build_meta(&self, interrupted: bool) -> TurnMeta {
        let avg_tps = if self.tps_samples.is_empty() {
            None
        } else {
            let sum: f64 = self.tps_samples.iter().sum();
            Some(sum / self.tps_samples.len() as f64)
        };
        let elapsed = self
            .config
            .clock
            .instant_now()
            .duration_since(self.started_at);
        TurnMeta {
            elapsed_ms: elapsed.as_millis() as u64,
            avg_tps,
            interrupted,
            tool_elapsed: self.tool_elapsed.clone(),
        }
    }

    fn apply_model_change(
        &mut self,
        model: String,
        api_base: String,
        api_key: String,
        provider_type: String,
    ) {
        self.model = model;
        self.provider = Provider::new(
            api_base,
            api_key,
            &provider_type,
            self.http_client.clone(),
            std::sync::Arc::clone(&self.config.clock),
        )
        .with_model_config(self.config.api.model_config.clone());
    }

    /// Handle a command that can be processed regardless of turn state.
    fn handle_background_cmd(&self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::EngineAsk {
                id,
                system,
                messages,
                model,
                response_format,
                reasoning_effort,
                tools,
                session_id,
            } => {
                spawn_engine_ask(
                    self.config,
                    self.http_client,
                    AskTask {
                        id,
                        system,
                        messages,
                        model,
                        response_format,
                        reasoning_effort,
                        tools,
                        session_id,
                    },
                    self.event_tx,
                );
                true
            }
            _ => false,
        }
    }

    fn handle_turn_cmd(&mut self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::Steer { text } => {
                self.emit(EngineEvent::Steered {
                    text: text.clone(),
                    count: 1,
                });
                self.push_message(Message::user(Content::text(text)));
                self.emit_messages_snapshot();
                true
            }
            UiCommand::Unsteer { count } => {
                for _ in 0..count {
                    if let Some(pos) = self.messages.iter().rposition(|m| m.role == Role::User) {
                        self.messages.remove(pos);
                    }
                }
                self.emit_messages_snapshot();
                true
            }
            UiCommand::SetAgentMode {
                mode,
                system_prompt,
                tools,
            } => {
                self.mode = mode;
                if let Some(prompt) = system_prompt {
                    self.system_prompt = prompt;
                    if let Some(first) = self.messages.first_mut() {
                        if matches!(first.role, Role::System) {
                            *first = Message::system(&self.system_prompt);
                        }
                    }
                } else {
                    self.regenerate_system_prompt();
                }
                if let Some(tools) = tools {
                    self.tools = tools;
                }
                true
            }
            UiCommand::SetReasoningEffort { effort } => {
                self.reasoning_effort = effort;
                true
            }
            UiCommand::SetModel {
                model,
                api_base,
                api_key,
                provider_type,
            } => {
                self.apply_model_change(model, api_base, api_key, provider_type);
                true
            }
            UiCommand::Cancel => {
                self.cancel.cancel();
                true
            }
            other => self.handle_background_cmd(other),
        }
    }

    async fn run(&mut self, content: Content, history: Vec<Message>) {
        self.provider.reset_turn_state();
        self.messages = Vec::with_capacity(history.len() + 2);
        self.messages.push(Message::system(&self.system_prompt));
        self.messages.extend(history);

        if !content.is_empty() {
            self.push_message(Message::user(content));
        }
        self.emit_messages_snapshot();

        let mut first = true;
        let mut empty_retries: u8 = 0;
        const MAX_EMPTY_RETRIES: u8 = 2;

        loop {
            if !first {
                self.drain_commands();
            }
            first = false;

            self.regenerate_system_prompt();

            // Sorted by name so the request prefix stays byte-identical
            // across turns. Anything that reorders tools busts the cache.
            let tool_defs: Vec<ToolDefinition> = if self.provider.tool_calling() {
                let mut defs: Vec<ToolDefinition> = self
                    .dispatcher
                    .definitions()
                    .into_iter()
                    .filter(|d| {
                        self.dispatcher
                            .is_visible(d.function.name.as_str(), self.mode)
                    })
                    .collect();
                // Plugin tools with `override_core` shadow the same-named core tool.
                let overridden: std::collections::HashSet<&str> = self
                    .tools
                    .iter()
                    .filter(|pt| pt.override_core)
                    .map(|pt| pt.name.as_str())
                    .collect();
                if !overridden.is_empty() {
                    defs.retain(|d| !overridden.contains(d.function.name.as_str()));
                }
                for pt in &self.tools {
                    if let Some(ref modes) = pt.modes {
                        if !modes.contains(&self.mode) {
                            continue;
                        }
                    }
                    defs.push(ToolDefinition::new(FunctionSchema {
                        name: pt.name.clone(),
                        description: pt.description.clone(),
                        parameters: pt.parameters.clone(),
                    }));
                }
                defs.sort_by(|a, b| a.function.name.cmp(&b.function.name));
                defs
            } else {
                Vec::new()
            };

            if self.cancel.is_cancelled() {
                self.emit_turn_complete(true);
                return;
            }

            // Fire `smelt.provider.middleware{on_request=...}` hooks
            // and accept any mutation they apply to the message slice.
            if let Some(Some(replacement)) = self
                .host_call(|reply| crate::host::HostCall::ProviderRequest {
                    messages: self.messages.clone(),
                    reply,
                })
                .await
            {
                self.messages = replacement;
            }

            let (result, partial_text, partial_reasoning) = self.call_llm(&tool_defs).await;
            let (resp, had_injected) = match result {
                Ok(r) => r,
                Err(ProviderError::Cancelled) => {
                    self.commit_partial_assistant(partial_text, partial_reasoning);
                    self.emit_turn_complete(true);
                    return;
                }
                Err(ProviderError::QuotaExceeded(ref body)) => {
                    log::entry(
                        log::Level::Warn,
                        "agent_stop",
                        &serde_json::json!({"reason": "quota_exceeded", "error": body}),
                    );
                    self.emit_turn_complete(false);
                    self.emit(EngineEvent::TurnError {
                        message: "API quota exceeded — check your plan and billing details"
                            .to_string(),
                    });
                    return;
                }
                Err(e) => {
                    let error_msg = e.to_string().replace('\n', " ");
                    let is_ctx = is_context_window_error(&e);
                    log::entry(
                        log::Level::Warn,
                        if is_ctx {
                            "context_limit_reached"
                        } else {
                            "agent_stop"
                        },
                        &serde_json::json!({"reason": "llm_error", "error": error_msg.clone()}),
                    );
                    if is_ctx {
                        // Ask the host's recovery hook for a shorter
                        // conversation. On success we swap messages
                        // (preserving the system prompt at index 0)
                        // and re-enter the loop transparently.
                        let history: Vec<Message> = self.messages.iter().skip(1).cloned().collect();
                        let recovered = self
                            .host_call(|reply| crate::host::HostCall::RecoverFromContextLimit {
                                messages: history,
                                reply,
                            })
                            .await
                            .flatten();
                        if let Some(shorter) = recovered {
                            log::entry(
                                log::Level::Info,
                                "context_limit_recovered",
                                &serde_json::json!({"new_message_count": shorter.len() + 1}),
                            );
                            self.messages.truncate(1);
                            self.messages.extend(shorter);
                            self.emit_messages_snapshot();
                            continue;
                        }
                    }
                    self.emit_turn_complete(false);
                    let message = if is_ctx {
                        "Context limit reached. Run /compact and retry.".to_string()
                    } else {
                        error_msg
                    };
                    self.emit(EngineEvent::TurnError { message });
                    return;
                }
            };

            if let Some(tps) = resp.tokens_per_sec {
                self.tps_samples.push(tps);
            }
            if resp.usage.prompt_tokens.is_some() {
                send_usage(
                    self.event_tx,
                    &self.config.api.provider_type,
                    &self.config.api.model_config,
                    &self.model,
                    resp.usage,
                    resp.tokens_per_sec,
                    false,
                );
            }

            let content = resp.content.map(Content::text);
            let tool_calls = resp.tool_calls;
            let reasoning = resp.reasoning_content;
            let reasoning_details = resp.reasoning_details;

            // Injected message arrived during the LLM call; loop so the model can respond.
            if had_injected && tool_calls.is_empty() {
                continue;
            }

            // Streaming already delivered deltas; only emit batch events for non-streaming.
            if partial_text.is_empty() && partial_reasoning.is_empty() {
                if let Some(ref reasoning) = reasoning {
                    let trimmed = reasoning.trim();
                    if !trimmed.is_empty() {
                        self.emit(EngineEvent::Thinking {
                            content: trimmed.to_string(),
                        });
                    }
                }

                if let Some(ref content) = content {
                    let trimmed = content.as_text().trim();
                    if !trimmed.is_empty() {
                        self.emit(EngineEvent::Text {
                            content: trimmed.to_string(),
                        });
                    }
                }
            }

            if tool_calls.is_empty() {
                let is_empty = content.is_none()
                    && reasoning.is_none()
                    && self
                        .messages
                        .last()
                        .map(|m| m.role == Role::Tool)
                        .unwrap_or(false);

                if is_empty && empty_retries < MAX_EMPTY_RETRIES {
                    empty_retries += 1;
                    log::entry(
                        log::Level::Warn,
                        "empty_response_retry",
                        &serde_json::json!({ "attempt": empty_retries }),
                    );
                    continue;
                }

                let msg = self
                    .apply_response_hooks(Message::assistant_with_reasoning(
                        content,
                        reasoning,
                        reasoning_details,
                        None,
                    ))
                    .await;
                self.messages.push(msg);
                self.emit_messages_snapshot();
                self.emit_turn_complete(false);
                return;
            }

            empty_retries = 0;
            let msg = self
                .apply_response_hooks(Message::assistant_with_reasoning(
                    content,
                    reasoning,
                    reasoning_details,
                    Some(tool_calls.clone()),
                ))
                .await;
            self.messages.push(msg);
            self.emit_messages_snapshot();

            let mut plan = self.classify_tools(&tool_calls);
            let mut completed: Vec<Option<ToolResult>> =
                (0..plan.slots.len()).map(|_| None).collect();
            let (cancelled, deferred, mut tool_results) =
                self.execute_concurrent(&mut plan, &mut completed).await;
            let seq_tool_results = self.run_sequential(&plan, &mut completed).await;
            tool_results.extend(seq_tool_results);
            if cancelled {
                self.mark_unfinished_cancelled(&plan, &completed);
            }
            self.collect_results(&plan, completed);
            for (call_id, content, is_error) in tool_results {
                self.push_message(Message::tool(call_id, content, is_error));
            }
            for cmd in deferred {
                self.handle_turn_cmd(cmd);
            }
        }
    }

    fn classify_tools<'b>(&mut self, tool_calls: &'b [protocol::ToolCall]) -> ToolExecutionPlan<'b>
    where
        'a: 'b,
    {
        let mut plan = ToolExecutionPlan {
            slots: Vec::new(),
            ready: Vec::new(),
            pending_perms: Vec::new(),
            pending_tools: Vec::new(),
            sequential_tools: Vec::new(),
            pending_tool_hooks: Vec::new(),
            pending_tool_perms: Vec::new(),
        };

        for tc in tool_calls {
            self.drain_commands();
            if self.cancel.is_cancelled() {
                break;
            }

            let args: HashMap<String, Value> =
                serde_json::from_str(&tc.function.arguments).unwrap_or_default();

            let tool_start = self.config.clock.instant_now();
            self.emit(EngineEvent::ToolStarted {
                call_id: tc.id.clone(),
                tool_name: tc.function.name.clone(),
                args: args.clone(),
            });

            let tool = self.tools.iter().find(|pt| {
                pt.name == tc.function.name
                    && (pt.override_core || !self.dispatcher.contains(&tc.function.name))
            });
            if let Some(pt) = tool {
                let is_sequential =
                    matches!(pt.execution_mode, protocol::ToolExecutionMode::Sequential);
                if pt.hooks.any() {
                    let request_id = next_request_id();
                    self.emit(EngineEvent::ToolHooksRequest {
                        request_id,
                        call_id: tc.id.clone(),
                        tool_name: tc.function.name.clone(),
                        args: args.clone(),
                        mode: self.mode,
                    });
                    plan.pending_tool_hooks.push((
                        request_id,
                        PendingToolCall {
                            tc,
                            args: args.clone(),
                            tool_start,
                            is_sequential,
                        },
                    ));
                } else if is_sequential {
                    plan.sequential_tools.push((tc, args.clone(), tool_start));
                } else {
                    let request_id = next_request_id();
                    self.emit(EngineEvent::ToolDispatch {
                        request_id,
                        call_id: tc.id.clone(),
                        tool_name: tc.function.name.clone(),
                        args: args.clone(),
                    });
                    plan.pending_tools
                        .push((request_id, tc.id.clone(), tool_start));
                }
                continue;
            }

            let hooks = match self
                .dispatcher
                .evaluate_hooks(&tc.function.name, &args, self.mode)
            {
                Some(h) => h,
                None => {
                    self.push_tool_result(
                        &tc.id,
                        &format!("unknown tool: {}", tc.function.name),
                        true,
                        Some(tool_start),
                    );
                    continue;
                }
            };

            let idx = plan.slots.len();
            match hooks.decision {
                Decision::Allow => {
                    plan.slots.push(ToolSlot {
                        tc,
                        args,
                        confirm_msg: None,
                        start: tool_start,
                    });
                    plan.ready.push(idx);
                }
                Decision::Deny => {
                    self.push_tool_result(
                        &tc.id,
                        "The user's permission settings blocked this tool call. \
                         Try a different approach or ask the user for guidance.",
                        false,
                        None,
                    );
                }
                Decision::Error(ref err) => {
                    self.push_tool_result(&tc.id, err, true, None);
                }
                Decision::Ask => {
                    let summary = if hooks.summary.is_empty() {
                        protocol::StyledLines::from_plain(&tc.function.name)
                    } else {
                        hooks.summary
                    };
                    let request_id = next_request_id();
                    self.emit(EngineEvent::RequestPermission {
                        request_id,
                        call_id: tc.id.clone(),
                        tool_name: tc.function.name.clone(),
                        args: args.clone(),
                        approval_patterns: hooks.approval_patterns,
                        summary,
                    });
                    plan.slots.push(ToolSlot {
                        tc,
                        args,
                        confirm_msg: None,
                        start: tool_start,
                    });
                    plan.pending_perms.push((idx, request_id));
                }
            }
        }

        plan
    }

    /// Run ready tools concurrently, processing permission decisions and steering mid-flight.
    /// Returns `(cancelled, deferred_commands, tool_results)`.
    async fn execute_concurrent<'b>(
        &mut self,
        plan: &mut ToolExecutionPlan<'b>,
        completed: &mut [Option<ToolResult>],
    ) -> (bool, Vec<UiCommand>, Vec<(String, String, bool)>) {
        use futures_util::stream::StreamExt;

        type TaggedFut<'x> =
            std::pin::Pin<Box<dyn std::future::Future<Output = (usize, ToolResult)> + Send + 'x>>;

        let contexts: Vec<_> = plan.slots.iter().map(|_| ToolContext).collect();
        // Cloned once so the select! arms can compute elapsed without reborrowing `self`.
        let clock = std::sync::Arc::clone(&self.config.clock);
        let elapsed_ms_since = |start: Instant| -> u64 {
            clock.instant_now().duration_since(start).as_millis() as u64
        };

        let mut futs: futures_util::stream::FuturesUnordered<TaggedFut<'_>> =
            futures_util::stream::FuturesUnordered::new();

        // Side-call futures from `smelt.tools.call` — don't count against `outstanding`.
        type SideFut<'x> =
            std::pin::Pin<Box<dyn std::future::Future<Output = (u64, ToolResult)> + Send + 'x>>;
        let mut side_futs: futures_util::stream::FuturesUnordered<SideFut<'_>> =
            futures_util::stream::FuturesUnordered::new();

        let dispatcher = self.dispatcher;
        for &i in &plan.ready {
            let fut = dispatcher
                .dispatch(
                    &plan.slots[i].tc.function.name,
                    plan.slots[i].args.clone(),
                    &contexts[i],
                )
                .expect("dispatcher resolved tool at slot-build time");
            futs.push(Box::pin(async move { (i, fut.await) }));
        }

        let mut outstanding = plan.ready.len()
            + plan.pending_perms.len()
            + plan.pending_tools.len()
            + plan.pending_tool_hooks.len()
            + plan.pending_tool_perms.len();
        let cancel = &self.cancel;
        let cmd_rx = &mut self.cmd_rx;
        let mut deferred: Vec<UiCommand> = Vec::new();
        let mut tool_results: Vec<(String, String, bool)> = Vec::new();

        let cancelled = loop {
            if outstanding == 0 {
                break false;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break true,
                Some(cmd) = cmd_rx.recv() => match cmd {
                    UiCommand::Cancel => cancel.cancel(),
                    UiCommand::PermissionDecision { request_id, approved, message } => {
                        if let Some(pos) = plan
                            .pending_perms
                            .iter()
                            .position(|(_, rid)| *rid == request_id)
                        {
                            let (idx, _) = plan.pending_perms.swap_remove(pos);
                            if approved {
                                plan.slots[idx].confirm_msg = message;
                                let fut = dispatcher
                                    .dispatch(
                                        &plan.slots[idx].tc.function.name,
                                        plan.slots[idx].args.clone(),
                                        &contexts[idx],
                                    )
                                    .expect("dispatcher resolved tool at slot-build time");
                                futs.push(Box::pin(async move { (idx, fut.await) }));
                            } else {
                                let denial = match message {
                                    Some(msg) => format!(
                                        "The user denied this tool call with message: {msg}"
                                    ),
                                    None => "The user denied this tool call. Try a different \
                                             approach or ask the user for guidance."
                                        .to_string(),
                                };
                                completed[idx] = Some(ToolResult {
                                    content: denial,
                                    is_error: false,
                                    metadata: None,
                                });
                                outstanding -= 1;
                            }
                        } else if let Some(pos) = plan
                            .pending_tool_perms
                            .iter()
                            .position(|(rid, _)| *rid == request_id)
                        {
                            let (_, pending) = plan.pending_tool_perms.swap_remove(pos);
                            if approved {
                                if pending.is_sequential {
                                    plan.sequential_tools.push((
                                        pending.tc,
                                        pending.args,
                                        pending.tool_start,
                                    ));
                                } else {
                                    let rid = next_request_id();
                                    let _ = self.event_tx.send(EngineEvent::ToolDispatch {
                                        request_id: rid,
                                        call_id: pending.tc.id.clone(),
                                        tool_name: pending.tc.function.name.clone(),
                                        args: pending.args.clone(),
                                    });
                                    plan.pending_tools.push((
                                        rid,
                                        pending.tc.id.clone(),
                                        pending.tool_start,
                                    ));
                                }
                            } else {
                                let denial = match message {
                                    Some(msg) => format!(
                                        "The user denied this tool call with message: {msg}"
                                    ),
                                    None => "The user denied this tool call. Try a different \
                                             approach or ask the user for guidance."
                                        .to_string(),
                                };
                                let tool_start = pending.tool_start;
                                let elapsed_ms = Some(elapsed_ms_since(tool_start));
                                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                    call_id: pending.tc.id.clone(),
                                    result: ToolOutcome {
                                        content: denial.clone(),
                                        is_error: false,
                                        metadata: None,
                                    },
                                    elapsed_ms,
                                });
                                tool_results.push((pending.tc.id.clone(), denial, false));
                                outstanding -= 1;
                            }
                        }
                    }
                    UiCommand::ToolHooksResponse { request_id, hooks } => {
                        if let Some(pos) = plan
                            .pending_tool_hooks
                            .iter()
                            .position(|(rid, _)| *rid == request_id)
                        {
                            let (_, pending) = plan.pending_tool_hooks.swap_remove(pos);
                            match hooks.decision {
                                Decision::Allow => {
                                    if pending.is_sequential {
                                        plan.sequential_tools.push((
                                            pending.tc,
                                            pending.args,
                                            pending.tool_start,
                                        ));
                                    } else {
                                        let rid = next_request_id();
                                        let _ = self
                                            .event_tx
                                            .send(EngineEvent::ToolDispatch {
                                                request_id: rid,
                                                call_id: pending.tc.id.clone(),
                                                tool_name: pending.tc.function.name.clone(),
                                                args: pending.args.clone(),
                                            });
                                        plan.pending_tools.push((
                                            rid,
                                            pending.tc.id.clone(),
                                            pending.tool_start,
                                        ));
                                    }
                                }
                                Decision::Deny => {
                                    let denial = "The user's permission settings blocked \
                                                  this tool call. Try a different approach \
                                                  or ask the user for guidance."
                                        .to_string();
                                    let tool_start = pending.tool_start;
                                    let elapsed_ms = Some(elapsed_ms_since(tool_start));
                                    let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                        call_id: pending.tc.id.clone(),
                                        result: ToolOutcome {
                                            content: denial.clone(),
                                            is_error: false,
                                            metadata: None,
                                        },
                                        elapsed_ms,
                                    });
                                    tool_results.push((pending.tc.id.clone(), denial, false));
                                    outstanding -= 1;
                                }
                                Decision::Error(ref err) => {
                                    let tool_start = pending.tool_start;
                                    let elapsed_ms = Some(elapsed_ms_since(tool_start));
                                    let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                        call_id: pending.tc.id.clone(),
                                        result: ToolOutcome {
                                            content: err.clone(),
                                            is_error: true,
                                            metadata: None,
                                        },
                                        elapsed_ms,
                                    });
                                    tool_results.push((pending.tc.id.clone(), err.clone(), true));
                                    outstanding -= 1;
                                }
                                Decision::Ask => {
                                    let summary = if hooks.summary.is_empty() {
                                        protocol::StyledLines::from_plain(
                                            &pending.tc.function.name,
                                        )
                                    } else {
                                        hooks.summary.clone()
                                    };
                                    let rid = next_request_id();
                                    let _ = self
                                        .event_tx
                                        .send(EngineEvent::RequestPermission {
                                            request_id: rid,
                                            call_id: pending.tc.id.clone(),
                                            tool_name: pending.tc.function.name.clone(),
                                            args: pending.args.clone(),
                                            approval_patterns: hooks.approval_patterns,
                                            summary,
                                        });
                                    plan.pending_tool_perms.push((rid, pending));
                                }
                            }
                        }
                    }
                    UiCommand::ToolResult { request_id, call_id, content, is_error } => {
                        if let Some(pos) = plan
                            .pending_tools
                            .iter()
                            .position(|(rid, _, _)| *rid == request_id)
                        {
                            let (_, _, start) = plan.pending_tools.swap_remove(pos);
                            let elapsed_ms = Some(elapsed_ms_since(start));
                            let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                call_id: call_id.clone(),
                                result: ToolOutcome {
                                    content: content.clone(),
                                    is_error,
                                    metadata: None,
                                },
                                elapsed_ms,
                            });
                            tool_results.push((call_id, content, is_error));
                            outstanding -= 1;
                        }
                    }
                    UiCommand::CallCoreTool { request_id, parent_call_id, tool_name, args } => {
                        if dispatcher.contains(&tool_name) {
                            let _ = parent_call_id;
                            let ctx = ToolContext;
                            side_futs.push(Box::pin(async move {
                                let r = dispatcher
                                    .dispatch(&tool_name, args, &ctx)
                                    .expect("dispatcher contains tool")
                                    .await;
                                (request_id, r)
                            }));
                        } else {
                            let _ = self.event_tx.send(EngineEvent::CoreToolResult {
                                request_id,
                                content: format!("tool not found: {tool_name}"),
                                is_error: true,
                                metadata: None,
                            });
                        }
                    }
                    UiCommand::Steer { .. }
                    | UiCommand::Unsteer { .. }
                    | UiCommand::SetAgentMode { .. }
                    | UiCommand::SetReasoningEffort { .. }
                    | UiCommand::SetModel { .. } => deferred.push(cmd),
                    _ => {}
                },
                Some((idx, result)) = futs.next(), if !futs.is_empty() => {
                    completed[idx] = Some(result);
                    outstanding -= 1;
                }
                Some((req_id, result)) = side_futs.next(), if !side_futs.is_empty() => {
                    let _ = self.event_tx.send(EngineEvent::CoreToolResult {
                        request_id: req_id,
                        content: result.content,
                        is_error: result.is_error,
                        metadata: result.metadata,
                    });
                }
            }
        };

        if cancelled {
            for (_, call_id, start) in plan.pending_tools.drain(..) {
                let elapsed_ms = Some(elapsed_ms_since(start));
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: call_id.clone(),
                    result: ToolOutcome {
                        content: "cancelled".to_string(),
                        is_error: true,
                        metadata: None,
                    },
                    elapsed_ms,
                });
                tool_results.push((call_id, "cancelled".to_string(), true));
            }
            for (_, pending) in plan.pending_tool_hooks.drain(..) {
                let elapsed_ms = Some(elapsed_ms_since(pending.tool_start));
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: pending.tc.id.clone(),
                    result: ToolOutcome {
                        content: "cancelled".to_string(),
                        is_error: true,
                        metadata: None,
                    },
                    elapsed_ms,
                });
                tool_results.push((pending.tc.id.clone(), "cancelled".to_string(), true));
            }
            for (_, pending) in plan.pending_tool_perms.drain(..) {
                let elapsed_ms = Some(elapsed_ms_since(pending.tool_start));
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: pending.tc.id.clone(),
                    result: ToolOutcome {
                        content: "cancelled".to_string(),
                        is_error: true,
                        metadata: None,
                    },
                    elapsed_ms,
                });
                tool_results.push((pending.tc.id.clone(), "cancelled".to_string(), true));
            }
        }

        (cancelled, deferred, tool_results)
    }

    /// Emits cancelled results for slots that never completed.
    fn mark_unfinished_cancelled(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        completed: &[Option<ToolResult>],
    ) {
        let starts: Vec<_> = plan
            .slots
            .iter()
            .enumerate()
            .filter(|(i, _)| completed[*i].is_none())
            .map(|(_, slot)| (slot.tc.id.clone(), slot.start))
            .collect();
        for (call_id, start) in starts {
            self.push_tool_result(&call_id, "cancelled", true, Some(start));
        }
    }

    /// Dispatch sequential tools one at a time (used by tools that await user interaction).
    async fn run_sequential(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        _completed: &mut [Option<ToolResult>],
    ) -> Vec<(String, String, bool)> {
        let mut tool_results = Vec::new();
        let mut cancelled = false;
        for (tc, args, start) in &plan.sequential_tools {
            let (content, is_error) = if cancelled || self.cancel.is_cancelled() {
                cancelled = true;
                ("cancelled".to_string(), true)
            } else {
                let request_id = next_request_id();
                let _ = self.event_tx.send(EngineEvent::ToolDispatch {
                    request_id,
                    call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
                    args: args.clone(),
                });
                match self.wait_for_tool_result(request_id).await {
                    Some(result) => result,
                    None => {
                        cancelled = true;
                        ("cancelled".to_string(), true)
                    }
                }
            };
            let elapsed_ms = Some(self.elapsed_ms_since(*start));
            let _ = self.event_tx.send(EngineEvent::ToolFinished {
                call_id: tc.id.clone(),
                result: ToolOutcome {
                    content: content.clone(),
                    is_error,
                    metadata: None,
                },
                elapsed_ms,
            });
            tool_results.push((tc.id.clone(), content, is_error));
        }
        tool_results
    }

    /// Commit tool results to history, emit ToolFinished, and record elapsed times.
    fn collect_results(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        mut completed: Vec<Option<ToolResult>>,
    ) {
        for (i, slot) in plan.slots.iter().enumerate() {
            let Some(result) = completed[i].take() else {
                continue;
            };
            let ToolResult {
                content,
                is_error,
                metadata,
            } = result;

            if log::Level::Debug.enabled() {
                let mut preview = content[..content.floor_char_boundary(500)].to_string();
                if self.config.redact_secrets {
                    preview = crate::redact::redact(&preview);
                }
                log::entry(
                    log::Level::Debug,
                    "tool_result",
                    &serde_json::json!({
                        "tool": slot.tc.function.name,
                        "id": slot.tc.id,
                        "is_error": is_error,
                        "content_len": content.len(),
                        "content_preview": preview,
                    }),
                );
            }

            let elapsed_ms = self.elapsed_ms_since(slot.start);
            self.tool_elapsed.insert(slot.tc.id.clone(), elapsed_ms);
            let mut tool_content = content.clone();
            if let Some(ref msg) = slot.confirm_msg {
                tool_content.push_str(&format!("\n\nUser message: {msg}"));
            }
            let history_content =
                match crate::result_dedup::duplicate_of(&tool_content, is_error, &self.messages) {
                    Some(prior_id) => crate::result_dedup::dedup_stub(prior_id),
                    None => tool_content,
                };
            self.push_message(Message::tool(slot.tc.id.clone(), history_content, is_error));
            self.emit_messages_snapshot();
            self.emit(EngineEvent::ToolFinished {
                call_id: slot.tc.id.clone(),
                result: ToolOutcome {
                    content,
                    is_error,
                    metadata,
                },
                elapsed_ms: Some(elapsed_ms),
            });
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.handle_turn_cmd(cmd);
        }
    }

    /// Call the LLM. Returns `(result, partial_text, partial_reasoning)`.
    /// `had_injected` in the result is true when a Steer arrived mid-call.
    async fn call_llm(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> (
        Result<(crate::provider::LLMResponse, bool), ProviderError>,
        String,
        String,
    ) {
        // The chat future borrows self.provider and self.model, so model
        // changes received mid-request are deferred until the future resolves.
        let mut pending_model: Option<(String, String, String, String)> = None;
        let mut deferred_turn_cmds: Vec<UiCommand> = Vec::new();

        let partial_text = std::sync::Mutex::new(String::new());
        let partial_reasoning = std::sync::Mutex::new(String::new());

        // Model changes received mid-request are applied after the future resolves.
        let result = {
            let on_retry = |delay: std::time::Duration, attempt: u32| {
                let _ = self.event_tx.send(EngineEvent::Retrying {
                    delay_ms: delay.as_millis() as u64,
                    attempt,
                });
            };
            let on_delta = |delta: provider::StreamDelta| match delta {
                provider::StreamDelta::Text(text) => {
                    partial_text.lock().unwrap().push_str(text);
                    let _ = self.event_tx.send(EngineEvent::TextDelta {
                        delta: text.to_string(),
                    });
                }
                provider::StreamDelta::Thinking(text) => {
                    partial_reasoning.lock().unwrap().push_str(text);
                    let _ = self.event_tx.send(EngineEvent::ThinkingDelta {
                        delta: text.to_string(),
                    });
                }
                provider::StreamDelta::ToolArgs {
                    call_id,
                    tool_name,
                    delta,
                } => {
                    let _ = self.event_tx.send(EngineEvent::ToolArgsDelta {
                        call_id: call_id.to_string(),
                        tool_name: tool_name.to_string(),
                        delta: delta.to_string(),
                    });
                }
            };
            let mut cache = self
                .provider
                .default_cache_config(self.config.cache_ttl_long);
            cache.prompt_cache_key = Some(self.session_id.clone());
            let opts = ChatOptions {
                cancel: &self.cancel,
                on_retry: Some(&on_retry),
                on_delta: Some(&on_delta),
                response_format: None,
                cache,
            };
            let chat_future = self.provider.chat(
                &self.messages,
                tool_defs,
                &self.model,
                self.reasoning_effort,
                &opts,
            );
            tokio::pin!(chat_future);

            let mut cancel_received = false;
            loop {
                if cancel_received {
                    break (&mut chat_future).await;
                }
                tokio::select! {
                    biased;
                    result = &mut chat_future => break result,
                    Some(cmd) = self.cmd_rx.recv() => match cmd {
                        UiCommand::Cancel => {
                            self.cancel.cancel();
                            cancel_received = true;
                        }
                        UiCommand::SetAgentMode { mode, system_prompt, tools } => {
                            self.mode = mode;
                            if let Some(p) = system_prompt { self.system_prompt = p; }
                            if let Some(t) = tools { self.tools = t; }
                        }
                        UiCommand::SetReasoningEffort { effort } => self.reasoning_effort = effort,
                        UiCommand::SetModel { model, api_base, api_key, provider_type } => {
                            pending_model = Some((model, api_base, api_key, provider_type));
                        }
                        UiCommand::Steer { .. }
                        | UiCommand::Unsteer { .. } => deferred_turn_cmds.push(cmd),
                        other => {
                            self.handle_background_cmd(other);
                        }
                    },
                }
            }
        };

        let pt = partial_text.into_inner().unwrap_or_default();
        let pr = partial_reasoning.into_inner().unwrap_or_default();

        if let Some((model, api_base, api_key, provider_type)) = pending_model {
            self.apply_model_change(model, api_base, api_key, provider_type);
        }
        let had_injected = deferred_turn_cmds
            .iter()
            .any(|c| matches!(c, UiCommand::Steer { .. }));
        for cmd in deferred_turn_cmds {
            self.handle_turn_cmd(cmd);
        }
        (result.map(|r| (r, had_injected)), pt, pr)
    }

    async fn wait_for_tool_result(&mut self, request_id: u64) -> Option<(String, bool)> {
        loop {
            match self.cmd_rx.recv().await {
                Some(UiCommand::ToolResult {
                    request_id: id,
                    content,
                    is_error,
                    ..
                }) if id == request_id => return Some((content, is_error)),
                Some(UiCommand::SetAgentMode {
                    mode,
                    system_prompt,
                    tools,
                }) => {
                    self.mode = mode;
                    if let Some(p) = system_prompt {
                        self.system_prompt = p;
                    } else {
                        self.regenerate_system_prompt();
                    }
                    if let Some(t) = tools {
                        self.tools = t;
                    }
                }
                Some(UiCommand::SetReasoningEffort { effort }) => self.reasoning_effort = effort,
                Some(UiCommand::SetModel {
                    model,
                    api_base,
                    api_key,
                    provider_type,
                }) => self.apply_model_change(model, api_base, api_key, provider_type),
                Some(UiCommand::Cancel) => {
                    self.cancel.cancel();
                    return None;
                }
                None => return None,
                Some(other) => {
                    self.handle_background_cmd(other);
                }
            }
        }
    }

    fn push_tool_result(
        &mut self,
        tool_call_id: &str,
        content: &str,
        is_error: bool,
        started_at: Option<Instant>,
    ) {
        let history_content =
            match crate::result_dedup::duplicate_of(content, is_error, &self.messages) {
                Some(prior_id) => crate::result_dedup::dedup_stub(prior_id),
                None => content.to_string(),
            };
        self.push_message(Message::tool(
            tool_call_id.to_string(),
            history_content,
            is_error,
        ));
        self.emit(EngineEvent::ToolFinished {
            call_id: tool_call_id.to_string(),
            result: ToolOutcome {
                content: content.to_string(),
                is_error,
                metadata: None,
            },
            elapsed_ms: started_at.map(|t| self.elapsed_ms_since(t)),
        });
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn send_usage(
    tx: &mpsc::UnboundedSender<EngineEvent>,
    provider_type: &str,
    model_config: &crate::ModelConfig,
    model: &str,
    usage: protocol::TokenUsage,
    tokens_per_sec: Option<f64>,
    background: bool,
) {
    let resolved = crate::pricing::resolve(model, provider_type, model_config);
    let cost = resolved.pricing.cost(&usage);
    let _ = tx.send(EngineEvent::TokenUsage {
        usage,
        tokens_per_sec,
        cost_usd: if cost > 0.0 { Some(cost) } else { None },
        background,
    });
}

#[derive(Clone)]
struct PricingContext {
    provider_type: String,
    model_config: crate::ModelConfig,
}

impl PricingContext {
    fn from_api(api: &crate::ApiConfig) -> Self {
        Self {
            provider_type: api.provider_type.clone(),
            model_config: api.model_config.clone(),
        }
    }

    fn emit(
        &self,
        tx: &mpsc::UnboundedSender<EngineEvent>,
        model: &str,
        usage: protocol::TokenUsage,
    ) {
        send_usage(
            tx,
            &self.provider_type,
            &self.model_config,
            model,
            usage,
            None,
            true,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_cfg() -> ApiConfig {
        ApiConfig {
            base: "https://x/".into(),
            key: "k".into(),
            key_env: "K".into(),
            provider_type: "openai".into(),
            model_config: ModelConfig::default(),
        }
    }

    // ---- next_request_id ----

    #[test]
    fn next_request_id_returns_monotonically_increasing_values() {
        let a = next_request_id();
        let b = next_request_id();
        let c = next_request_id();
        assert!(b > a);
        assert!(c > b);
    }

    // ---- build_provider ----

    #[test]
    fn build_provider_strips_trailing_slash_from_api_base() {
        let api = ApiConfig {
            base: "https://x/".into(),
            ..api_cfg()
        };
        let p = build_provider(
            &api,
            &reqwest::Client::new(),
            None,
            None,
            None,
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert_eq!(p.api_base(), "https://x");
        assert_eq!(p.api_key(), "k");
    }

    #[test]
    fn build_provider_uses_api_base_and_key_overrides_when_some() {
        let api = ApiConfig {
            base: "default-base".into(),
            key: "default-key".into(),
            ..api_cfg()
        };
        let p = build_provider(
            &api,
            &reqwest::Client::new(),
            Some("override-base/"),
            Some("ok"),
            None,
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert_eq!(p.api_base(), "override-base");
        assert_eq!(p.api_key(), "ok");
    }

    #[test]
    fn build_provider_falls_back_to_api_fields_when_overrides_none() {
        let api = ApiConfig {
            base: "fallback-base/".into(),
            key: "fallback-key".into(),
            ..api_cfg()
        };
        let p = build_provider(
            &api,
            &reqwest::Client::new(),
            None,
            None,
            None,
            std::sync::Arc::new(crate::clock::RealClock),
        );
        assert_eq!(p.api_base(), "fallback-base");
        assert_eq!(p.api_key(), "fallback-key");
    }

    #[test]
    fn build_provider_applies_model_overrides_when_some() {
        let api = ApiConfig {
            model_config: ModelConfig {
                temperature: Some(0.1),
                top_p: Some(0.2),
                ..Default::default()
            },
            ..api_cfg()
        };
        let overrides = protocol::ModelConfigOverrides {
            temperature: Some(0.9),
            top_k: Some(7),
            ..Default::default()
        };
        let p = build_provider(
            &api,
            &reqwest::Client::new(),
            None,
            None,
            Some(&overrides),
            std::sync::Arc::new(crate::clock::RealClock),
        );
        let cfg = p.model_config_for_test();
        assert_eq!(cfg.temperature, Some(0.9));
        assert_eq!(cfg.top_p, Some(0.2));
        assert_eq!(cfg.top_k, Some(7));
    }

    // ---- send_usage ----

    #[test]
    fn send_usage_emits_token_usage_event_with_cost_when_pricing_resolves() {
        let (tx, mut rx) = mpsc::unbounded_channel::<EngineEvent>();
        let cfg = ModelConfig {
            input_cost: Some(5.0),
            output_cost: Some(10.0),
            ..Default::default()
        };
        let usage = protocol::TokenUsage {
            prompt_tokens: Some(1_000_000),
            completion_tokens: Some(500_000),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        };
        send_usage(&tx, "openai", &cfg, "model-x", usage, Some(50.0), false);
        match rx.try_recv().unwrap() {
            EngineEvent::TokenUsage {
                cost_usd,
                tokens_per_sec,
                background,
                ..
            } => {
                assert!(cost_usd.is_some());
                assert_eq!(tokens_per_sec, Some(50.0));
                assert!(!background);
            }
            _ => panic!("expected TokenUsage"),
        }
    }

    #[test]
    fn send_usage_emits_no_cost_when_pricing_zero() {
        let (tx, mut rx) = mpsc::unbounded_channel::<EngineEvent>();
        let cfg = ModelConfig::default();
        let usage = protocol::TokenUsage::default();
        send_usage(&tx, "openai-compatible", &cfg, "model", usage, None, false);
        match rx.try_recv().unwrap() {
            EngineEvent::TokenUsage { cost_usd, .. } => assert!(cost_usd.is_none()),
            _ => panic!("expected TokenUsage"),
        }
    }

    // ---- PricingContext ----

    #[test]
    fn pricing_context_from_api_clones_provider_type_and_model_config() {
        let api = api_cfg();
        let pc = PricingContext::from_api(&api);
        assert_eq!(pc.provider_type, "openai");
        assert!(pc.model_config.tool_calling.is_none());
    }

    #[test]
    fn pricing_context_emit_sends_background_event() {
        let (tx, mut rx) = mpsc::unbounded_channel::<EngineEvent>();
        let pc = PricingContext::from_api(&api_cfg());
        pc.emit(&tx, "m", protocol::TokenUsage::default());
        match rx.try_recv().unwrap() {
            EngineEvent::TokenUsage {
                background,
                tokens_per_sec,
                ..
            } => {
                assert!(background);
                assert!(tokens_per_sec.is_none());
            }
            _ => panic!("expected TokenUsage"),
        }
    }
}
