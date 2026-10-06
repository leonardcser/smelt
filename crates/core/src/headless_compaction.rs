//! Headless adapter for the shared Lua compaction algorithm.

use std::{cell::RefCell, collections::HashSet, rc::Rc, sync::Arc};

use engine::{HostCall, HostRequestDecision};
use mlua::{Function, Lua, Table, Value};
use protocol::{EngineEvent, Message, ModelHistoryCoordinates};

use crate::{
    lua::{
        ask::{self, AskContext, AskRequest},
        LuaRuntime, LuaShared,
    },
    runtime::Core,
    session::Session,
};

struct State {
    session: Session,
    generation: u64,
    checkpoint_changed: bool,
    estimated_tokens: u32,
    pending_asks: HashSet<u64>,
    system: String,
    tools: Vec<protocol::ToolDef>,
}

pub(crate) struct HeadlessCompaction {
    state: Rc<RefCell<State>>,
    shared: Arc<LuaShared>,
    prepare: Function,
    recover: Function,
}

impl HeadlessCompaction {
    pub(crate) fn new(
        runtime: &LuaRuntime,
        session: &Session,
        system: String,
        tools: Vec<protocol::ToolDef>,
    ) -> mlua::Result<Self> {
        let lua = &runtime.lua;
        let state = Rc::new(RefCell::new(State {
            session: session.clone(),
            generation: 0,
            checkpoint_changed: false,
            estimated_tokens: 0,
            pending_asks: HashSet::new(),
            system,
            tools,
        }));
        let host = lua.create_table()?;
        let s = state.clone();
        host.set(
            "generation",
            lua.create_function(move |_, ()| Ok(s.borrow().generation))?,
        )?;
        let s = state.clone();
        host.set(
            "messages",
            lua.create_function(move |lua, ()| {
                crate::lua::serde_to_lua_preserving_nulls(
                    lua,
                    &protocol::history_to_messages(&s.borrow().session.model_history()),
                )
            })?,
        )?;
        let s = state.clone();
        host.set(
            "context_tokens",
            lua.create_function(move |_, ()| Ok(s.borrow().session.context_tokens))?,
        )?;
        host.set(
            "context_window",
            lua.create_function(|_, ()| {
                Ok(crate::host::with_core(|core| {
                    core.config.active_model().and_then(|model| {
                        model
                            .config
                            .context_window
                            .or(core.config.context_window)
                            .or_else(|| {
                                smelt_provider::catalog::context_window(
                                    &model.provider_type,
                                    &model.api_base,
                                    &model.model_name,
                                )
                            })
                    })
                }))
            })?,
        )?;
        let s = state.clone();
        host.set(
            "checkpoint",
            lua.create_function(move |_, spec: Table| {
                let mut state = s.borrow_mut();
                let before = crate::session::estimate_message_tokens(
                    &protocol::history_to_messages(&state.session.model_history()),
                );
                let installed = state.session.install_context_checkpoint(
                    spec.get("kind")?,
                    spec.get("summary")?,
                    spec.get("first_live_message_index")?,
                    spec.get("tokens_before")?,
                );
                if installed {
                    let after = crate::session::estimate_message_tokens(
                        &protocol::history_to_messages(&state.session.model_history()),
                    );
                    let estimate = state
                        .estimated_tokens
                        .saturating_sub(before)
                        .saturating_add(after);
                    let len = state.session.history.len();
                    state
                        .session
                        .record_checkpoint_tokens_after_estimate(estimate, len);
                    state.checkpoint_changed = true;
                }
                Ok(installed)
            })?,
        )?;
        let s = state.clone();
        let shared = Arc::clone(runtime.shared());
        host.set(
            "ask",
            lua.create_function(move |lua, spec: Table| {
                let messages: Vec<Message> =
                    crate::lua::lua_to_serde(lua, &spec.get::<Value>("messages")?)
                        .ok_or_else(|| mlua::Error::external("invalid compaction messages"))?;
                let (id, stream) = ask::register_callbacks(
                    &shared,
                    lua,
                    spec.get("on_response")?,
                    spec.get("on_delta")?,
                    spec.get("on_draft_rejected")?,
                )?;
                let result = crate::host::with_core(|core| -> mlua::Result<()> {
                    let model = ask::select_model(
                        &core.config,
                        spec.get::<Option<String>>("model")?.as_deref(),
                    )
                    .map_err(mlua::Error::external)?;
                    let key = std::env::var(&model.api_key_env).unwrap_or_default();
                    let state = s.borrow();
                    let context = AskContext {
                        system: state.system.clone(),
                        tools: state.tools.clone(),
                        reasoning_effort: core.config.reasoning_effort.clone(),
                        fast_mode: core.config.settings.fast_mode,
                        session_id: state.session.id.clone(),
                        persistence: protocol::PersistenceScope::default(),
                    };
                    let request = AskRequest {
                        id,
                        messages,
                        stream,
                        visible_retries: true,
                        ..Default::default()
                    };
                    let command = ask::prepare_request(
                        &core.config,
                        context,
                        request,
                        model.target(key),
                        &model.reasoning_catalog(),
                    )
                    .map_err(mlua::Error::external)?;
                    core.engine.send(command);
                    Ok(())
                });
                if let Err(error) = result {
                    ask::remove_callbacks(&shared, id);
                    return Err(error);
                }
                s.borrow_mut().pending_asks.insert(id);
                Ok(id)
            })?,
        )?;
        let factory: Function = lua.load(r#"
            return function(host)
                host.state = smelt.state.get("compact")
                host.settings = smelt.settings
                host.truncate = smelt.text.truncate
                host.is_cancelled = smelt.task.is_cancelled
                host.notify = smelt.notify
                host.log = smelt.log.info
                host.preferred_model = function() return smelt.state.persistent("model_preferred").compact end
                host.guard = host.generation
                host.guard_current = function(g) return g == host.generation() end
                host.begin_work = function()
                    local generation, alive = host.generation(), true
                    return {
                        alive = function() return alive and generation == host.generation() end,
                        remove = function() alive = false end,
                    }
                end
                return require("smelt.compact").new(host)
            end
        "#).eval()?;
        host.set(
            "summary_prefix",
            protocol::COMPACTION_SUMMARY_PREFIX.trim_end(),
        )?;
        let hooks: Table = factory.call(host)?;
        Ok(Self {
            state,
            shared: Arc::clone(runtime.shared()),
            prepare: hooks.get("prepare_request")?,
            recover: hooks.get("context_limit")?,
        })
    }

    pub(crate) fn dispatch(
        &self,
        lua: &Lua,
        core: &mut Core,
        session: &Session,
        call: HostCall,
    ) -> mlua::Result<()> {
        let (hook, payload, reply) = match call {
            HostCall::PrepareRequest {
                messages,
                estimated_tokens,
                reply,
                ..
            } => {
                let mut state = self.state.borrow_mut();
                state.session = session.clone();
                state.estimated_tokens = estimated_tokens;
                let len = session.history.len();
                let checkpoint = session.checkpoint.as_ref().and_then(|cp| {
                    cp.tokens_after_estimate
                        .map(|tokens| (tokens, cp.tokens_after_estimate_history_len))
                });
                let base = session
                    .context_tokens_history_len
                    .or_else(|| checkpoint.and_then(|(_, len)| len))
                    .unwrap_or(len);
                let delta = session.history.get(base..).unwrap_or_default();
                let estimate = crate::context_estimate::PrepareContextEstimate::from_history_delta(
                    session.context_tokens,
                    session.context_tokens_history_len,
                    checkpoint,
                    len,
                    delta,
                    messages.model(),
                    estimated_tokens,
                );
                let payload = lua.create_table()?;
                payload.set(
                    "messages",
                    crate::lua::serde_to_lua_preserving_nulls(lua, &messages.model())?,
                )?;
                payload.set("estimated_tokens", estimated_tokens)?;
                payload.set("estimated_context_tokens", estimate.total_context_tokens)?;
                payload.set("context_estimate", estimate.into_lua_table(lua)?)?;
                let payload = Value::Table(payload);
                (self.prepare.clone(), payload, reply)
            }
            HostCall::RecoverFromContextLimit {
                messages, reply, ..
            } => {
                self.state.borrow_mut().session = session.clone();
                (
                    self.recover.clone(),
                    crate::lua::serde_to_lua_preserving_nulls(lua, &messages)?,
                    reply,
                )
            }
            HostCall::ProviderResponse { reply, .. } => {
                let _ = reply.send(None);
                return Ok(());
            }
            HostCall::RequestAudit { .. } => return Ok(()),
        };
        self.state.borrow_mut().generation += 1;
        let state = self.state.clone();
        let reply = Rc::new(RefCell::new(Some(reply)));
        let reply_fn = lua.create_function(move |lua, value: Value| {
            let decision = match value {
                Value::Table(table) => match table.get::<Option<String>>("action")?.as_deref() {
                    Some("abort") => HostRequestDecision::Abort(table.get("message")?),
                    Some("replace")
                        if table.get::<Option<String>>("source")?.as_deref()
                            == Some("model_history") =>
                    {
                        let state = state.borrow();
                        let coordinates = state
                            .session
                            .checkpoint
                            .as_ref()
                            .map_or(ModelHistoryCoordinates::canonical(), |cp| {
                                ModelHistoryCoordinates::projected(1, cp.first_live_index)
                            });
                        HostRequestDecision::replace_model_history(
                            state.session.model_history(),
                            coordinates,
                        )
                    }
                    Some("replace") => HostRequestDecision::replace_canonical_history(
                        crate::lua::lua_to_serde(lua, &table.get::<Value>("messages")?)
                            .ok_or_else(|| mlua::Error::external("invalid replacement messages"))?,
                    ),
                    _ => HostRequestDecision::Continue,
                },
                _ => HostRequestDecision::Continue,
            };
            if let Some(reply) = reply.borrow_mut().take() {
                let _ = reply.send(decision);
            }
            Ok(())
        })?;
        crate::host::scope_core(core, || hook.call::<()>((payload, reply_fn)))
    }

    pub(crate) fn handle_event(
        &self,
        lua: &Lua,
        core: &mut Core,
        event: &EngineEvent,
    ) -> mlua::Result<bool> {
        let id = match event {
            EngineEvent::EngineAskResponse { id, .. }
            | EngineEvent::EngineAskDelta { id, .. }
            | EngineEvent::EngineAskDraftRejected { id } => *id,
            _ => return Ok(false),
        };
        if !self.state.borrow().pending_asks.contains(&id) {
            return Ok(false);
        }
        if matches!(event, EngineEvent::EngineAskResponse { .. }) {
            self.state.borrow_mut().pending_asks.remove(&id);
        }
        crate::host::scope_core(core, || ask::dispatch_callbacks(&self.shared, lua, event))
    }

    pub(crate) fn sync_checkpoint(&self, session: &mut Session) {
        let mut state = self.state.borrow_mut();
        if state.checkpoint_changed {
            *session = state.session.clone();
            state.checkpoint_changed = false;
        }
    }

    pub(crate) fn cancel(&self) {
        let mut state = self.state.borrow_mut();
        state.generation += 1;
        for id in state.pending_asks.drain() {
            ask::remove_callbacks(&self.shared, id);
        }
    }
}
