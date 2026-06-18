//! `smelt.engine` - cancel, ask, inherited ask, and submit_command for Lua-rendered turns.

use crate::app::QueuedInput;
use crate::lua::{LuaHandle, LuaShared};
use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use smelt_core::lua::api::reasoning::LuaReasoningEffort;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::lua_type::LuaCallback;
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::AskCallbacks;
use std::sync::Arc;

/// One text-only message used by request hooks that exchange plain
/// user/assistant rows.
#[allow(dead_code)]
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskMessage")]
pub struct LuaAskMessage {
    /// Either `"user"` or `"assistant"`. Other roles are silently dropped.
    pub role: String,
    /// Message body as plain text.
    pub content: String,
}

/// One assistant tool call returned by `smelt.engine.ask` /
/// `smelt.engine.ask_inherited`. Matches the provider wire shape.
#[allow(dead_code)]
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskToolCall")]
pub struct LuaAskToolCall {
    /// Stable provider-generated call id.
    pub id: String,
    /// Always `"function"`.
    #[lua(rename = "type")]
    pub call_type: String,
    /// Tool name and arguments.
    pub function: LuaAskFunctionCall,
}

/// Function payload inside an `AskToolCall`.
#[allow(dead_code)]
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskFunctionCall")]
pub struct LuaAskFunctionCall {
    /// Tool/function name.
    pub name: String,
    /// JSON arguments string.
    pub arguments: String,
}

/// Structured assistant reply delivered to `smelt.engine.ask` /
/// `smelt.engine.ask_inherited` callbacks on success. The live table may
/// also carry provider-specific `reasoning_details` blocks when the model
/// returned them.
#[allow(dead_code)]
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskResponse")]
pub struct LuaAskResponseTable {
    /// Always `"assistant"` for successful ask replies.
    pub role: String,
    /// Assistant text content when present.
    pub content: Option<String>,
    /// Flattened reasoning text when present.
    pub reasoning_content: Option<String>,
    /// Structured tool calls emitted by the model, if any.
    pub tool_calls: Option<Vec<LuaAskToolCall>>,
}

/// Subcommand rule override accepted inside `CommandOverrides`. Mirrors
/// the front-matter `{ allow?, ask?, deny? }` shape.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.engine.RuleOverride")]
pub struct LuaRuleOverride {
    /// Patterns that auto-allow.
    #[lua(default)]
    pub allow: Vec<String>,
    /// Patterns that always prompt.
    #[lua(default)]
    pub ask: Vec<String>,
    /// Patterns that auto-deny.
    #[lua(default)]
    pub deny: Vec<String>,
}

impl From<LuaRuleOverride> for smelt_core::custom_commands::RuleOverride {
    fn from(r: LuaRuleOverride) -> Self {
        Self {
            allow: r.allow,
            ask: r.ask,
            deny: r.deny,
        }
    }
}

/// Front-matter override block accepted by
/// `smelt.engine.submit_command`. Mirrors what plugin commands set in
/// their markdown header. Tool-name keys (e.g. `bash`, `edit`) become
/// per-subcommand pattern buckets.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.engine.CommandOverrides")]
pub struct LuaCommandOverrides {
    /// Override the command description shown in `/help`.
    pub description: Option<String>,
    /// Force a specific provider for this command's turn.
    pub provider: Option<String>,
    /// Force a specific model id.
    pub model: Option<String>,
    /// Sampling temperature override.
    pub temperature: Option<f64>,
    /// Nucleus-sampling cutoff override.
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff override.
    pub top_k: Option<u32>,
    /// Minimum-probability cutoff override.
    pub min_p: Option<f64>,
    /// Repeat-penalty override.
    pub repeat_penalty: Option<f64>,
    /// Reasoning-effort override; one of the `smelt.reasoning.Effort` strings.
    pub reasoning_effort: Option<String>,
    /// Per-tool `allow`/`ask`/`deny` patterns for the duration of the turn.
    pub tools: Option<LuaRuleOverride>,
    /// Per-subcommand pattern buckets keyed by tool name.
    #[lua(rest)]
    pub subcommands: std::collections::HashMap<String, LuaRuleOverride>,
}

impl From<LuaCommandOverrides> for smelt_core::custom_commands::CommandOverrides {
    fn from(o: LuaCommandOverrides) -> Self {
        Self {
            provider: o.provider,
            model: o.model,
            temperature: o.temperature,
            top_p: o.top_p,
            top_k: o.top_k,
            min_p: o.min_p,
            repeat_penalty: o.repeat_penalty,
            reasoning_effort: o.reasoning_effort,
            tools: o.tools.map(Into::into),
            subcommands: o
                .subcommands
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

/// Structured JSON-output specification for `smelt.engine.ask`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskResponseFormat")]
pub struct LuaAskResponseFormat {
    /// Schema name (used by some providers as the response_format label).
    pub name: String,
    /// JSON schema describing the expected response shape. Accepts a Lua
    /// table that round-trips through `lua_table_to_json` into a JSON value.
    pub schema: mlua::Table,
}

/// Typed error table delivered to `on_response` when the underlying
/// provider call fails. `kind` is a stable string the caller can branch
/// on; `message` is a human-readable single-line description. The
/// struct exists purely as a doc / LuaCATS schema target - the actual
/// table is built in `LuaRuntime::fire_ask_callback` because it lands
/// on a callback path that bypasses `FromLua` decoding.
#[allow(dead_code)]
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskError")]
pub struct LuaAskErrorTable {
    /// One of `"network" | "rate_limited" | "quota" | "invalid_response" | "context_window" | "cancelled" | "other"`.
    pub kind: String,
    /// Human-readable single-line description (newlines collapsed to spaces).
    pub message: String,
}

/// Request object passed to `smelt.engine.on_prepare_request`.
#[allow(dead_code)]
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.PrepareRequest")]
pub struct LuaPrepareRequest {
    /// Model-visible conversation excluding the system prompt.
    pub messages: Vec<LuaAskMessage>,
    /// Conservative token estimate for the request about to be sent,
    /// including system prompt, messages, and tool definitions.
    pub estimated_tokens: u32,
    /// Active-context estimate for auto-compaction. When a provider has
    /// reported context usage, this starts from that server-observed count
    /// and adds only local messages appended after the matching token
    /// snapshot; before the first usage report it equals `estimated_tokens`.
    pub estimated_context_tokens: u32,
    /// Structured breakdown explaining how `estimated_context_tokens` was
    /// computed.
    pub context_estimate: LuaPrepareContextEstimate,
}

/// Token accounting breakdown passed inside `smelt.engine.PrepareRequest`.
#[allow(dead_code)]
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.PrepareContextEstimate")]
pub struct LuaPrepareContextEstimate {
    /// One of `"full_request_estimate" | "provider_snapshot" |
    /// "provider_snapshot_plus_history_delta"`.
    pub source: String,
    /// Total active-context estimate used by auto-compaction.
    pub total_context_tokens: u32,
    /// Latest provider-reported active context, when available.
    #[lua(default)]
    pub provider_context_tokens: Option<u32>,
    /// Locally estimated tokens added on top of provider usage.
    pub estimated_delta_tokens: u32,
    /// History length attached to the latest token snapshot, when available.
    #[lua(default)]
    pub latest_snapshot_history_len: Option<usize>,
    /// Current session history length at the prepare hook.
    pub current_history_len: usize,
}

/// Spec for `smelt.engine.ask`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.AskSpec")]
pub struct LuaAskSpec {
    /// System prompt sent before the conversation.
    pub system: String,
    /// Prior turns. When present, this must be a sequence of full
    /// `protocol::Message`-shaped rows such as `{ role, content?,
    /// reasoning_content?, tool_calls?, tool_call_id?, is_error? }`.
    #[lua(default)]
    pub messages: Option<mlua::Table>,
    /// Single-shot question appended as a final user message after `messages`.
    pub question: Option<String>,
    /// Model reference (`"provider/model"` or a bare name resolved against
    /// the configured providers). When `nil`, falls back to the primary model.
    pub model: Option<String>,
    /// JSON-schema response constraint.
    pub response_format: Option<LuaAskResponseFormat>,
    /// Reasoning effort for the request; defaults to `"off"`.
    pub reasoning_effort: Option<LuaReasoningEffort>,
    /// Lifecycle guard returned by `smelt.lifecycle.guard(...)`. When provided,
    /// the Lua bootstrap suppresses `on_delta` and `on_response` after the guard expires.
    pub guard: Option<mlua::Table>,
    /// Surface provider retry events on the main work indicator. Intended
    /// for foreground auxiliary work such as compaction.
    pub visible_retries: Option<bool>,
    /// Fires for each streamed assistant text delta when provided. The final
    /// `on_response` still fires once with the full assistant message.
    pub on_delta: Option<LuaCallback<(String,), ()>>,
    /// Fires once with `(response, err)`. On success `err` is `nil` and
    /// `response` is a full assistant message table;
    /// on failure `response` is `nil` and `err` is a
    /// `smelt.engine.AskError` table.
    pub on_response: Option<LuaCallback<(mlua::Value, Option<LuaAskErrorTable>), ()>>,
}

/// Spec for `smelt.engine.ask_inherited`.
#[derive(Debug, LuaOpts)]
#[lua(name = "smelt.engine.InheritedAskSpec")]
pub struct LuaInheritedAskSpec {
    /// Prior turns. When present, this must be a sequence of full
    /// `protocol::Message`-shaped rows such as `{ role, content?,
    /// reasoning_content?, tool_calls?, tool_call_id?, is_error? }`.
    /// When omitted or empty, the live model-visible history is inherited.
    #[lua(default)]
    pub messages: Option<mlua::Table>,
    /// Single-shot question appended as a final user message after `messages`.
    pub question: Option<String>,
    /// Model reference (`"provider/model"` or a bare name resolved against
    /// the configured providers). When `nil`, falls back to the primary model.
    pub model: Option<String>,
    /// JSON-schema response constraint.
    pub response_format: Option<LuaAskResponseFormat>,
    /// Reasoning effort for the request; defaults to `"off"`.
    pub reasoning_effort: Option<LuaReasoningEffort>,
    /// Lifecycle guard returned by `smelt.lifecycle.guard(...)`. When provided,
    /// the Lua bootstrap suppresses `on_delta` and `on_response` after the guard expires.
    pub guard: Option<mlua::Table>,
    /// Surface provider retry events on the main work indicator. Intended
    /// for foreground auxiliary work such as compaction.
    pub visible_retries: Option<bool>,
    /// Fires for each streamed assistant text delta when provided. The final
    /// `on_response` still fires once with the full assistant message.
    pub on_delta: Option<LuaCallback<(String,), ()>>,
    /// Fires once with `(response, err)`. On success `err` is `nil` and
    /// `response` is a full assistant message table;
    /// on failure `response` is `nil` and `err` is a
    /// `smelt.engine.AskError` table.
    pub on_response: Option<LuaCallback<(mlua::Value, Option<LuaAskErrorTable>), ()>>,
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "engine",
        "LLM engine control - cancel, ask, inherited ask, submit commands, and request tool approval. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "cancel",
        "Cancel the in-flight turn or foreground/background work. If queued prompt messages are waiting during a turn, restores them to the prompt instead of cancelling. In-flight `smelt.engine.ask` requests are unaffected and may still fire callbacks unless their lifecycle guard expires.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if app.queued_inputs.is_empty() {
                    app.discard_turn(crate::app::TurnEnd::Cancelled);
                } else {
                    app.drain_queued_inputs_into_prompt();
                }
            });
            Ok(())
        },
    )?;
    m.fn_(
        "is_running",
        "Return `true` if an agent turn is currently in flight (a request is being streamed or a tool is executing).",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.agent_is_running()).unwrap_or(false)),
    )?;
    m.fn_(
        "summary_prefix",
        "Return the canonical compaction-summary prefix used when a checkpoint summary is represented as a user message.",
        &[],
        |_, ()| Ok(engine::SUMMARY_PREFIX.trim_end().to_string()),
    )?;
    {
        let s = shared.clone();
        type ReplyCb = LuaCallback<(mlua::Value,), ()>;
        type HookCb = LuaCallback<(Vec<LuaAskMessage>, ReplyCb), ()>;
        m.fn_(
            "on_context_limit",
            "Register a recovery hook the engine calls when a provider returns a context-window error mid-turn. `hook` receives the conversation so far (excluding the system prompt) and a `reply` callback the hook MUST call exactly once - with `{ action = \"replace\", messages = messages }` (engine swaps it in and retries the turn), `{ action = \"abort\", message = message }` (engine aborts with that terminal error), or `nil` / `{ action = \"continue\" }` (engine continues without recovery). The first registered hook to call `reply` wins; later hooks are ignored. Returns a `Reg` whose `:remove()` drops the hook. Bundled `compact.lua` registers a hook that runs the standard summarization flow.",
            &["hook"],
            move |lua, hook: HookCb| -> LuaResult<smelt_core::lua::reg::LuaReg> {
                let id = s.hooks.context_limit.register(lua, hook.into_inner(), "")?;
                Ok(s.hooks.context_limit.reg_for(id))
            },
        )?;
    }
    {
        let s = shared.clone();
        type ReplyCb = LuaCallback<(mlua::Value,), ()>;
        type HookCb = LuaCallback<(LuaPrepareRequest, ReplyCb), ()>;
        m.fn_(
            "on_prepare_request",
            "Register a hook the engine calls immediately before each provider request. `hook` receives `{ messages, estimated_tokens, estimated_context_tokens }` and a `reply` callback the hook MUST call exactly once - with `{ action = \"replace\", messages = messages }` (engine swaps it in before sampling), `{ action = \"replace\", source = \"model_history\" }` (engine uses the current checkpointed session model history), `{ action = \"abort\", message = message }` (engine aborts with that terminal error), or `nil` / `{ action = \"continue\" }` (engine sends the original request). `messages` is built lazily when the hook reads it. Returns a `Reg` whose `:remove()` drops the hook.",
            &["hook"],
            move |lua, hook: HookCb| -> LuaResult<smelt_core::lua::reg::LuaReg> {
                let id = s.hooks.prepare_request.register(lua, hook.into_inner(), "")?;
                Ok(s.hooks.prepare_request.reg_for(id))
            },
        )?;
    }
    m.fn_(
        "reload",
        "Re-evaluate every Lua surface: clears every command, keymap, statusline source, tool, hook, timer, and cell subscriber, wipes non-stdlib `package.loaded` entries, then re-runs the bootstrap chunks (from disk overlay if present, embedded otherwise, using the same `module_overlay_roots()` lookup as `require`), bundled autoload modules, `init.lua`, global plugins, and `.smelt/init.lua` + `.smelt/plugins/*`. Cancels any in-flight `smelt.spawn` tasks and dismisses an open modal dialog before reloading (the parked coroutine is dropped with the rest). `early.lua` is intentionally skipped - its CLI-flag and `smelt.builtins.disable` effects are startup-only.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if app.prompt_input_is_busy() {
                    app.notify_error("cannot reload while agent is working".into());
                    return;
                }
                app.reload_lua_dismissing_modal();
            });
            Ok(())
        },
    )?;

    m.fn_(
        "reload_when_idle",
        "Schedule a full config reload for the next safe idle point, including prompt inputs such as AGENTS.md, skills, and `--system-prompt`. Returns `true` when this call queued a new reload and `false` when one was already pending.",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::lua::try_with_app(|app| app.schedule_lua_reload()).unwrap_or(false))
        },
    )?;

    m.fn_(
        "submit_command",
        "Start an agent turn from a Lua-defined custom command (`/name`). `display` overrides the transcript label while `name` remains the command id. Queues behind the active turn if the agent is already running. See [`smelt.engine.CommandOverrides`](types.md#smeltenginecommandoverrides) for the override shape.",
        &["name", "body", "overrides", "display"],
        |_,
         (name, body, overrides, display): (
            String,
            String,
            Option<LuaCommandOverrides>,
            Option<String>,
        )|
         -> LuaResult<()> {
            let parsed = overrides.map(Into::into).unwrap_or_default();
            crate::lua::with_app(|app| {
                let cmd = smelt_core::custom_commands::CustomCommand {
                    display: display.unwrap_or_else(|| name.clone()),
                    name,
                    body,
                    overrides: parsed,
                };
                if app.prompt_input_is_busy() {
                    let text = if app.core.config.settings.redact_secrets {
                        engine::redact::redact(&cmd.body)
                    } else {
                        cmd.body.clone()
                    };
                    let display = if app.core.config.settings.redact_secrets {
                        engine::redact::redact(&format!("/{}", cmd.display))
                    } else {
                        format!("/{}", cmd.display)
                    };
                    let queued = QueuedInput::custom_command_request(display, text, cmd.overrides);
                    let target = smelt_core::lua::current_command_queue_target()
                        .map(crate::app::QueueStage::from_command_target)
                        .unwrap_or(crate::app::QueueStage::Turn);
                    match target {
                        crate::app::QueueStage::Turn => {
                            app.queued_inputs.try_push_turn(queued);
                        }
                        crate::app::QueueStage::Request => {
                            app.queue_input_for_request(queued);
                        }
                    }
                    return;
                }
                let turn = app.begin_custom_command_turn(cmd);
                app.agent = Some(turn);
            });
            Ok(())
        },
    )?;

    m.fn_(
        "submit_command_continuation",
        "Start an idle custom-command continuation without using the prompt queue. Returns `false` instead of queuing when a turn, compaction, or busy token is active or when `continuation_token` does not match the most recent completed turn. When it starts, the work elapsed timer carries forward from that completed turn.",
        &["name", "body", "overrides", "display", "continuation_token"],
        |_,
         (name, body, overrides, display, continuation_token): (
            String,
            String,
            Option<LuaCommandOverrides>,
            Option<String>,
            Option<u64>,
        )|
         -> LuaResult<bool> {
            let Some(continuation_token) = continuation_token else {
                return Ok(false);
            };
            let parsed = overrides.map(Into::into).unwrap_or_default();
            Ok(crate::lua::with_app(|app| {
                if app.prompt_input_is_busy() || !app.consume_continuation_token(continuation_token) {
                    return false;
                }
                let cmd = smelt_core::custom_commands::CustomCommand {
                    display: display.unwrap_or_else(|| name.clone()),
                    name,
                    body,
                    overrides: parsed,
                };
                let turn = app.begin_custom_command_continuation(cmd);
                app.agent = Some(turn);
                true
            }))
        },
    )?;

    // smelt.engine.ask({system, messages?, question?, model?, response_format?, reasoning_effort?, on_response})
    {
        let s = shared.clone();
        m.fn_(
            "ask",
            "Run an out-of-band LLM request without touching the main turn. `spec.model` selects an alternate model (defaults to the primary), `spec.response_format` enforces a JSON schema, `spec.reasoning_effort` controls effort (defaults to `\"off\"`). `spec.on_response` fires once with `(response, err)`, where `response` is a structured assistant message table on success. Returns the request id.",
            &["spec"],
            move |lua, spec: LuaAskSpec| -> LuaResult<u64> {
                if spec.system.is_empty() {
                    return Err(LuaError::external(
                        "smelt.engine.ask: `system` must be a non-empty string",
                    ));
                }

                let mut messages: Vec<protocol::Message> = spec
                    .messages
                    .as_ref()
                    .map(|table| crate::lua::api::session::lua_messages_to_protocol(lua, table))
                    .unwrap_or_default();

                let id = s.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                let stream = register_ask_callbacks(
                    &s,
                    lua,
                    id,
                    spec.on_response,
                    spec.on_delta,
                )?;

                let system = spec.system;
                let _guard = spec.guard;
                let response_format = spec.response_format.map(|f| protocol::AskResponseFormat {
                    name: f.name,
                    schema: smelt_core::lua::api::lua_table_to_json(lua, &f.schema),
                });
                let reasoning_effort = spec
                    .reasoning_effort
                    .map(Into::into)
                    .unwrap_or(protocol::ReasoningEffort::Off);
                let model_ref = spec.model;
                let question = spec.question;
                let visible_retries = spec.visible_retries.unwrap_or(false);
                crate::lua::with_app(|app| {
                    if let Some(q) = question {
                        messages.push(protocol::Message::user(protocol::Content::text(&q)));
                    }
                    let model = model_ref.and_then(|r| resolve_model_for_ask(app, &r));
                    let session_id = app.core.session.id.clone();
                    let session_dir = smelt_core::session::dir_for(&app.core.session);
                    app.core.engine.send(protocol::UiCommand::EngineAsk {
                        id,
                        system,
                        messages,
                        model,
                        response_format,
                        reasoning_effort,
                        tools: Vec::new(),
                        session_id,
                        session_dir,
                        stream,
                        visible_retries,
                    })
                });
                Ok(id)
            },
        )?;
    }

    // smelt.engine.ask_inherited({messages?, question?, model?, response_format?, reasoning_effort?, on_response})
    {
        let s = shared.clone();
        m.fn_(
            "ask_inherited",
            "Run an auxiliary LLM request that inherits the current session's assembled system prompt and active tool list. When `spec.messages` is omitted or empty, the live model-visible history is inherited exactly; otherwise the supplied full `protocol::Message` rows override the inherited history while preserving the same prompt structure. `spec.on_response` fires once with `(response, err)`, where `response` is a structured assistant message table on success. Returns the request id.",
            &["spec"],
            move |lua, spec: LuaInheritedAskSpec| -> LuaResult<u64> {
                let mut messages: Vec<protocol::Message> = spec
                    .messages
                    .as_ref()
                    .map(|table| crate::lua::api::session::lua_messages_to_protocol(lua, table))
                    .unwrap_or_default();

                let id = s.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                let stream = register_ask_callbacks(
                    &s,
                    lua,
                    id,
                    spec.on_response,
                    spec.on_delta,
                )?;

                let _guard = spec.guard;
                let response_format = spec.response_format.map(|f| protocol::AskResponseFormat {
                    name: f.name,
                    schema: smelt_core::lua::api::lua_table_to_json(lua, &f.schema),
                });
                let reasoning_effort = spec
                    .reasoning_effort
                    .map(Into::into)
                    .unwrap_or(protocol::ReasoningEffort::Off);
                let model_ref = spec.model;
                let question = spec.question;
                let visible_retries = spec.visible_retries.unwrap_or(false);
                crate::lua::with_app(|app| {
                    let system = app.assemble_system_prompt();
                    if messages.is_empty() {
                        messages = app.model_history_messages();
                    }
                    if let Some(q) = question {
                        messages.push(protocol::Message::user(protocol::Content::text(&q)));
                    }
                    let model = model_ref.and_then(|r| resolve_model_for_ask(app, &r));
                    let session_id = app.core.session.id.clone();
                    let session_dir = smelt_core::session::dir_for(&app.core.session);
                    app.core.engine.send(protocol::UiCommand::EngineAsk {
                        id,
                        system,
                        messages,
                        model,
                        response_format,
                        reasoning_effort,
                        tools: app.lua.tool_defs(
                            app.core.config.mode.clone(),
                            smelt_core::lua::ToolVisibility::Interactive,
                        ),
                        session_id,
                        session_dir,
                        stream,
                        visible_retries,
                    })
                });
                Ok(id)
            },
        )?;
    }

    Ok(())
}

fn register_ask_callbacks(
    shared: &Arc<LuaShared>,
    lua: &Lua,
    id: u64,
    on_response: Option<LuaCallback<(mlua::Value, Option<LuaAskErrorTable>), ()>>,
    on_delta: Option<LuaCallback<(String,), ()>>,
) -> LuaResult<bool> {
    let stream = on_delta.is_some();
    let response = on_response
        .map(|cb| LuaHandle::from_func(lua, cb.into_inner()))
        .transpose()?;
    let delta = on_delta
        .map(|cb| LuaHandle::from_func(lua, cb.into_inner()))
        .transpose()?;
    if response.is_some() || delta.is_some() {
        if let Ok(mut cbs) = shared.ask_callbacks.lock() {
            cbs.insert(id, AskCallbacks { response, delta });
        }
    }
    Ok(stream)
}

/// Resolve a Lua-provided model reference into an `AskModel` carrying api
/// base / key / provider type. Notifies on error and returns `None`.
fn resolve_model_for_ask(
    app: &mut crate::app::TuiApp,
    reference: &str,
) -> Option<protocol::AskModel> {
    let resolved =
        match smelt_core::config::resolve_model_ref(&app.core.config.available_models, reference) {
            Ok(m) => m.clone(),
            Err(err) => {
                app.notify_error_sticky(format!("smelt.engine: {err}"));
                return None;
            }
        };
    let api_key = app
        .resolve_api_key_for_env(&resolved.api_key_env)
        .unwrap_or_default();
    Some(protocol::AskModel {
        model: resolved.model_name,
        api_base: resolved.api_base,
        api_key,
        provider_type: resolved.provider_type,
    })
}
