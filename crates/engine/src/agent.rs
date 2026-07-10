use crate::log;
use crate::provider::EngineProvider;
use crate::tools::{ToolContext, ToolDispatcher, ToolResult};
use crate::{ApiConfig, EngineConfig};
use protocol::Decision;
use protocol::{
    AgentMode, AskModel, AssistantStep, Content, EngineAskError, EngineAskErrorKind, EngineEvent,
    HistoryItem, Message, ReasoningEffort, ReasoningKind, ToolInvocation, ToolOutcome, TurnMeta,
    UiCommand,
};
use serde_json::Value;
use smelt_provider::{
    quota_exceeded_message, sort_tools_for_cache_stability, CancellationToken, ChatOptions,
    ChatRequestOptions, ChatResponse, FunctionSchema, ModelConfig, ProviderError,
    ProviderStreamEvent, ReasoningStreamEvent, RequestAttemptInfo, ResponseFormat,
    ToolCallStreamEvent, ToolDefinition,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::Instant;
use tokio::sync::mpsc;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

fn dispatch_request_audit(
    host_tx: &mpsc::UnboundedSender<crate::host::HostCall>,
    session_dir: &Path,
    ctx: crate::request_log::RequestContext,
    info: &RequestAttemptInfo<'_>,
    pricing: &smelt_provider::ResolvedPricing,
    mode: crate::RequestAuditMode,
) {
    let Some((entry, payload_mode)) = crate::request_log::entry(ctx, info, pricing, mode) else {
        return;
    };
    let _ = host_tx.send(crate::host::HostCall::RequestAudit {
        session_dir: session_dir.to_path_buf(),
        entry: Box::new(entry),
        payload_mode,
    });
}

fn last_note_kind(items: &[HistoryItem]) -> Option<protocol::HistoryNoteKind> {
    items.last().and_then(HistoryItem::note_kind)
}

fn mode_append_returns_to_base(append: &protocol::HistoryAppend) -> bool {
    let protocol::HistoryAppendPolicy::ModeChange { base } = &append.policy else {
        return false;
    };
    append
        .item
        .as_note()
        .and_then(protocol::HistoryNote::mode)
        .is_some_and(|mode| mode == base.as_str())
}

/// Main engine task. Runs in a tokio::spawn and processes commands/events.
pub(crate) async fn engine_task(
    mut config: EngineConfig,
    dispatcher: Box<dyn crate::tools::ToolDispatcher>,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    host_tx: mpsc::UnboundedSender<crate::host::HostCall>,
) {
    // Some openai-compatible endpoints gate on User-Agent (e.g. api.kimi.com).
    // Per-request header() calls (Copilot, Codex) still override this.
    let client = reqwest::Client::builder()
        .user_agent(concat!("smelt/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    smelt_provider::catalog::spawn_fetch(
        client.clone(),
        Some(crate::paths::cache_dir().join("web")),
    );

    let _ = event_tx.send(EngineEvent::Ready);

    let mut bg_cancel = CancellationToken::new();

    loop {
        if bg_cancel.is_cancelled() {
            bg_cancel = CancellationToken::new();
        }
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::StartTurn(payload) => {
                        let protocol::StartTurnPayload {
                            turn_id,
                            input,
                            mode,
                            model,
                            reasoning_effort,
                            fast_mode,
                            history,
                            api_base,
                            api_key,
                            session_id,
                            session_dir,
                            model_config_overrides,
                            permission_overrides,
                            system_prompt: tui_system_prompt,
                            tools,
                        } = *payload;
                        let display = input.display();
                        let loaded_history = match load_model_history(history, &session_dir) {
                            Ok(history) => history,
                            Err(message) => {
                                let _ = event_tx.send(EngineEvent::TurnError {
                                    message,
                                    kind: None,
                                    retry_at_ms: None,
                                });
                                continue;
                            }
                        };

                        let provider = build_provider(
                            &config.api, &client,
                            api_base.as_deref(), api_key.as_deref(),
                            model_config_overrides.as_ref(),
                            std::sync::Arc::clone(&config.clock),
                        );
                        let system_prompt = tui_system_prompt
                            .or_else(|| config.system_prompt_override.clone())
                            .unwrap_or_else(|| {
                                crate::build_system_prompt(
                                    config.system_prompt_behavior,
                                    crate::SystemPromptCapabilities::from_tool_calling(
                                        provider.tool_calling(),
                                    ),
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
                            cancel: CancellationToken::new(),
                            bg_cancel: bg_cancel.clone(),
                            history: Vec::new(),
                            history_coordinates: loaded_history.coordinates,
                            mode,
                            reasoning_effort,
                            fast_mode,
                            turn_id,
                            model,
                            display,
                            system_prompt,
                            tools,
                            permission_overrides,
                            pending_history_items: Vec::new(),
                            next_history_changed_from: 0,
                            session_id,
                            session_dir,
                            started_at: config.clock.instant_now(),
                            tps_samples: Vec::new(),
                            tool_elapsed: HashMap::new(),
                        };
                        turn.run(input, loaded_history.items).await;
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
                    UiCommand::SetCwd { cwd } => {
                        config.cwd = std::path::PathBuf::from(cwd);
                    }
                    UiCommand::SetMode { .. } => {}
                    UiCommand::SetFastMode { .. } => {}
                    UiCommand::Cancel => {
                        bg_cancel.cancel();
                    }
                    other => {
                        let ctx = BackgroundCtx {
                            config: &config,
                            http_client: &client,
                            dispatcher: &*dispatcher,
                            event_tx: &event_tx,
                            host_tx: &host_tx,
                            bg_cancel: bg_cancel.clone(),
                        };
                        let _ = dispatch_background_cmd(other, &ctx);
                    }
                }
            }
            else => break,
        }
    }

    let _ = event_tx.send(EngineEvent::Shutdown { reason: None });
}

struct LoadedModelHistory {
    items: Vec<HistoryItem>,
    coordinates: protocol::ModelHistoryCoordinates,
}

fn load_model_history(
    source: protocol::ModelHistorySource,
    session_dir: &std::path::Path,
) -> Result<LoadedModelHistory, String> {
    let coordinates = source.coordinates();
    let items = match source {
        protocol::ModelHistorySource::Items { items, .. } => {
            smelt_perf::perf::record_value("engine:model_history:source_items", 1);
            smelt_perf::perf::record_value("engine:model_history:items", items.len() as u64);
            items
        }
        protocol::ModelHistorySource::Store {
            prefix,
            first_live_index,
            end_index,
            suffix,
            ..
        } => {
            smelt_perf::perf::record_value("engine:model_history:source_store", 1);
            smelt_perf::perf::record_value(
                "engine:model_history:first_live_index",
                first_live_index as u64,
            );
            smelt_perf::perf::record_value("engine:model_history:end_index", end_index as u64);
            smelt_perf::perf::record_value(
                "engine:model_history:suffix_items",
                suffix.len() as u64,
            );
            let mut history = prefix;
            if end_index > first_live_index {
                let db_path = session_dir.join("session.db");
                let db = smelt_store::SessionDb::open(&db_path)
                    .map_err(|err| format!("open model history database {db_path:?}: {err}"))?;
                let mut rows = db
                    .read_history_items_range(first_live_index..end_index)
                    .map_err(|err| format!("read model history rows: {err}"))?;
                smelt_perf::perf::record_value("engine:model_history:rows_read", rows.len() as u64);
                history.append(&mut rows);
            }
            history.extend(suffix);
            smelt_perf::perf::record_value("engine:model_history:items", history.len() as u64);
            history
        }
    };
    Ok(LoadedModelHistory { items, coordinates })
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
    pub fast_mode: bool,
    /// Optional tool list. When non-empty AND matching the main session's
    /// tools byte-for-byte, the request reuses the main session's
    /// Anthropic prefix cache.
    pub tools: Vec<protocol::ToolDef>,
    /// Session id forwarded as `prompt_cache_key` to OpenAI / Codex so
    /// the EngineAsk hits the same cache shard as the main turn.
    pub session_id: String,
    /// On-disk directory for this session. Used to write the SQLite request
    /// audit for introspection.
    pub session_dir: std::path::PathBuf,
    /// Whether text deltas for this auxiliary request should be forwarded
    /// as `EngineAskDelta` events.
    pub stream: bool,
    /// Whether provider retries for this auxiliary request should update the
    /// visible work state.
    pub visible_retries: bool,
}

/// Immutable refs out-of-band command dispatch needs. Bundling them lets
/// every site that drains `cmd_rx` (the outer engine loop, the
/// turn-control loop, `call_llm`, `execute_concurrent`,
/// `wait_for_tool_result`) route background commands through a single
/// function - and lets `execute_concurrent` in particular do so without
/// colliding with its long-held `&mut self.cmd_rx` borrow in the select.
pub(crate) struct BackgroundCtx<'a> {
    pub config: &'a EngineConfig,
    pub http_client: &'a reqwest::Client,
    pub dispatcher: &'a dyn ToolDispatcher,
    pub event_tx: &'a mpsc::UnboundedSender<EngineEvent>,
    pub host_tx: &'a mpsc::UnboundedSender<crate::host::HostCall>,
    pub bg_cancel: CancellationToken,
}

/// Dispatch a command that doesn't depend on turn state. Returns `None`
/// when the command was consumed; `Some(cmd)` when the caller should
/// handle it (turn-control, per-tool protocol, lifecycle, etc.). Today
/// only `EngineAsk` is handled here; new "anywhere" commands extend the
/// match and instantly become available at every call site.
pub(crate) fn dispatch_background_cmd(
    cmd: UiCommand,
    ctx: &BackgroundCtx<'_>,
) -> Option<UiCommand> {
    match cmd {
        UiCommand::EngineAsk {
            id,
            system,
            messages,
            model,
            response_format,
            reasoning_effort,
            fast_mode,
            tools,
            session_id,
            session_dir,
            stream,
            visible_retries,
        } => {
            spawn_engine_ask(
                ctx.config,
                ctx.http_client,
                ctx.dispatcher,
                AskTask {
                    id,
                    system,
                    messages,
                    model,
                    response_format,
                    reasoning_effort,
                    fast_mode,
                    tools,
                    session_id,
                    session_dir,
                    stream,
                    visible_retries,
                },
                ctx.event_tx,
                ctx.host_tx,
                ctx.bg_cancel.clone(),
            );
            None
        }
        other => Some(other),
    }
}

fn spawn_engine_ask(
    config: &EngineConfig,
    client: &reqwest::Client,
    dispatcher: &dyn ToolDispatcher,
    task: AskTask,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
    host_tx: &mpsc::UnboundedSender<crate::host::HostCall>,
    cancel: CancellationToken,
) {
    let AskTask {
        id,
        system,
        messages: supplied_messages,
        model,
        response_format,
        reasoning_effort,
        fast_mode,
        tools: supplied_tools,
        session_id,
        session_dir,
        stream,
        visible_retries,
    } = task;

    // Inherit-session is signalled by a non-empty supplied tool list
    // (the Lua side fills it with `app.lua.tool_defs(...)` only on that
    // path). When present, merge the engine's MCP defs in too so the
    // tools section matches the main turn byte-for-byte. Plain callers
    // (predict, title) pass an empty list and get an empty `tools`
    // field - sending MCP defs to them would waste tokens and break
    // their own cache prefix.
    let mut messages = supplied_messages;
    let tools = supplied_tools;
    let mcp_defs = if tools.is_empty() {
        Vec::new()
    } else {
        dispatcher.definitions()
    };
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
    let audit_mode = config.request_audit;
    let audit_host_tx = host_tx.clone();
    tokio::spawn(async move {
        messages.insert(0, protocol::Message::system(&system));
        let log_session_dir = session_dir.clone();
        let ask_id = id;

        let mut opts = ChatOptions::new(&cancel);
        let mut request_opts = ChatRequestOptions {
            cache: provider.default_cache_config(cache_ttl_long, Some(&session_id)),
            fast_mode,
            ..ChatRequestOptions::default()
        };
        if let Some(fmt) = response_format {
            request_opts.response_format = Some(ResponseFormat {
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
            .chain(mcp_defs)
            .collect();
        sort_tools_for_cache_stability(&mut tool_defs);

        let result = {
            let on_retry = |delay: std::time::Duration, attempt: u32| {
                let _ = tx.send(EngineEvent::Retrying {
                    delay_ms: delay.as_millis() as u64,
                    attempt,
                });
            };
            if visible_retries {
                opts.on_retry = Some(&on_retry);
            }
            let on_delta = |delta: ProviderStreamEvent| {
                if let ProviderStreamEvent::TextDelta(text) = delta {
                    let _ = tx.send(EngineEvent::EngineAskDelta {
                        id,
                        delta: text.to_string(),
                    });
                }
            };
            if stream {
                opts.on_delta = Some(&on_delta);
            }
            let log_pricing = pricing.clone();
            let on_attempt = move |info: RequestAttemptInfo<'_>| {
                let resolved = smelt_provider::resolve_pricing(
                    info.model,
                    &log_pricing.provider_type,
                    &log_pricing.api_base,
                    &log_pricing.model_config,
                );
                let ctx = crate::request_log::RequestContext {
                    request_id: ask_id,
                    kind: "engine_ask".to_string(),
                    turn_id: None,
                    ask_id: Some(ask_id),
                    history_len: None,
                    background: true,
                };
                dispatch_request_audit(
                    &audit_host_tx,
                    &log_session_dir,
                    ctx,
                    &info,
                    &resolved,
                    audit_mode,
                );
            };
            opts.on_attempt = Some(&on_attempt);
            provider
                .chat(
                    &messages,
                    &tool_defs,
                    &model_name,
                    reasoning_effort,
                    &request_opts,
                    &opts,
                )
                .await
        };

        match result {
            Ok(resp) => {
                pricing.emit(&tx, &model_name, resp.usage);
                let message = protocol::Message::assistant_with_reasoning(
                    resp.content.map(protocol::Content::text),
                    resp.reasoning_content,
                    resp.reasoning_details,
                    (!resp.tool_calls.is_empty()).then_some(resp.tool_calls),
                );
                let _ = tx.send(EngineEvent::EngineAskResponse {
                    id,
                    message: Some(message),
                    error: None,
                });
            }
            Err(e) => {
                let kind = classify_provider_error(&e);
                let message = e.to_string().replace('\n', " ");
                let _ = tx.send(EngineEvent::EngineAskResponse {
                    id,
                    message: None,
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
        ProviderError::QuotaExceeded { .. } => EngineAskErrorKind::Quota,
        ProviderError::Network(_) | ProviderError::Stream(_) => EngineAskErrorKind::Network,
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

fn provider_retry_at_ms(e: &ProviderError) -> Option<u64> {
    match e {
        ProviderError::RateLimited { resets_at } => {
            resets_at.map(|epoch| epoch.saturating_mul(1000))
        }
        ProviderError::QuotaExceeded { resets_at, .. } => {
            resets_at.map(|epoch| epoch.saturating_mul(1000))
        }
        _ => None,
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
        || lower.contains("model token limit")
}

fn build_provider(
    api: &ApiConfig,
    client: &reqwest::Client,
    api_base: Option<&str>,
    api_key: Option<&str>,
    model_overrides: Option<&protocol::ModelConfigOverrides>,
    clock: std::sync::Arc<dyn crate::clock::Clock>,
) -> EngineProvider {
    let model_config = match model_overrides {
        Some(o) => api.model_config.clone().with_overrides(o),
        None => api.model_config.clone(),
    };
    EngineProvider::new(
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
    /// Synthetic outcomes produced inside `classify_tools` itself - unknown
    /// tools, denied dispatch decisions. Folded into the assistant step's
    /// `invocations` at commit time so the invariant ("every dispatched
    /// tool_call has a paired outcome") holds even when no execution
    /// happens for that slot.
    inline_outcomes: Vec<(String, ToolOutcome)>,
}

/// Fold every produced outcome into a `Vec<ToolInvocation>` in the order
/// the LLM emitted the calls. Any call without a recorded outcome gets a
/// synthetic `interrupted` outcome - that case should be unreachable in
/// production (all execution paths produce an outcome), but the safety net
/// makes the on-disk + on-wire invariant true *by construction*, not by
/// careful code review.
///
/// Precedence on `call_id` collision: `slot` > `plugin` > `inline`. The
/// classify-then-execute pipeline routes each call to exactly one path,
/// so collisions shouldn't happen in practice - but if a path bug starts
/// double-writing, the explicit precedence keeps the result deterministic.
fn pair_invocations_in_order(
    calls: &[protocol::ToolCall],
    slot_outcomes: Vec<(String, ToolOutcome, Option<u64>)>,
    plugin_outcomes: Vec<(String, ToolOutcome, Option<u64>)>,
    inline_outcomes: Vec<(String, ToolOutcome)>,
) -> Vec<ToolInvocation> {
    let mut by_id: HashMap<String, (ToolOutcome, Option<u64>)> = HashMap::new();
    for (id, o, e) in slot_outcomes {
        by_id.insert(id, (o, e));
    }
    for (id, o, e) in plugin_outcomes {
        by_id.entry(id).or_insert((o, e));
    }
    for (id, o) in inline_outcomes {
        by_id.entry(id).or_insert((o, None));
    }
    calls
        .iter()
        .map(|tc| {
            let (result, elapsed_ms) = by_id.remove(&tc.id).unwrap_or_else(|| {
                // Safety net: should be unreachable because every execution
                // path (slot, plugin, sequential, inline-classify) writes
                // an outcome before pair_invocations_in_order runs. If it
                // fires, our reasoning was wrong - log so we can find out.
                crate::log::entry(
                    crate::log::Level::Warn,
                    "agent_invocation_missing",
                    &serde_json::json!({
                        "call_id": tc.id,
                        "tool": tc.function.name,
                    }),
                );
                (
                    ToolOutcome {
                        content: "interrupted: tool result missing at commit time".into(),
                        is_error: true,
                        metadata: None,
                    },
                    None,
                )
            });
            ToolInvocation {
                call_id: tc.id.clone(),
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
                result,
                elapsed_ms,
            }
        })
        .collect()
}

/// Log a warning when a host hook returns a `Vec<Message>` with assistant
/// `tool_calls` whose ids aren't satisfied by following `Role::Tool`
/// messages. The orphans are auto-repaired by `history_from_messages` -
/// this surface makes the repair visible so a misbehaving plugin can be
/// found instead of silently fixed.
fn warn_if_replacement_has_orphans(replacement: &[Message], site: &str) {
    use protocol::Role;
    let mut orphans: Vec<String> = Vec::new();
    let mut i = 0;
    while i < replacement.len() {
        if matches!(replacement[i].role, Role::Assistant) {
            if let Some(ref calls) = replacement[i].tool_calls {
                let mut satisfied: std::collections::HashSet<&str> =
                    std::collections::HashSet::new();
                let mut j = i + 1;
                while j < replacement.len() && matches!(replacement[j].role, Role::Tool) {
                    if let Some(ref id) = replacement[j].tool_call_id {
                        satisfied.insert(id.as_str());
                    }
                    j += 1;
                }
                for call in calls {
                    if !satisfied.contains(call.id.as_str()) {
                        orphans.push(call.id.clone());
                    }
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    if !orphans.is_empty() {
        crate::log::entry(
            crate::log::Level::Warn,
            "host_hook_orphan_tool_calls",
            &serde_json::json!({
                "site": site,
                "orphan_call_ids": orphans,
                "note": "auto-repaired with synthetic 'interrupted' results",
            }),
        );
    }
}

fn estimate_prompt_tokens(
    system_prompt: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
) -> u32 {
    let message_bytes = serde_json::to_vec(messages)
        .map(|v| v.len())
        .unwrap_or_default();
    let tool_bytes = serde_json::to_vec(tool_defs)
        .map(|v| v.len())
        .unwrap_or_default();
    let bytes = system_prompt.len() + message_bytes + tool_bytes;
    let tokens = bytes.div_ceil(4);
    tokens.min(u32::MAX as usize) as u32
}

struct PendingToolCall<'a> {
    tc: &'a protocol::ToolCall,
    args: HashMap<String, Value>,
    tool_start: Instant,
    is_sequential: bool,
}

struct Turn<'a> {
    provider: EngineProvider,
    dispatcher: &'a dyn ToolDispatcher,
    cmd_rx: &'a mut mpsc::UnboundedReceiver<UiCommand>,
    event_tx: &'a mpsc::UnboundedSender<EngineEvent>,
    /// Host-callback channel for provider middleware and request preparation.
    host_tx: &'a mpsc::UnboundedSender<crate::host::HostCall>,
    config: &'a EngineConfig,
    http_client: &'a reqwest::Client,
    cancel: CancellationToken,
    bg_cancel: CancellationToken,
    /// Committed conversation history. Invariant: every `Assistant` step
    /// either has no `invocations` (terminal) or has a `ToolOutcome` paired
    /// with every `ToolCall` the LLM emitted. There is no in-flight tool
    /// state here - that lives on stack-locals during the run loop's
    /// dispatch phase and is folded into a single `HistoryItem::Assistant`
    /// at commit time.
    history: Vec<HistoryItem>,
    history_coordinates: protocol::ModelHistoryCoordinates,
    mode: AgentMode,
    reasoning_effort: ReasoningEffort,
    fast_mode: bool,
    turn_id: u64,
    model: String,
    display: Option<String>,
    system_prompt: String,
    tools: Vec<protocol::ToolDef>,
    permission_overrides: Option<protocol::PermissionOverrides>,
    pending_history_items: Vec<HistoryItem>,
    next_history_changed_from: usize,
    /// Stable per-session identifier sent as OpenAI's `prompt_cache_key`
    /// to anchor cache routing across all turns in this session.
    session_id: String,
    /// On-disk directory for this session. Used to write the SQLite request
    /// audit for introspection.
    session_dir: std::path::PathBuf,
    started_at: Instant,
    tps_samples: Vec<f64>,
    tool_elapsed: HashMap<String, u64>,
}

enum HostCallResult<T> {
    Replied(T),
    Cancelled,
    Dropped,
}

enum PrepareRequestOutcome {
    Continue,
    Restart,
    Abort(String),
    Cancelled,
}

impl<'a> Turn<'a> {
    fn emit(&self, event: EngineEvent) {
        let _ = self.event_tx.send(event);
    }

    fn public_history_len(&self) -> usize {
        self.history
            .len()
            .saturating_sub(self.system_history_offset())
    }

    fn system_history_offset(&self) -> usize {
        self.history
            .first()
            .is_some_and(|item| matches!(item, HistoryItem::System { .. })) as usize
    }

    fn raw_history_index(&self, public_idx: usize) -> usize {
        public_idx.saturating_add(self.system_history_offset())
    }

    fn mark_history_changed_from(&mut self, public_idx: usize) {
        self.next_history_changed_from = self.next_history_changed_from.min(public_idx);
    }

    fn mark_append_history_changed(&mut self) {
        self.mark_history_changed_from(self.public_history_len());
    }

    /// True for commands that inject or remove user messages from the
    /// current turn. These are speculative until the operation that
    /// received them succeeds; callers decide whether to apply them.
    fn is_turn_injection(cmd: &UiCommand) -> bool {
        matches!(cmd, UiCommand::Steer { .. } | UiCommand::Unsteer { .. })
    }

    /// Drain `deferred` back through `handle_turn_cmd`.
    fn apply_deferred_turn_cmds(&mut self, deferred: Vec<UiCommand>) {
        for cmd in deferred {
            self.handle_turn_cmd(cmd);
        }
    }

    /// Fire a `HostCall` and await its `oneshot::Sender<Reply>`.
    ///
    /// Commands received while waiting are handled immediately except for
    /// `Steer`/`Unsteer` injections, which are collected and applied only
    /// if the host replies successfully. This keeps speculative user messages
    /// from surviving an error or cancellation.
    async fn host_call<Reply, F>(
        &mut self,
        build: F,
        mut apply_on_success: impl FnMut(&mut Self, Vec<UiCommand>),
    ) -> HostCallResult<Reply>
    where
        F: FnOnce(tokio::sync::oneshot::Sender<Reply>) -> crate::host::HostCall,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.host_tx.send(build(tx)).is_err() {
            return HostCallResult::Dropped;
        }
        tokio::pin!(rx);
        let mut deferred: Vec<UiCommand> = Vec::new();
        loop {
            tokio::select! {
                res = &mut rx => {
                    let result = match res {
                        Ok(reply) => HostCallResult::Replied(reply),
                        Err(_) => HostCallResult::Dropped,
                    };
                    if matches!(result, HostCallResult::Replied(_)) {
                        apply_on_success(self, deferred);
                    }
                    return result;
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    if Self::is_turn_injection(&cmd) {
                        deferred.push(cmd);
                    } else {
                        self.handle_turn_cmd(cmd);
                    }
                    if self.cancel.is_cancelled() {
                        return HostCallResult::Cancelled;
                    }
                }
                else => return HostCallResult::Dropped,
            }
        }
    }

    /// Run `smelt.provider.middleware{on_response=...}` hooks against
    /// the assembled assistant `Message`. Returns the replacement when
    /// any hook produced one; otherwise the original.
    async fn apply_response_hooks(&mut self, message: Message) -> Message {
        match self
            .host_call(
                |reply| crate::host::HostCall::ProviderResponse {
                    message: message.clone(),
                    reply,
                },
                |this, deferred| this.apply_deferred_turn_cmds(deferred),
            )
            .await
        {
            HostCallResult::Replied(Some(replacement)) => replacement,
            HostCallResult::Replied(None) | HostCallResult::Dropped | HostCallResult::Cancelled => {
                message
            }
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

    /// Append a user turn, redacting content first when `redact_secrets` is on.
    fn push_user(&mut self, mut content: Content, display: Option<String>) {
        let display = if self.config.redact_secrets {
            crate::redact::redact_content(&mut content);
            display.map(|text| crate::redact::redact(&text))
        } else {
            display
        };
        self.mark_append_history_changed();
        self.history.push(HistoryItem::User { content, display });
    }

    /// Append current-turn content that may be a synthetic internal note.
    fn push_turn_content(&mut self, mut content: Content, display: Option<String>) {
        let display = if self.config.redact_secrets {
            crate::redact::redact_content(&mut content);
            display.map(|text| crate::redact::redact(&text))
        } else {
            display
        };
        let mut item = protocol::history_item_from_user_content(content);
        if let HistoryItem::User { display: slot, .. } = &mut item {
            *slot = display;
        }
        self.mark_append_history_changed();
        self.history.push(item);
    }

    /// Append an assistant step atomically. When `invocations` is non-empty,
    /// every entry already carries its `ToolOutcome` - the only way to
    /// satisfy `AssistantStep`'s shape - so the on-disk and on-wire
    /// representations can never carry an orphan tool_use.
    fn push_assistant_step(&mut self, mut step: AssistantStep) {
        if self.config.redact_secrets {
            for inv in &mut step.invocations {
                let redacted = crate::redact::redact(&inv.result.content);
                if redacted != inv.result.content {
                    inv.result.content = redacted;
                }
            }
        }
        self.mark_append_history_changed();
        self.history.push(HistoryItem::Assistant(step));
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
                crate::build_system_prompt(
                    self.config.system_prompt_behavior,
                    crate::SystemPromptCapabilities::from_tool_calling(
                        self.provider.tool_calling(),
                    ),
                    self.config.instructions.as_deref(),
                    self.config.skill_section.as_deref(),
                )
            });
        self.system_prompt = new;
        if let Some(first) = self.history.first_mut() {
            if matches!(first, HistoryItem::System { .. }) {
                *first = HistoryItem::system(&self.system_prompt);
            }
        }
    }

    fn emit_history_appended_from(&mut self, first_index: usize) {
        let start = self.raw_history_index(first_index).min(self.history.len());
        let items = self.history[start..].to_vec();
        if items.is_empty() {
            return;
        }
        self.next_history_changed_from = self.public_history_len();
        self.emit(EngineEvent::HistoryAppended {
            turn_id: self.turn_id,
            delta: protocol::CanonicalHistoryDelta {
                first_index: self
                    .history_coordinates
                    .canonical_index(protocol::ModelHistoryIndex::new(first_index)),
                items,
            },
        });
    }

    fn replace_model_history(
        &mut self,
        history: Vec<HistoryItem>,
        coordinates: protocol::ModelHistoryCoordinates,
    ) {
        self.mark_history_changed_from(0);
        self.history_coordinates = coordinates;
        self.history.truncate(1);
        self.history.extend(history);
        self.emit_messages_snapshot();
    }

    /// Emit the canonical suffix affected by model-visible history changes.
    /// Synthetic checkpoint items are omitted from the payload.
    fn emit_messages_snapshot(&mut self) {
        let items: Vec<HistoryItem> = self
            .history
            .iter()
            .filter(|i| !matches!(i, HistoryItem::System { .. }))
            .cloned()
            .collect();
        let first_changed_index = self.next_history_changed_from.min(items.len());
        self.next_history_changed_from = items.len();
        self.emit(EngineEvent::HistoryUpdated {
            turn_id: self.turn_id,
            update: self
                .history_coordinates
                .canonical_delta(protocol::ModelHistoryIndex::new(first_changed_index), items),
        });
    }

    fn queue_history_item(&mut self, append: protocol::HistoryAppend) {
        match &append.policy {
            protocol::HistoryAppendPolicy::ReplaceContextNote { name } => {
                if protocol::replace_context_note(
                    &mut self.pending_history_items,
                    &append.item,
                    name,
                ) {
                    return;
                }
                if protocol::replace_context_note(&mut self.history, &append.item, name) {
                    self.mark_history_changed_from(0);
                    self.emit_messages_snapshot();
                    return;
                }
                protocol::apply_history_append(&mut self.pending_history_items, &append);
                return;
            }
            protocol::HistoryAppendPolicy::RemoveContextNote { name } => {
                if protocol::remove_context_note(&mut self.pending_history_items, name) {
                    return;
                }
                if protocol::remove_context_note(&mut self.history, name) {
                    self.mark_history_changed_from(0);
                    self.emit_messages_snapshot();
                }
                return;
            }
            _ => {}
        }

        let replace_note_kind = append.replacement_note_kind();
        if replace_note_kind == Some(protocol::HistoryNoteKind::ModeChange) {
            if let Some(idx) = self
                .pending_history_items
                .iter()
                .position(|item| item.note_kind() == Some(protocol::HistoryNoteKind::ModeChange))
            {
                if mode_append_returns_to_base(&append) {
                    self.pending_history_items.remove(idx);
                } else {
                    self.pending_history_items[idx] = append.item;
                }
                return;
            }
            if last_note_kind(&self.history) == Some(protocol::HistoryNoteKind::ModeChange) {
                let changed_from = self.public_history_len().saturating_sub(1);
                let result = protocol::apply_history_append(&mut self.history, &append);
                if result != protocol::HistoryAppendResult::Unchanged {
                    self.mark_history_changed_from(changed_from);
                    self.emit_messages_snapshot();
                }
                return;
            }
        } else if let Some(kind) = replace_note_kind {
            if protocol::replace_last_note_kind(&mut self.pending_history_items, &append.item, kind)
            {
                return;
            }
            if protocol::replace_last_note_kind(&mut self.history, &append.item, kind) {
                self.mark_history_changed_from(0);
                self.emit_messages_snapshot();
                return;
            }
        }

        protocol::apply_history_append(&mut self.pending_history_items, &append);
    }

    fn apply_pending_history_items_for_request(&mut self) {
        if self.pending_history_items.is_empty() {
            return;
        }
        let items = std::mem::take(&mut self.pending_history_items);
        let first_index = self.public_history_len();
        self.mark_append_history_changed();
        self.history.extend(items);
        self.emit_history_appended_from(first_index);
    }

    /// Commit a streamed-but-cancelled assistant message. The model never
    /// asked for any tools, so this is a terminal step - `invocations` is
    /// empty by construction.
    fn commit_partial_assistant(&mut self, text: String, reasoning: String) -> Option<usize> {
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
            let first_index = self.public_history_len();
            self.push_assistant_step(AssistantStep::terminal(content, reasoning, Vec::new()));
            Some(first_index)
        } else {
            None
        }
    }

    fn emit_turn_complete(&mut self, interrupted: bool) {
        let meta = self.build_meta(interrupted);
        let history = if interrupted {
            let items: Vec<HistoryItem> = std::mem::take(&mut self.history)
                .into_iter()
                .filter(|i| !matches!(i, HistoryItem::System { .. }))
                .collect();
            let first_changed_index = self.next_history_changed_from.min(items.len());
            self.next_history_changed_from = items.len();
            Some(
                self.history_coordinates
                    .canonical_delta(protocol::ModelHistoryIndex::new(first_changed_index), items),
            )
        } else {
            self.history.clear();
            None
        };
        self.emit(EngineEvent::TurnComplete {
            turn_id: self.turn_id,
            history,
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
            display_tps: None,
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
        self.provider = EngineProvider::new(
            api_base,
            api_key,
            &provider_type,
            self.http_client.clone(),
            std::sync::Arc::clone(&self.config.clock),
        )
        .with_model_config(self.config.api.model_config.clone());
    }

    async fn prepare_request_with_host(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> PrepareRequestOutcome {
        let history_without_system: Vec<HistoryItem> =
            self.history.iter().skip(1).cloned().collect();
        let request_view = protocol::history_to_messages(&history_without_system);
        let estimated_tokens =
            estimate_prompt_tokens(&self.system_prompt, &request_view, tool_defs);
        let apply_deferred = |this: &mut Self, deferred: Vec<UiCommand>| {
            this.apply_deferred_turn_cmds(deferred);
        };
        let decision = self
            .host_call(
                |reply| crate::host::HostCall::PrepareRequest {
                    messages: request_view,
                    estimated_tokens,
                    reply,
                },
                apply_deferred,
            )
            .await;
        let (replacement, coordinates) = match decision {
            HostCallResult::Replied(crate::host::HostRequestDecision::Continue)
            | HostCallResult::Dropped => return PrepareRequestOutcome::Continue,
            HostCallResult::Cancelled => return PrepareRequestOutcome::Cancelled,
            HostCallResult::Replied(crate::host::HostRequestDecision::Abort(message)) => {
                return PrepareRequestOutcome::Abort(message);
            }
            HostCallResult::Replied(crate::host::HostRequestDecision::Replace {
                messages,
                coordinates,
            }) => (messages, coordinates),
        };
        if replacement.is_empty() {
            log::entry(
                log::Level::Warn,
                "prepare_request_empty_replacement",
                &serde_json::json!({}),
            );
            return PrepareRequestOutcome::Continue;
        }
        warn_if_replacement_has_orphans(&replacement, "prepare_request");
        let new = protocol::history_from_messages(replacement);
        log::entry(
            log::Level::Info,
            "prepare_request_replaced",
            &serde_json::json!({
                "estimated_tokens_before": estimated_tokens,
                "new_message_count": new.len() + 1,
            }),
        );
        self.replace_model_history(new, coordinates);
        PrepareRequestOutcome::Restart
    }

    /// Bundle of refs that out-of-band command dispatch needs. Built on
    /// demand so callsites that already hold a `&mut` borrow on
    /// `self.cmd_rx` (notably `execute_concurrent`) can bypass the
    /// borrow checker by pre-binding the ctx outside the select loop.
    fn bg_ctx(&self) -> BackgroundCtx<'_> {
        BackgroundCtx {
            config: self.config,
            http_client: self.http_client,
            dispatcher: self.dispatcher,
            event_tx: self.event_tx,
            host_tx: self.host_tx,
            bg_cancel: self.bg_cancel.clone(),
        }
    }

    fn apply_reasoning_effort(&mut self, effort: ReasoningEffort) {
        // Kimi's Anthropic-compatible endpoint rejects requests when reasoning
        // is toggled mid-turn because earlier assistant tool-call messages lack
        // the reasoning_content the backend now expects. The UI/session state
        // already records the new choice; keep this turn on its starting effort.
        if self.provider.supports_mid_turn_reasoning_changes() {
            self.reasoning_effort = effort;
        }
    }

    fn handle_turn_cmd(&mut self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::Steer { text } => {
                self.emit(EngineEvent::Steered {
                    text: text.clone(),
                    count: 1,
                });
                let first_index = self.public_history_len();
                self.push_user(Content::text(text), None);
                self.emit_history_appended_from(first_index);
                true
            }
            UiCommand::Unsteer { count } => {
                for _ in 0..count {
                    if let Some(pos) = self
                        .history
                        .iter()
                        .rposition(|i| matches!(i, HistoryItem::User { .. }))
                    {
                        self.mark_history_changed_from(pos.saturating_sub(1));
                        self.history.remove(pos);
                    }
                }
                self.emit_messages_snapshot();
                true
            }
            UiCommand::AppendHistoryItem { append } => {
                self.queue_history_item(append);
                true
            }
            UiCommand::SetReasoningEffort { effort } => {
                self.apply_reasoning_effort(effort);
                true
            }
            UiCommand::SetFastMode { enabled } => {
                self.fast_mode = enabled;
                true
            }
            UiCommand::SetMode { mode } => {
                self.mode = mode;
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
                self.bg_cancel.cancel();
                true
            }
            other => dispatch_background_cmd(other, &self.bg_ctx()).is_none(),
        }
    }

    fn push_current_turn_input(&mut self, input: protocol::StartTurnInput) {
        match input {
            protocol::StartTurnInput::User { content, .. } if !content.is_empty() => {
                self.push_turn_content(content, self.display.clone());
            }
            protocol::StartTurnInput::Note { note } => {
                self.mark_append_history_changed();
                self.history.push(HistoryItem::note(note));
            }
            protocol::StartTurnInput::User { .. } => {}
        }
    }

    async fn run(&mut self, input: protocol::StartTurnInput, history: Vec<HistoryItem>) {
        self.provider.reset_turn_state();
        self.history = Vec::with_capacity(history.len() + 2);
        self.history.push(HistoryItem::system(&self.system_prompt));
        self.history.extend(history);
        self.next_history_changed_from = self.public_history_len();
        self.push_current_turn_input(input);
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
                            .is_visible(d.function.name.as_str(), self.mode.clone())
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
                sort_tools_for_cache_stability(&mut defs);
                defs
            } else {
                Vec::new()
            };

            if self.cancel.is_cancelled() {
                self.emit_turn_complete(true);
                return;
            }

            self.apply_pending_history_items_for_request();

            match self.prepare_request_with_host(&tool_defs).await {
                PrepareRequestOutcome::Restart => continue,
                PrepareRequestOutcome::Continue => {}
                PrepareRequestOutcome::Cancelled => {
                    self.emit_turn_complete(true);
                    return;
                }
                PrepareRequestOutcome::Abort(message) => {
                    self.emit(EngineEvent::TurnError {
                        message,
                        kind: None,
                        retry_at_ms: None,
                    });
                    self.emit_turn_complete(false);
                    return;
                }
            }
            self.drain_commands();
            if self.cancel.is_cancelled() {
                self.emit_turn_complete(true);
                return;
            }

            let (result, partial_text, partial_reasoning) = self.call_llm(&tool_defs).await;
            let (resp, had_injected) = match result {
                Ok(r) => r,
                Err(ProviderError::Cancelled) => {
                    if let Some(first_index) =
                        self.commit_partial_assistant(partial_text, partial_reasoning)
                    {
                        self.emit_history_appended_from(first_index);
                    }
                    self.emit_turn_complete(true);
                    return;
                }
                Err(ProviderError::QuotaExceeded { body, resets_at }) => {
                    log::entry(
                        log::Level::Warn,
                        "agent_stop",
                        &serde_json::json!({"reason": "quota_exceeded", "error": body}),
                    );
                    self.emit(EngineEvent::TurnError {
                        message: quota_exceeded_message().to_string(),
                        kind: Some(EngineAskErrorKind::Quota),
                        retry_at_ms: resets_at.map(|epoch| epoch.saturating_mul(1000)),
                    });
                    self.emit_turn_complete(false);
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
                        // conversation. On success we swap history
                        // (preserving the system prompt at index 0)
                        // and re-enter the loop transparently. The view
                        // sent to the host is the wire-shape; the
                        // returned `Vec<Message>` is folded back into
                        // `HistoryItem`s, which repairs any orphan
                        // tool_use the compaction plugin might emit.
                        let recovery_view: Vec<Message> = protocol::history_to_messages(
                            &self.history.iter().skip(1).cloned().collect::<Vec<_>>(),
                        );
                        let recovery_decision = self
                            .host_call(
                                |reply| crate::host::HostCall::RecoverFromContextLimit {
                                    messages: recovery_view,
                                    reply,
                                },
                                |this, deferred| this.apply_deferred_turn_cmds(deferred),
                            )
                            .await;
                        match recovery_decision {
                            HostCallResult::Cancelled => {
                                self.emit_turn_complete(true);
                                return;
                            }
                            HostCallResult::Replied(crate::host::HostRequestDecision::Abort(
                                message,
                            )) => {
                                self.emit(EngineEvent::TurnError {
                                    message,
                                    kind: None,
                                    retry_at_ms: None,
                                });
                                self.emit_turn_complete(false);
                                return;
                            }
                            HostCallResult::Replied(
                                crate::host::HostRequestDecision::Replace {
                                    messages: shorter,
                                    coordinates,
                                },
                            ) => {
                                warn_if_replacement_has_orphans(&shorter, "context_limit_recovery");
                                let new = protocol::history_from_messages(shorter);
                                log::entry(
                                    log::Level::Info,
                                    "context_limit_recovered",
                                    &serde_json::json!({"new_message_count": new.len() + 1}),
                                );
                                self.replace_model_history(new, coordinates);
                                continue;
                            }
                            HostCallResult::Replied(crate::host::HostRequestDecision::Continue)
                            | HostCallResult::Dropped => {}
                        }
                    }
                    let message = if is_ctx {
                        "Context limit reached. Run /compact and retry.".to_string()
                    } else {
                        error_msg
                    };
                    self.emit(EngineEvent::TurnError {
                        message,
                        kind: Some(classify_provider_error(&e)),
                        retry_at_ms: provider_retry_at_ms(&e),
                    });
                    self.emit_turn_complete(false);
                    return;
                }
            };

            if let Some(tps) = resp.tokens_per_sec {
                self.tps_samples.push(tps);
            }
            if resp.usage.has_any() {
                send_usage(
                    self.event_tx,
                    &self.config.api.provider_type,
                    &self.config.api.base,
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
            let reasoning_parts = resp.reasoning_parts;
            let reasoning_details = resp.reasoning_details;

            // Injected message arrived during the LLM call; loop so the model can respond.
            if had_injected && tool_calls.is_empty() {
                continue;
            }

            // Streaming already delivered deltas; only emit batch events for non-streaming.
            if partial_text.is_empty() && partial_reasoning.is_empty() {
                if reasoning_parts.is_empty() {
                    if let Some(ref reasoning) = reasoning {
                        let trimmed = reasoning.trim();
                        if !trimmed.is_empty() {
                            self.emit(EngineEvent::Reasoning {
                                kind: ReasoningKind::Raw,
                                title: None,
                                content: trimmed.to_string(),
                            });
                        }
                    }
                } else {
                    for part in &reasoning_parts {
                        let (title, content) = normalize_reasoning_part(part.kind, &part.content);
                        self.emit(EngineEvent::Reasoning {
                            kind: part.kind,
                            title,
                            content,
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
                    && matches!(
                        self.history.last(),
                        Some(HistoryItem::Assistant(t)) if !t.invocations.is_empty()
                    );

                if is_empty && empty_retries < MAX_EMPTY_RETRIES {
                    empty_retries += 1;
                    log::entry(
                        log::Level::Warn,
                        "empty_response_retry",
                        &serde_json::json!({ "attempt": empty_retries }),
                    );
                    continue;
                }

                let hooked = self
                    .apply_response_hooks(Message::assistant_with_reasoning(
                        content,
                        reasoning,
                        reasoning_details,
                        None,
                    ))
                    .await;
                let turn = AssistantStep::terminal(
                    hooked.content,
                    hooked.reasoning_content,
                    hooked.reasoning_details.unwrap_or_default(),
                );
                let first_index = self.public_history_len();
                self.push_assistant_step(turn);
                self.emit_history_appended_from(first_index);
                self.emit_turn_complete(false);
                return;
            }

            empty_retries = 0;
            let hooked = self
                .apply_response_hooks(Message::assistant_with_reasoning(
                    content,
                    reasoning,
                    reasoning_details,
                    Some(tool_calls.clone()),
                ))
                .await;
            // Capture the (possibly hook-mutated) in-flight assistant shape
            // for atomic commit. The `tool_calls` that classify_tools will
            // dispatch are read from the hook output too - plugins that
            // synthesize calls must do so via the hook return value.
            let post_hook_content = hooked.content;
            let post_hook_reasoning = hooked.reasoning_content;
            let post_hook_reasoning_blocks = hooked.reasoning_details.unwrap_or_default();
            let dispatched_calls = hooked.tool_calls.unwrap_or_default();

            let mut plan = self.classify_tools(&dispatched_calls);
            let mut completed: Vec<Option<ToolResult>> =
                (0..plan.slots.len()).map(|_| None).collect();
            let (cancelled, deferred, mut plugin_outcomes) =
                self.execute_concurrent(&mut plan, &mut completed).await;
            let seq_outcomes = self.run_sequential(&plan).await;
            plugin_outcomes.extend(seq_outcomes);
            if cancelled {
                self.mark_unfinished_cancelled(&plan, &mut completed);
            }
            let slot_outcomes = self.gather_slot_results(&plan, completed);
            let inline_outcomes = std::mem::take(&mut plan.inline_outcomes);
            // All execution paths have written their outcome into one of
            // these three vecs. Pair-and-order folds them onto the
            // dispatched_calls list so the resulting `Vec<ToolInvocation>`
            // has the same length and order as the LLM's emitted tool_uses.
            // The history can never observe an unpaired tool_use.
            let mut invocations = pair_invocations_in_order(
                &dispatched_calls,
                slot_outcomes,
                plugin_outcomes,
                inline_outcomes,
            );
            crate::result_dedup::apply_in_place(&mut invocations, &self.history);
            crate::trim::budget_tool_invocations(&mut invocations);
            let first_index = self.public_history_len();
            self.push_assistant_step(AssistantStep::with_invocations(
                post_hook_content,
                post_hook_reasoning,
                post_hook_reasoning_blocks,
                invocations,
            ));
            self.emit_history_appended_from(first_index);
            for cmd in deferred {
                self.handle_turn_cmd(cmd);
            }
        }
    }

    fn send_tool_started_for_call(
        event_tx: &mpsc::UnboundedSender<EngineEvent>,
        tc: &protocol::ToolCall,
        args: &HashMap<String, Value>,
    ) {
        let _ = event_tx.send(EngineEvent::ToolStarted {
            call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            args: args.clone(),
        });
    }

    fn emit_tool_started_for_call(&self, tc: &protocol::ToolCall, args: &HashMap<String, Value>) {
        Self::send_tool_started_for_call(self.event_tx, tc, args);
    }

    fn send_tool_dispatch_for_call(
        event_tx: &mpsc::UnboundedSender<EngineEvent>,
        request_id: u64,
        tc: &protocol::ToolCall,
        args: &HashMap<String, Value>,
    ) {
        Self::send_tool_started_for_call(event_tx, tc, args);
        let _ = event_tx.send(EngineEvent::ToolDispatch {
            request_id,
            call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            args: args.clone(),
        });
    }

    fn send_tool_rejected_for_call(
        event_tx: &mpsc::UnboundedSender<EngineEvent>,
        tc: &protocol::ToolCall,
        args: &HashMap<String, Value>,
        summary: protocol::StyledLines,
        result: ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        let _ = event_tx.send(EngineEvent::ToolRejected {
            call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            args: args.clone(),
            summary,
            result,
            elapsed_ms,
        });
    }

    fn emit_tool_rejected_for_call(
        &self,
        tc: &protocol::ToolCall,
        args: &HashMap<String, Value>,
        summary: protocol::StyledLines,
        result: ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        Self::send_tool_rejected_for_call(self.event_tx, tc, args, summary, result, elapsed_ms);
    }

    fn blocked_tool_outcome() -> ToolOutcome {
        ToolOutcome {
            content: "The user's permission settings blocked this tool call. \
                      Try a different approach or ask the user for guidance."
                .to_string(),
            is_error: false,
            metadata: None,
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
            inline_outcomes: Vec::new(),
        };

        // Cancellation isn't checked inside the per-tool loop: if it
        // were, the unprocessed tool_calls would never produce
        // outcomes, leaving the committed assistant step with orphan
        // calls. Letting classification finish is cheap, and
        // `execute_concurrent` short-circuits the actual work on
        // cancellation by draining its pending vecs with a synthetic
        // `cancelled` outcome (which the invariant requires).
        // Tool evaluation replies for plugin calls share `cmd_rx` with user commands.
        // Leave them queued until `execute_concurrent` has the full request-id plan.
        for tc in tool_calls {
            let args: HashMap<String, Value> =
                serde_json::from_str(&tc.function.arguments).unwrap_or_default();

            let tool_start = self.config.clock.instant_now();

            let tool = self.tools.iter().find(|pt| {
                pt.name == tc.function.name
                    && (pt.override_core || !self.dispatcher.contains(&tc.function.name))
            });
            if let Some(pt) = tool {
                let is_sequential =
                    matches!(pt.execution_mode, protocol::ToolExecutionMode::Sequential);
                let request_id = next_request_id();
                self.emit(EngineEvent::ToolEvaluationRequest {
                    request_id,
                    call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
                    args: args.clone(),
                    mode: self.mode.clone(),
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
                continue;
            }

            let evaluation = match self.dispatcher.evaluate_tool_call(
                &tc.function.name,
                &args,
                self.mode.clone(),
                self.permission_overrides.as_ref(),
            ) {
                Some(h) => h,
                None => {
                    let outcome = ToolOutcome {
                        content: format!("unknown tool: {}", tc.function.name),
                        is_error: true,
                        metadata: None,
                    };
                    self.emit_tool_rejected_for_call(
                        tc,
                        &args,
                        protocol::StyledLines::empty(),
                        outcome.clone(),
                        Some(self.elapsed_ms_since(tool_start)),
                    );
                    plan.inline_outcomes.push((tc.id.clone(), outcome));
                    continue;
                }
            };

            let protocol::ToolEvaluation { decision, metadata } = evaluation;
            let summary = metadata.summary.clone();
            let idx = plan.slots.len();
            match decision {
                Decision::Allow => {
                    self.emit_tool_started_for_call(tc, &args);
                    plan.slots.push(ToolSlot {
                        tc,
                        args,
                        confirm_msg: None,
                        start: tool_start,
                    });
                    plan.ready.push(idx);
                }
                Decision::Deny => {
                    let outcome = Self::blocked_tool_outcome();
                    self.emit_tool_rejected_for_call(tc, &args, summary, outcome.clone(), None);
                    plan.inline_outcomes.push((tc.id.clone(), outcome));
                }
                Decision::Error(ref err) => {
                    let outcome = ToolOutcome {
                        content: err.clone(),
                        is_error: true,
                        metadata: None,
                    };
                    self.emit_tool_rejected_for_call(tc, &args, summary, outcome.clone(), None);
                    plan.inline_outcomes.push((tc.id.clone(), outcome));
                }
                Decision::Ask => {
                    let request_id = next_request_id();
                    self.emit(EngineEvent::RequestPermission {
                        request_id,
                        call_id: tc.id.clone(),
                        tool_name: tc.function.name.clone(),
                        args: args.clone(),
                        approval_patterns: metadata.approval_patterns,
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
    ) -> (
        bool,
        Vec<UiCommand>,
        Vec<(String, ToolOutcome, Option<u64>)>,
    ) {
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

        // Side-call futures from `smelt.tools.call` - don't count against `outstanding`.
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
        // Pre-bind disjoint immutable refs so the catch-all arm can route
        // background commands (today: EngineAsk) through
        // `dispatch_background_cmd` without colliding with `cmd_rx`'s
        // long-held `&mut self.cmd_rx` borrow. Without this, any tool
        // that parks on `smelt.engine.ask` (notably `web_fetch`) would
        // deadlock the turn: the command would land in cmd_rx, fall into
        // a silent `_ => {}`, and never produce an `EngineAskResponse`.
        let bg_ctx = BackgroundCtx {
            config: self.config,
            http_client: self.http_client,
            dispatcher,
            event_tx: self.event_tx,
            host_tx: self.host_tx,
            bg_cancel: self.bg_cancel.clone(),
        };
        let mut deferred: Vec<UiCommand> = Vec::new();
        let mut speculative: Vec<UiCommand> = Vec::new();
        let mut tool_results: Vec<(String, ToolOutcome, Option<u64>)> = Vec::new();

        let cancelled = loop {
            if outstanding == 0 {
                break false;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break true,
                Some(cmd) = cmd_rx.recv() => match cmd {
                    UiCommand::Cancel => {
                        cancel.cancel();
                        self.bg_cancel.cancel();
                    }
                    UiCommand::PermissionDecision { request_id, approved, message } => {
                        if let Some(pos) = plan
                            .pending_perms
                            .iter()
                            .position(|(_, rid)| *rid == request_id)
                        {
                            let (idx, _) = plan.pending_perms.swap_remove(pos);
                            if approved {
                                plan.slots[idx].confirm_msg = message;
                                Self::send_tool_started_for_call(
                                    self.event_tx,
                                    plan.slots[idx].tc,
                                    &plan.slots[idx].args,
                                );
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
                                    outstanding -= 1;
                                } else {
                                    let rid = next_request_id();
                                    Self::send_tool_dispatch_for_call(
                                        self.event_tx,
                                        rid,
                                        pending.tc,
                                        &pending.args,
                                    );
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
                                let outcome = ToolOutcome {
                                    content: denial,
                                    is_error: false,
                                    metadata: None,
                                };
                                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                    call_id: pending.tc.id.clone(),
                                    result: outcome.clone(),
                                    elapsed_ms,
                                });
                                tool_results.push((pending.tc.id.clone(), outcome, elapsed_ms));
                                outstanding -= 1;
                            }
                        }
                    }
                    UiCommand::ToolEvaluationResponse { request_id, evaluation } => {
                        if let Some(pos) = plan
                            .pending_tool_hooks
                            .iter()
                            .position(|(rid, _)| *rid == request_id)
                        {
                            let (_, pending) = plan.pending_tool_hooks.swap_remove(pos);
                            let protocol::ToolEvaluation { decision, metadata } = evaluation;
                            match decision {
                                Decision::Allow => {
                                    if pending.is_sequential {
                                        plan.sequential_tools.push((
                                            pending.tc,
                                            pending.args,
                                            pending.tool_start,
                                        ));
                                        outstanding -= 1;
                                    } else {
                                        let rid = next_request_id();
                                        Self::send_tool_dispatch_for_call(
                                            self.event_tx,
                                            rid,
                                            pending.tc,
                                            &pending.args,
                                        );
                                        plan.pending_tools.push((
                                            rid,
                                            pending.tc.id.clone(),
                                            pending.tool_start,
                                        ));
                                    }
                                }
                                Decision::Deny => {
                                    let tool_start = pending.tool_start;
                                    let elapsed_ms = Some(elapsed_ms_since(tool_start));
                                    let outcome = Self::blocked_tool_outcome();
                                    let summary = metadata.summary.clone();
                                    Self::send_tool_rejected_for_call(
                                        self.event_tx,
                                        pending.tc,
                                        &pending.args,
                                        summary,
                                        outcome.clone(),
                                        elapsed_ms,
                                    );
                                    tool_results.push((pending.tc.id.clone(), outcome, elapsed_ms));
                                    outstanding -= 1;
                                }
                                Decision::Error(ref err) => {
                                    let tool_start = pending.tool_start;
                                    let elapsed_ms = Some(elapsed_ms_since(tool_start));
                                    let outcome = ToolOutcome {
                                        content: err.clone(),
                                        is_error: true,
                                        metadata: None,
                                    };
                                    let summary = metadata.summary.clone();
                                    Self::send_tool_rejected_for_call(
                                        self.event_tx,
                                        pending.tc,
                                        &pending.args,
                                        summary,
                                        outcome.clone(),
                                        elapsed_ms,
                                    );
                                    tool_results.push((pending.tc.id.clone(), outcome, elapsed_ms));
                                    outstanding -= 1;
                                }
                                Decision::Ask => {
                                    let summary = metadata.summary.clone();
                                    let rid = next_request_id();
                                    let _ = self
                                        .event_tx
                                        .send(EngineEvent::RequestPermission {
                                            request_id: rid,
                                            call_id: pending.tc.id.clone(),
                                            tool_name: pending.tc.function.name.clone(),
                                            args: pending.args.clone(),
                                            approval_patterns: metadata.approval_patterns,
                                            summary,
                                        });
                                    plan.pending_tool_perms.push((rid, pending));
                                }
                            }
                        }
                    }
                    UiCommand::ToolResult { request_id, call_id, content, is_error, metadata } => {
                        if let Some(pos) = plan
                            .pending_tools
                            .iter()
                            .position(|(rid, _, _)| *rid == request_id)
                        {
                            let (_, _, start) = plan.pending_tools.swap_remove(pos);
                            let elapsed_ms = Some(elapsed_ms_since(start));
                            let outcome = ToolOutcome {
                                content,
                                is_error,
                                metadata,
                            };
                            let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                call_id: call_id.clone(),
                                result: outcome.clone(),
                                elapsed_ms,
                            });
                            tool_results.push((call_id, outcome, elapsed_ms));
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
                    UiCommand::Steer { .. } | UiCommand::Unsteer { .. } => {
                        speculative.push(cmd)
                    }
                    UiCommand::AppendHistoryItem { append } => {
                        deferred.push(UiCommand::AppendHistoryItem { append });
                    }
                    UiCommand::SetReasoningEffort { .. }
                    | UiCommand::SetFastMode { .. }
                    | UiCommand::SetMode { .. }
                    | UiCommand::SetModel { .. } => deferred.push(cmd),
                    other => { let _ = dispatch_background_cmd(other, &bg_ctx); }
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
            let cancelled_outcome = || ToolOutcome {
                content: "cancelled".to_string(),
                is_error: true,
                metadata: None,
            };
            for (_, call_id, start) in plan.pending_tools.drain(..) {
                let elapsed_ms = Some(elapsed_ms_since(start));
                let outcome = cancelled_outcome();
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: call_id.clone(),
                    result: outcome.clone(),
                    elapsed_ms,
                });
                tool_results.push((call_id, outcome, elapsed_ms));
            }
            for (_, pending) in plan.pending_tool_hooks.drain(..) {
                let elapsed_ms = Some(elapsed_ms_since(pending.tool_start));
                let outcome = cancelled_outcome();
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: pending.tc.id.clone(),
                    result: outcome.clone(),
                    elapsed_ms,
                });
                tool_results.push((pending.tc.id.clone(), outcome, elapsed_ms));
            }
            for (_, pending) in plan.pending_tool_perms.drain(..) {
                let elapsed_ms = Some(elapsed_ms_since(pending.tool_start));
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: pending.tc.id.clone(),
                    result: cancelled_outcome(),
                    elapsed_ms,
                });
                tool_results.push((pending.tc.id.clone(), cancelled_outcome(), elapsed_ms));
            }
        }

        if !cancelled {
            for cmd in speculative {
                self.handle_turn_cmd(cmd);
            }
        }
        for cmd in deferred {
            self.handle_turn_cmd(cmd);
        }

        (cancelled, Vec::new(), tool_results)
    }

    /// Populate `completed[i]` with a synthetic `cancelled` outcome for any
    /// slot that never finished. Emits a `ToolFinished` event per slot. The
    /// resulting `Vec<Option<ToolResult>>` invariant - Some for every slot -
    /// is what `gather_slot_results` then folds into the assistant step.
    fn mark_unfinished_cancelled(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        completed: &mut [Option<ToolResult>],
    ) {
        for (i, slot) in plan.slots.iter().enumerate() {
            if completed[i].is_some() {
                continue;
            }
            let outcome = ToolResult {
                content: "cancelled".to_string(),
                is_error: true,
                metadata: None,
            };
            self.emit(EngineEvent::ToolFinished {
                call_id: slot.tc.id.clone(),
                result: ToolOutcome {
                    content: outcome.content.clone(),
                    is_error: outcome.is_error,
                    metadata: outcome.metadata.clone(),
                },
                elapsed_ms: Some(self.elapsed_ms_since(slot.start)),
            });
            completed[i] = Some(outcome);
        }
    }

    /// Dispatch sequential tools one at a time (used by tools that await user interaction).
    async fn run_sequential(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
    ) -> Vec<(String, ToolOutcome, Option<u64>)> {
        let mut tool_results = Vec::new();
        let mut cancelled = false;
        for (tc, args, start) in &plan.sequential_tools {
            let (content, is_error, metadata) = if cancelled || self.cancel.is_cancelled() {
                cancelled = true;
                ("cancelled".to_string(), true, None)
            } else {
                let request_id = next_request_id();
                Self::send_tool_dispatch_for_call(self.event_tx, request_id, tc, args);
                match self.wait_for_tool_result(request_id).await {
                    Some((c, e, m)) => (c, e, m),
                    None => {
                        cancelled = true;
                        ("cancelled".to_string(), true, None)
                    }
                }
            };
            let elapsed_ms = Some(self.elapsed_ms_since(*start));
            let outcome = ToolOutcome {
                content,
                is_error,
                metadata,
            };
            let _ = self.event_tx.send(EngineEvent::ToolFinished {
                call_id: tc.id.clone(),
                result: outcome.clone(),
                elapsed_ms,
            });
            tool_results.push((tc.id.clone(), outcome, elapsed_ms));
        }
        tool_results
    }

    /// Fold built-in (dispatched-by-slot) tool results into `(call_id,
    /// outcome, elapsed)` triples ready to be paired into the assistant
    /// turn. Emits `ToolFinished` and records `tool_elapsed` along the way.
    ///
    /// Pure accumulator: never touches `self.history`. The atomic commit
    /// in `run()` is the only place that writes to history.
    fn gather_slot_results(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        mut completed: Vec<Option<ToolResult>>,
    ) -> Vec<(String, ToolOutcome, Option<u64>)> {
        let mut out = Vec::with_capacity(plan.slots.len());
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
            let mut full_content = content.clone();
            if let Some(ref msg) = slot.confirm_msg {
                full_content.push_str(&format!("\n\nUser message: {msg}"));
            }
            self.emit(EngineEvent::ToolFinished {
                call_id: slot.tc.id.clone(),
                result: ToolOutcome {
                    content: content.clone(),
                    is_error,
                    metadata: metadata.clone(),
                },
                elapsed_ms: Some(elapsed_ms),
            });
            out.push((
                slot.tc.id.clone(),
                ToolOutcome {
                    content: full_content,
                    is_error,
                    metadata,
                },
                Some(elapsed_ms),
            ));
        }
        out
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
    ) -> (Result<(ChatResponse, bool), ProviderError>, String, String) {
        // The chat future borrows self.provider and self.model, so model
        // changes received mid-request are deferred until the future resolves.
        let mut pending_model: Option<(String, String, String, String)> = None;
        // Speculative steer/unsteer commands received while the request is
        // in flight. Applied only if the request succeeds so the app can
        // preserve its queue on failure.
        let mut deferred_turn_cmds: Vec<UiCommand> = Vec::new();
        // History appends (e.g. process status) are factual and are applied
        // regardless of whether the request succeeds or fails.
        let mut deferred_appends: Vec<protocol::HistoryAppend> = Vec::new();

        let partial_text = std::sync::Mutex::new(String::new());
        let partial_reasoning = std::sync::Mutex::new(String::new());
        let reasoning_stream = std::sync::Mutex::new(ReasoningStreamState::default());

        let reasoning_changes_apply_now = self.provider.supports_mid_turn_reasoning_changes();

        // Model changes received mid-request are applied after the future resolves.
        let result = {
            let on_retry = |delay: std::time::Duration, attempt: u32| {
                let _ = self.event_tx.send(EngineEvent::Retrying {
                    delay_ms: delay.as_millis() as u64,
                    attempt,
                });
            };
            let on_delta = |delta: ProviderStreamEvent| match delta {
                ProviderStreamEvent::TextDelta(text) => {
                    partial_text.lock().unwrap().push_str(text);
                    let _ = self.event_tx.send(EngineEvent::TextDelta {
                        delta: text.to_string(),
                    });
                }
                ProviderStreamEvent::Reasoning(event) => {
                    reasoning_stream.lock().unwrap().apply(
                        event,
                        self.event_tx,
                        &partial_reasoning,
                    );
                }
                ProviderStreamEvent::ToolCall(tool_event) => match tool_event {
                    ToolCallStreamEvent::Started {
                        stream_id,
                        call_id,
                        tool_name,
                    } => {
                        let _ = self.event_tx.send(EngineEvent::ToolCallDraftStarted {
                            stream_id: stream_id.to_string(),
                            call_id: call_id.map(str::to_string),
                            tool_name: tool_name.map(str::to_string),
                        });
                    }
                    ToolCallStreamEvent::ArgsDelta {
                        stream_id,
                        call_id,
                        tool_name,
                        delta,
                    } => {
                        let _ = self.event_tx.send(EngineEvent::ToolCallDraftDelta {
                            stream_id: stream_id.to_string(),
                            call_id: call_id.map(str::to_string),
                            tool_name: tool_name.map(str::to_string),
                            delta: delta.to_string(),
                        });
                    }
                    ToolCallStreamEvent::Finished {
                        stream_id,
                        call_id,
                        tool_name,
                        arguments,
                    } => {
                        let _ = self.event_tx.send(EngineEvent::ToolCallDraftFinished {
                            stream_id: stream_id.to_string(),
                            call_id: call_id.to_string(),
                            tool_name: tool_name.to_string(),
                            arguments: arguments.to_string(),
                        });
                    }
                },
            };
            // Resolve pricing once so the request-log sidecar can record an
            // estimated cost alongside usage on success.
            let pricing = smelt_provider::resolve_pricing(
                &self.model,
                self.provider.provider_kind().as_config_str(),
                self.provider.api_base(),
                self.provider.model_config(),
            );
            let session_dir = self.session_dir.clone();
            let audit_host_tx = self.host_tx.clone();
            let turn_id = self.turn_id;
            let history_len = self.history.len();
            let audit_mode = self.config.request_audit;
            // Convert the engine's `Vec<HistoryItem>` to the wire-format
            // `Vec<Message>` the provider speaks. Pairing is invariant-safe
            // by construction (every `AssistantStep` carries its
            // `ToolInvocation`s inline), so the resulting Message slice
            // satisfies "assistant tool_calls followed by tool_results".
            let wire_messages = protocol::history_to_messages(&self.history);
            let on_attempt = move |info: RequestAttemptInfo<'_>| {
                let ctx = crate::request_log::RequestContext {
                    request_id: turn_id,
                    kind: "turn".to_string(),
                    turn_id: Some(turn_id),
                    ask_id: None,
                    history_len: Some(history_len),
                    background: false,
                };
                dispatch_request_audit(
                    &audit_host_tx,
                    &session_dir,
                    ctx,
                    &info,
                    &pricing,
                    audit_mode,
                );
            };
            let request_opts = ChatRequestOptions {
                cache: self
                    .provider
                    .default_cache_config(self.config.cache_ttl_long, Some(&self.session_id)),
                fast_mode: self.fast_mode,
                ..ChatRequestOptions::default()
            };
            let opts = ChatOptions {
                cancel: &self.cancel,
                on_retry: Some(&on_retry),
                on_delta: Some(&on_delta),
                on_attempt: Some(&on_attempt),
            };
            let chat_future = self.provider.chat(
                &wire_messages,
                tool_defs,
                &self.model,
                self.reasoning_effort,
                &request_opts,
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
                            self.bg_cancel.cancel();
                        }
                        UiCommand::SetReasoningEffort { effort } => {
                            if reasoning_changes_apply_now {
                                self.reasoning_effort = effort;
                            }
                        }
                        UiCommand::SetMode { mode } => self.mode = mode,
                        UiCommand::SetFastMode { enabled } => self.fast_mode = enabled,
                        UiCommand::SetModel { model, api_base, api_key, provider_type } => {
                            pending_model = Some((model, api_base, api_key, provider_type));
                        }
                        UiCommand::AppendHistoryItem { append } => {
                            deferred_appends.push(append);
                        }
                        UiCommand::Steer { .. } | UiCommand::Unsteer { .. } => {
                            deferred_turn_cmds.push(cmd)
                        }
                        other => {
                            let _ = dispatch_background_cmd(other, &self.bg_ctx());
                        }
                    },
                }
            }
        };

        reasoning_stream
            .into_inner()
            .unwrap_or_default()
            .finish_all(self.event_tx);
        let pt = partial_text.into_inner().unwrap_or_default();
        let pr = partial_reasoning.into_inner().unwrap_or_default();

        if let Some((model, api_base, api_key, provider_type)) = pending_model {
            self.apply_model_change(model, api_base, api_key, provider_type);
        }
        for append in deferred_appends {
            self.queue_history_item(append);
        }
        let had_injected = deferred_turn_cmds
            .iter()
            .any(|c| matches!(c, UiCommand::Steer { .. }));
        if result.is_ok() {
            for cmd in deferred_turn_cmds {
                self.handle_turn_cmd(cmd);
            }
        }
        (result.map(|r| (r, had_injected)), pt, pr)
    }

    async fn wait_for_tool_result(
        &mut self,
        request_id: u64,
    ) -> Option<(String, bool, Option<serde_json::Value>)> {
        loop {
            match self.cmd_rx.recv().await {
                Some(UiCommand::ToolResult {
                    request_id: id,
                    call_id: _,
                    content,
                    is_error,
                    metadata,
                }) if id == request_id => return Some((content, is_error, metadata)),
                Some(UiCommand::AppendHistoryItem { append }) => {
                    self.queue_history_item(append);
                }
                Some(UiCommand::SetReasoningEffort { effort }) => {
                    self.apply_reasoning_effort(effort)
                }
                Some(UiCommand::SetMode { mode }) => self.mode = mode,
                Some(UiCommand::SetFastMode { enabled }) => self.fast_mode = enabled,
                Some(UiCommand::SetModel {
                    model,
                    api_base,
                    api_key,
                    provider_type,
                }) => self.apply_model_change(model, api_base, api_key, provider_type),
                Some(UiCommand::Cancel) => {
                    self.cancel.cancel();
                    self.bg_cancel.cancel();
                    return None;
                }
                None => return None,
                Some(other) => {
                    let _ = dispatch_background_cmd(other, &self.bg_ctx());
                }
            }
        }
    }
}

#[derive(Default)]
struct ReasoningStreamState {
    parts: HashMap<String, ActiveReasoningPart>,
}

struct ActiveReasoningPart {
    kind: ReasoningKind,
    source: String,
}

impl ReasoningStreamState {
    fn apply(
        &mut self,
        event: ReasoningStreamEvent<'_>,
        tx: &mpsc::UnboundedSender<EngineEvent>,
        partial_reasoning: &std::sync::Mutex<String>,
    ) {
        match event {
            ReasoningStreamEvent::PartStarted {
                item_id,
                part_index,
                kind,
            } => {
                let id = reasoning_part_id(item_id, part_index, kind);
                self.parts
                    .entry(id.clone())
                    .or_insert_with(|| start_reasoning_part(tx, id, kind));
            }
            ReasoningStreamEvent::Delta {
                item_id,
                part_index,
                kind,
                delta,
            } => {
                let id = reasoning_part_id(item_id, part_index, kind);
                let part = self
                    .parts
                    .entry(id.clone())
                    .or_insert_with(|| start_reasoning_part(tx, id.clone(), kind));
                part.source.push_str(delta);
                partial_reasoning.lock().unwrap().push_str(delta);
                let title = (kind == ReasoningKind::Summary)
                    .then(|| streaming_summary_title(&part.source))
                    .flatten();
                let _ = tx.send(EngineEvent::ReasoningPartDelta {
                    id,
                    kind,
                    delta: delta.to_string(),
                    title,
                });
            }
            ReasoningStreamEvent::PartFinished {
                item_id,
                part_index,
                kind,
                content,
            } => {
                let id = reasoning_part_id(item_id, part_index, kind);
                let mut part = self
                    .parts
                    .remove(&id)
                    .unwrap_or_else(|| start_reasoning_part(tx, id.clone(), kind));
                if let Some(content) = content {
                    if part.source.is_empty() && !content.is_empty() {
                        partial_reasoning.lock().unwrap().push_str(content);
                        let title = (kind == ReasoningKind::Summary)
                            .then(|| streaming_summary_title(content))
                            .flatten();
                        let _ = tx.send(EngineEvent::ReasoningPartDelta {
                            id: id.clone(),
                            kind,
                            delta: content.to_string(),
                            title,
                        });
                    }
                    part.source.clear();
                    part.source.push_str(content);
                }
                send_reasoning_part_finished(tx, id, part);
            }
        }
    }

    fn finish_all(self, tx: &mpsc::UnboundedSender<EngineEvent>) {
        let mut parts: Vec<_> = self.parts.into_iter().collect();
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, part) in parts {
            send_reasoning_part_finished(tx, id, part);
        }
    }
}

fn start_reasoning_part(
    tx: &mpsc::UnboundedSender<EngineEvent>,
    id: String,
    kind: ReasoningKind,
) -> ActiveReasoningPart {
    let _ = tx.send(EngineEvent::ReasoningPartStarted { id, kind });
    ActiveReasoningPart {
        kind,
        source: String::new(),
    }
}

fn reasoning_part_id(item_id: &str, part_index: u32, kind: ReasoningKind) -> String {
    let item_id = if item_id.is_empty() {
        "reasoning"
    } else {
        item_id
    };
    let kind = match kind {
        ReasoningKind::Summary => "summary",
        ReasoningKind::Raw => "raw",
    };
    format!("{item_id}:{kind}:{part_index}")
}

fn leading_bold_title(source: &str) -> Option<(&str, &str)> {
    let after_open = source.trim_start().strip_prefix("**")?;
    let close = after_open.find("**")?;
    let title = after_open[..close].trim();
    (!title.is_empty()).then_some((title, &after_open[close + 2..]))
}

fn streaming_summary_title(source: &str) -> Option<String> {
    leading_bold_title(source).map(|(title, _)| title.to_string())
}

fn normalize_reasoning_part(kind: ReasoningKind, source: &str) -> (Option<String>, String) {
    let source = source.trim();
    if kind == ReasoningKind::Raw {
        return (None, source.to_string());
    }

    let Some((title, after_title)) = leading_bold_title(source) else {
        return (None, source.to_string());
    };
    if after_title.trim().is_empty() || !after_title.starts_with(char::is_whitespace) {
        return (None, source.to_string());
    }
    let content = after_title
        .lines()
        .filter(|line| line.trim() != "<!-- -->")
        .collect::<Vec<_>>()
        .join("\n");
    (Some(title.to_string()), content.trim().to_string())
}

fn send_reasoning_part_finished(
    tx: &mpsc::UnboundedSender<EngineEvent>,
    id: String,
    part: ActiveReasoningPart,
) {
    let (title, content) = normalize_reasoning_part(part.kind, &part.source);
    let _ = tx.send(EngineEvent::ReasoningPartFinished {
        id,
        kind: part.kind,
        title,
        content,
    });
}

// ── Helpers ─────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn send_usage(
    tx: &mpsc::UnboundedSender<EngineEvent>,
    provider_type: &str,
    api_base: &str,
    model_config: &ModelConfig,
    model: &str,
    usage: protocol::TokenUsage,
    tokens_per_sec: Option<f64>,
    background: bool,
) {
    let resolved = smelt_provider::resolve_pricing(model, provider_type, api_base, model_config);
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
    api_base: String,
    model_config: ModelConfig,
}

impl PricingContext {
    fn from_api(api: &crate::ApiConfig) -> Self {
        Self {
            provider_type: api.provider_type.clone(),
            api_base: api.base.clone(),
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
            &self.api_base,
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

    #[test]
    fn completed_only_reasoning_part_emits_its_content_as_a_delta() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let partial_reasoning = std::sync::Mutex::new(String::new());
        let mut state = ReasoningStreamState::default();

        state.apply(
            ReasoningStreamEvent::PartFinished {
                item_id: "reasoning",
                part_index: 0,
                kind: ReasoningKind::Raw,
                content: Some("complete thought"),
            },
            &tx,
            &partial_reasoning,
        );

        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartStarted { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartDelta { delta, .. }) if delta == "complete thought"
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartFinished { content, .. }) if content == "complete thought"
        ));
        assert_eq!(partial_reasoning.into_inner().unwrap(), "complete thought");
    }

    #[test]
    fn summary_placeholder_normalizes_to_thinking_title_only() {
        assert_eq!(
            normalize_reasoning_part(
                ReasoningKind::Summary,
                "**Removing unused BlockIndex**\n\n<!-- -->",
            ),
            (Some("Removing unused BlockIndex".into()), String::new())
        );
    }

    #[test]
    fn summary_body_is_kept_separate_from_its_title() {
        assert_eq!(
            normalize_reasoning_part(
                ReasoningKind::Summary,
                "**Checking tests**\n\n<!-- -->\nAll tests passed.",
            ),
            (Some("Checking tests".into()), "All tests passed.".into(),)
        );
    }

    #[test]
    fn standalone_bold_summary_content_is_not_discarded() {
        assert_eq!(
            normalize_reasoning_part(ReasoningKind::Summary, "**Important conclusion**"),
            (None, "**Important conclusion**".into())
        );
    }

    #[test]
    fn load_model_history_reads_requested_store_range() {
        let dir = tempfile::tempdir().unwrap();
        let db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        let old = HistoryItem::user(protocol::Content::text("old"));
        let recent = HistoryItem::user(protocol::Content::text("recent"));
        let reply = HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("reply")),
            None,
            Vec::new(),
        ));
        let snapshot = smelt_store::SessionSnapshot {
            state: smelt_store::SessionState {
                id: "s1".into(),
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                fast_mode: None,
                parent_id: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: 0,
                history_len: 3,
                created_at: 10,
                updated_at: 20,
            },
            history_start_idx: 0,
            history_len: 3,
            history: vec![old, recent.clone(), reply.clone()],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };
        db.save_session_snapshot_for_import(&snapshot).unwrap();

        let history = load_model_history(
            protocol::ModelHistorySource::store(
                vec![HistoryItem::user(protocol::Content::text(
                    "SUMMARY:\ncompact",
                ))],
                1,
                3,
            ),
            dir.path(),
        )
        .unwrap();

        assert_eq!(history.items.len(), 3);
        assert!(
            matches!(&history.items[0], HistoryItem::User { content, .. } if content.text_content() == "SUMMARY:\ncompact")
        );
        assert_eq!(history.items[1], recent);
        assert_eq!(history.items[2], reply);
        assert_eq!(
            history.coordinates,
            protocol::ModelHistoryCoordinates::projected(1, 1)
        );
    }

    #[test]
    fn model_history_read_completes_after_concurrent_writer_commits() {
        let dir = tempfile::tempdir().unwrap();
        let db = smelt_store::SessionDb::open(dir.path().join("session.db")).unwrap();
        let item = HistoryItem::user(protocol::Content::text("persisted"));
        let snapshot = smelt_store::SessionSnapshot {
            state: smelt_store::SessionState {
                id: "blocked-history".into(),
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                parent_id: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: 0,
                history_len: 1,
                created_at: 10,
                updated_at: 20,
            },
            history_start_idx: 0,
            history_len: 1,
            history: vec![item.clone()],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };
        db.save_session_snapshot_for_import(&snapshot).unwrap();
        db.connection()
            .execute_batch(
                "BEGIN IMMEDIATE;
                 UPDATE store_meta SET updated_at = updated_at WHERE key = 'schema_version';",
            )
            .unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            db.connection().execute_batch("COMMIT").unwrap();
        });

        let history = load_model_history(
            protocol::ModelHistorySource::store(Vec::new(), 0, 1),
            dir.path(),
        )
        .unwrap();
        writer.join().unwrap();

        assert_eq!(history.items, vec![item]);
    }

    #[test]
    fn classify_provider_error_detects_kimi_model_token_limit() {
        let err = ProviderError::InvalidResponse(
            r#"{"error":{"type":"invalid_request_error","message":"Invalid request: Your request exceeded model token limit: 262144 (requested: 264866)"},"type":"error"}"#.into(),
        );

        assert_eq!(
            classify_provider_error(&err),
            EngineAskErrorKind::ContextWindow
        );
    }

    // ---- pair_invocations_in_order ----

    fn tc(id: &str) -> protocol::ToolCall {
        protocol::ToolCall::new(
            id.into(),
            protocol::FunctionCall {
                name: "f".into(),
                arguments: "{}".into(),
            },
        )
    }

    fn outcome(content: &str) -> ToolOutcome {
        ToolOutcome {
            content: content.into(),
            is_error: false,
            metadata: None,
        }
    }

    #[test]
    fn pair_invocations_in_order_precedence_is_slot_then_plugin_then_inline() {
        // Same call_id appears in all three inputs. The classifier guarantees
        // this can't happen in practice - this test pins the tiebreaker so a
        // future refactor that *does* introduce a collision behaves
        // predictably instead of silently flipping which outcome wins.
        let calls = vec![tc("c1"), tc("c2"), tc("c3")];
        let slot = vec![("c1".into(), outcome("slot-c1"), Some(11))];
        let plugin = vec![
            ("c1".into(), outcome("plugin-c1"), Some(99)),
            ("c2".into(), outcome("plugin-c2"), Some(22)),
        ];
        let inline = vec![
            ("c1".into(), outcome("inline-c1")),
            ("c2".into(), outcome("inline-c2")),
            ("c3".into(), outcome("inline-c3")),
        ];
        let out = pair_invocations_in_order(&calls, slot, plugin, inline);
        assert_eq!(out[0].result.content, "slot-c1");
        assert_eq!(out[0].elapsed_ms, Some(11));
        assert_eq!(out[1].result.content, "plugin-c2");
        assert_eq!(out[1].elapsed_ms, Some(22));
        assert_eq!(out[2].result.content, "inline-c3");
        assert_eq!(out[2].elapsed_ms, None);
    }

    fn api_cfg() -> ApiConfig {
        ApiConfig {
            base: "https://x/".into(),
            key: "k".into(),
            key_env: "K".into(),
            provider_type: "openai".into(),
            model_config: ModelConfig::default(),
        }
    }

    fn test_engine_config(clock: std::sync::Arc<dyn crate::clock::Clock>) -> EngineConfig {
        EngineConfig {
            system_prompt_override: Some("sys".into()),
            ..EngineConfig::new(api_cfg(), "m", std::path::PathBuf::from("/tmp"), clock)
        }
    }

    #[test]
    fn current_turn_content_classifies_internal_notes() {
        let client = reqwest::Client::new();
        let dispatcher = crate::tools::EmptyDispatcher::new();
        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (host_tx, _host_rx) = mpsc::unbounded_channel();
        let clock: std::sync::Arc<dyn crate::clock::Clock> =
            std::sync::Arc::new(crate::clock::RealClock);
        let config = test_engine_config(clock.clone());
        let provider = EngineProvider::new(
            "https://x".into(),
            "k".into(),
            "openai",
            client.clone(),
            clock,
        );
        let mut turn = Turn {
            provider,
            dispatcher: &dispatcher,
            cmd_rx: &mut cmd_rx,
            event_tx: &event_tx,
            host_tx: &host_tx,
            config: &config,
            http_client: &client,
            cancel: CancellationToken::new(),
            bg_cancel: CancellationToken::new(),
            history: vec![HistoryItem::system("sys")],
            history_coordinates: protocol::ModelHistoryCoordinates::canonical(),
            mode: AgentMode::normal(),
            reasoning_effort: ReasoningEffort::Off,
            fast_mode: false,
            turn_id: 1,
            model: "m".into(),
            display: None,
            system_prompt: "sys".into(),
            tools: Vec::new(),
            permission_overrides: None,
            pending_history_items: Vec::new(),
            next_history_changed_from: 0,
            session_id: "s".into(),
            session_dir: std::path::PathBuf::from("/tmp/s"),
            started_at: Instant::now(),
            tps_samples: Vec::new(),
            tool_elapsed: HashMap::new(),
        };

        turn.push_turn_content(
            Content::text(protocol::process_status_note(
                "Background process 751225 exited with code 1.",
            )),
            None,
        );
        turn.push_turn_content(
            Content::text(protocol::mode_change_note("now in apply mode.")),
            None,
        );

        assert!(matches!(
            &turn.history[1],
            HistoryItem::Note(protocol::HistoryNote::ProcessStatus { text, .. })
                if text == "Background process 751225 exited with code 1."
        ));
        assert!(matches!(
            &turn.history[2],
            HistoryItem::Note(protocol::HistoryNote::ModeChange { text, .. })
                if text == "now in apply mode."
        ));
        let typed_note = protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed("751225", Some(1)),
        );
        turn.push_current_turn_input(protocol::StartTurnInput::note(typed_note.clone()));

        assert!(matches!(
            &turn.history[3],
            HistoryItem::Note(note) if note == &typed_note
        ));
    }

    #[tokio::test]
    async fn checkpointed_start_turn_emits_canonical_typed_note_update() {
        let note = protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed("751225", Some(1)),
        );
        let config = test_engine_config(std::sync::Arc::new(crate::clock::RealClock));
        let mut handle = crate::start(config, Box::new(crate::tools::EmptyDispatcher::new()));
        drop(handle.take_host_rx());

        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(1), handle.recv())
                .await
                .expect("engine ready")
            {
                Some(EngineEvent::Ready) => break,
                Some(_) => continue,
                None => panic!("engine closed before Ready"),
            }
        }

        let prior = HistoryItem::User {
            content: Content::text("prior"),
            display: None,
        };
        handle.send(UiCommand::StartTurn(Box::new(protocol::StartTurnPayload {
            turn_id: 1,
            input: protocol::StartTurnInput::note(note.clone()),
            mode: AgentMode::normal(),
            model: "m".into(),
            reasoning_effort: ReasoningEffort::Off,
            fast_mode: false,
            history: protocol::ModelHistorySource::projected_items(
                vec![
                    HistoryItem::user(Content::text("SUMMARY:\ncheckpoint")),
                    prior.clone(),
                ],
                protocol::ModelHistoryCoordinates::projected(1, 24),
            ),
            api_base: None,
            api_key: None,
            session_id: "s".into(),
            session_dir: std::path::PathBuf::from("/tmp"),
            model_config_overrides: None,
            permission_overrides: None,
            system_prompt: Some("sys".into()),
            tools: Vec::new(),
        })));

        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(1), handle.recv())
                .await
                .expect("history snapshot")
            {
                Some(EngineEvent::HistoryUpdated { update, .. }) => {
                    assert_eq!(update.items, vec![HistoryItem::note(note)]);
                    assert_eq!(update.first_index.get(), 25);
                    break;
                }
                Some(_) => continue,
                None => panic!("engine closed before HistoryUpdated"),
            }
        }
        handle.send(UiCommand::Cancel);
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
        let cfg = p.model_config();
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
            context_tokens: Some(1_500_000),
            prompt_tokens: Some(1_000_000),
            completion_tokens: Some(500_000),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        };
        send_usage(&tx, "openai", "", &cfg, "model-x", usage, Some(50.0), false);
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
        send_usage(
            &tx,
            "openai-compatible",
            "",
            &cfg,
            "model",
            usage,
            None,
            false,
        );
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
        assert_eq!(pc.api_base, "https://x/");
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
