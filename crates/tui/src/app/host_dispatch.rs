//! Drain `engine::HostCall` requests and run the matching Lua hooks
//! on the TUI main thread. Each `HostCall` carries its own
//! `oneshot::Sender` for the reply; we send back at most once.

use crate::app::TuiApp;
use engine::{HostCall, HostRequestDecision};
use protocol::Message;
use smelt_core::lua::{HookRegistry, LuaShared};
use smelt_core::working::TurnPhase;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

type MessageReply = oneshot::Sender<HostRequestDecision>;
type MessageReplySlot = Arc<Mutex<Option<MessageReply>>>;
const PREPARE_CONTEXT_HISTORY_DELTA_MAX_ITEMS: usize = 256;

fn restore_working_phase() {
    crate::lua::try_with_app(|app| {
        if app.agent_is_running() {
            app.working.begin(TurnPhase::Working);
        }
    });
}

fn current_model_history_decision() -> HostRequestDecision {
    crate::lua::try_with_app(|app| HostRequestDecision::Replace(app.model_history_messages()))
        .unwrap_or(HostRequestDecision::Continue)
}

fn request_decision_from_lua(
    inner_lua: &mlua::Lua,
    value: mlua::Value,
) -> mlua::Result<HostRequestDecision> {
    let mlua::Value::Table(t) = value else {
        return Ok(HostRequestDecision::Continue);
    };

    match t.get::<Option<String>>("action")?.as_deref() {
        Some("continue") | None => Ok(HostRequestDecision::Continue),
        Some("abort") => Ok(HostRequestDecision::Abort(t.get::<String>("message")?)),
        Some("replace") => {
            if t.get::<Option<String>>("source")?.as_deref() == Some("model_history") {
                return Ok(current_model_history_decision());
            }
            Ok(t.get::<mlua::Value>("messages")
                .ok()
                .and_then(|v| smelt_core::lua::lua_to_serde::<Vec<Message>>(inner_lua, &v))
                .map(HostRequestDecision::Replace)
                .unwrap_or(HostRequestDecision::Continue))
        }
        Some(_) => Ok(HostRequestDecision::Continue),
    }
}

fn create_message_reply_fn(
    lua: &mlua::Lua,
    reply_for_closure: MessageReplySlot,
    compact_phase: bool,
) -> mlua::Result<mlua::Function> {
    lua.create_function(move |inner_lua, value: mlua::Value| {
        if compact_phase {
            restore_working_phase();
        }
        let Some(tx) = reply_for_closure.lock().ok().and_then(|mut g| g.take()) else {
            // Already replied; subsequent calls are silent no-ops so
            // hook authors aren't punished for defensive double-calls.
            return Ok(());
        };
        let decision = request_decision_from_lua(inner_lua, value)?;
        let _ = tx.send(decision);
        Ok(())
    })
}

fn prepare_request_to_lua(
    lua: &mlua::Lua,
    messages: Vec<Message>,
    estimated_tokens: u32,
    context_estimate: PrepareContextEstimate,
) -> mlua::Result<mlua::Table> {
    let request = lua.create_table()?;
    request.set("estimated_tokens", estimated_tokens)?;
    request.set(
        "estimated_context_tokens",
        context_estimate.total_context_tokens,
    )?;
    request.set("context_estimate", context_estimate.into_lua_table(lua)?)?;

    let messages = Rc::new(messages);
    let mt = lua.create_table()?;
    mt.set(
        "__index",
        lua.create_function(move |lua, (table, key): (mlua::Table, mlua::Value)| {
            if let mlua::Value::String(s) = key {
                if s.to_str()?.as_ref() == "messages" {
                    let value = smelt_core::lua::serde_to_lua(lua, messages.as_ref())?;
                    table.raw_set("messages", value.clone())?;
                    return Ok(value);
                }
            }
            Ok(mlua::Value::Nil)
        })?,
    )?;
    request.set_metatable(Some(mt))?;
    Ok(request)
}

impl TuiApp {
    /// Pull every pending `HostCall` and dispatch it. Non-blocking;
    /// returns once the channel reports `Empty`.
    pub(crate) fn drain_host_calls(&mut self) {
        loop {
            let call = match self.host_rx.try_recv() {
                Ok(c) => c,
                Err(_) => break,
            };
            self.dispatch_host_call(call);
        }
    }

    pub(crate) fn dispatch_host_call(&mut self, call: HostCall) {
        match call {
            HostCall::ProviderResponse { message, reply } => {
                let mutated = self.run_middleware_chain::<Message>(message, "on_response", |s| {
                    &s.hooks.provider_response
                });
                let _ = reply.send(mutated);
            }
            HostCall::RecoverFromContextLimit { messages, reply } => {
                self.dispatch_recover_from_context_limit(messages, reply);
            }
            HostCall::RequestAudit {
                session_dir,
                entry,
                payload_mode,
            } => {
                if !self.ephemeral() && !self.session_access.is_read_only() {
                    self.persister
                        .append_request_audit(crate::persist::PersistRequestAudit {
                            session_id: self.core.session.id.clone(),
                            session_dir,
                            entry: *entry,
                            payload_mode,
                        });
                }
            }
            HostCall::PrepareRequest {
                messages,
                estimated_tokens,
                reply,
            } => {
                self.dispatch_prepare_request(messages, estimated_tokens, reply);
            }
        }
    }

    /// Hand the first registered `smelt.engine.on_context_limit` hook the
    /// truncated history along with a Lua `reply` function whose body
    /// holds the engine's `oneshot::Sender`. The hook MUST call `reply`
    /// exactly once with `{ action = "replace", messages = ... }` (engine
    /// swaps and retries), `{ action = "abort", message = ... }` (engine
    /// aborts the turn), or `nil`/`{ action = "continue" }` (engine
    /// continues with the original request). If the hook vanishes without
    /// calling `reply`, the wrapping closure is GC'd, the Sender drops,
    /// and the engine's `.await` resolves to `None`.
    fn dispatch_recover_from_context_limit(&mut self, messages: Vec<Message>, reply: MessageReply) {
        self.call_message_reply_hook(
            "on_context_limit",
            |s| &s.hooks.context_limit,
            messages,
            reply,
            true,
            |_, func, messages_table, reply_fn| func.call::<()>((messages_table, reply_fn)),
        );
    }

    /// Hand the first registered `smelt.engine.on_prepare_request`
    /// hook the request metadata immediately. The `messages` field is built
    /// lazily if the hook reads it, so metadata-only hooks do not pay to
    /// serialize large histories into Lua.
    fn dispatch_prepare_request(
        &mut self,
        messages: Vec<Message>,
        estimated_tokens: u32,
        reply: MessageReply,
    ) {
        while let Ok(ev) = self.core.engine.try_recv() {
            self.dispatch_engine_event(ev);
        }
        let lua = self.lua.lua();
        let funcs = self
            .lua
            .core_shared()
            .hooks
            .prepare_request
            .snapshot_for(lua, "");
        let Some(func) = funcs.into_iter().next() else {
            let _ = reply.send(HostRequestDecision::Continue);
            return;
        };
        let identity = self.active_context_token_identity();
        let current_history_len = self.session_history_len();
        let checkpoint_context_tokens =
            self.core
                .session
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| {
                    checkpoint
                        .tokens_after_estimate
                        .map(|tokens| (tokens, checkpoint.tokens_after_estimate_history_len))
                });
        let base_history_len = self
            .core
            .session
            .context_tokens_history_len
            .or_else(|| checkpoint_context_tokens.and_then(|(_, len)| len))
            .unwrap_or(current_history_len);
        let history_delta_len = current_history_len.saturating_sub(base_history_len);
        let context_estimate = if history_delta_len > PREPARE_CONTEXT_HISTORY_DELTA_MAX_ITEMS {
            PrepareContextEstimate::full_request(estimated_tokens, current_history_len)
        } else {
            let history_delta = if base_history_len < current_history_len {
                self.session_history_range(base_history_len..current_history_len)
            } else {
                Vec::new()
            };
            PrepareContextEstimate::from_history_delta(
                self.core.session.context_tokens_for(&identity),
                self.core.session.context_tokens_history_len,
                checkpoint_context_tokens,
                current_history_len,
                &history_delta,
                &messages,
                estimated_tokens,
            )
        };
        if self.agent_is_running() {
            self.working.begin(TurnPhase::Compacting);
        }
        let reply_slot: MessageReplySlot = Arc::new(Mutex::new(Some(reply)));
        let reply_for_closure = Arc::clone(&reply_slot);
        let reply_fn = match create_message_reply_fn(lua, reply_for_closure, true) {
            Ok(f) => f,
            Err(e) => {
                self.lua
                    .record_error(format!("on_prepare_request: build reply: {e}"));
                restore_working_phase();
                if let Some(tx) = reply_slot.lock().ok().and_then(|mut g| g.take()) {
                    let _ = tx.send(HostRequestDecision::Continue);
                }
                return;
            }
        };
        let request =
            match prepare_request_to_lua(lua, messages, estimated_tokens, context_estimate) {
                Ok(request) => request,
                Err(e) => {
                    self.lua
                        .record_error(format!("on_prepare_request: build request: {e}"));
                    restore_working_phase();
                    if let Some(tx) = reply_slot.lock().ok().and_then(|mut g| g.take()) {
                        let _ = tx.send(HostRequestDecision::Continue);
                    }
                    return;
                }
            };
        match func.call::<()>((request, reply_fn)) {
            Ok(()) => {
                let replied = reply_slot.lock().ok().is_some_and(|g| g.is_none());
                if replied && self.agent_is_running() {
                    self.working.begin(TurnPhase::Working);
                }
            }
            Err(e) => {
                self.lua.record_error(format!("on_prepare_request: {e}"));
                restore_working_phase();
                if let Some(tx) = reply_slot.lock().ok().and_then(|mut g| g.take()) {
                    let _ = tx.send(HostRequestDecision::Continue);
                }
            }
        }
    }

    fn call_message_reply_hook(
        &mut self,
        label: &'static str,
        registry: impl Fn(&LuaShared) -> &Arc<HookRegistry>,
        messages: Vec<Message>,
        reply: MessageReply,
        compact_phase: bool,
        call: impl FnOnce(&mlua::Lua, mlua::Function, mlua::Value, mlua::Function) -> mlua::Result<()>,
    ) {
        let lua = self.lua.lua();
        let funcs = registry(self.lua.core_shared()).snapshot_for(lua, "");
        let Some(func) = funcs.into_iter().next() else {
            let _ = reply.send(HostRequestDecision::Continue);
            return;
        };
        let messages_table = match smelt_core::lua::serde_to_lua(lua, &messages) {
            Ok(v) => v,
            Err(e) => {
                self.lua
                    .record_error(format!("{label}: serialize messages: {e}"));
                let _ = reply.send(HostRequestDecision::Continue);
                return;
            }
        };
        if compact_phase && self.agent_is_running() {
            self.working.begin(TurnPhase::Compacting);
        }
        let reply_slot: MessageReplySlot = Arc::new(Mutex::new(Some(reply)));
        let reply_for_closure = Arc::clone(&reply_slot);
        let reply_fn = match create_message_reply_fn(lua, reply_for_closure, compact_phase) {
            Ok(f) => f,
            Err(e) => {
                self.lua.record_error(format!("{label}: build reply: {e}"));
                if compact_phase {
                    restore_working_phase();
                }
                if let Some(tx) = reply_slot.lock().ok().and_then(|mut g| g.take()) {
                    let _ = tx.send(HostRequestDecision::Continue);
                }
                return;
            }
        };
        match call(lua, func, messages_table, reply_fn) {
            Ok(()) => {
                let replied = reply_slot.lock().ok().is_some_and(|g| g.is_none());
                if compact_phase && replied && self.agent_is_running() {
                    self.working.begin(TurnPhase::Working);
                }
            }
            Err(e) => {
                self.lua.record_error(format!("{label}: {e}"));
                if compact_phase {
                    restore_working_phase();
                }
                if let Some(tx) = reply_slot.lock().ok().and_then(|mut g| g.take()) {
                    let _ = tx.send(HostRequestDecision::Continue);
                }
            }
        }
    }

    /// Snapshot a `HookRegistry`, serialize `payload` into Lua via serde,
    /// call each hook in registration order (passing the previous hook's
    /// returned table as input), and deserialize the final value back
    /// into `T`. Returns `None` when no hook is registered or no hook
    /// returned a replacement table - caller treats `None` as "no
    /// mutation, proceed with the original payload".
    fn run_middleware_chain<T>(
        &self,
        payload: T,
        label: &'static str,
        registry: impl Fn(&LuaShared) -> &Arc<HookRegistry>,
    ) -> Option<T>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let lua = self.lua.lua();
        let funcs = registry(self.lua.core_shared()).snapshot_for(lua, "");
        if funcs.is_empty() {
            return None;
        }
        let mut current = smelt_core::lua::serde_to_lua(lua, &payload).ok()?;
        let mut mutated = false;
        for func in funcs {
            match func.call::<mlua::Value>(current.clone()) {
                Ok(mlua::Value::Table(t)) => {
                    current = mlua::Value::Table(t);
                    mutated = true;
                }
                Ok(_) => {}
                Err(e) => {
                    self.lua
                        .record_error(format!("provider.middleware {label}: {e}"));
                }
            }
        }
        if !mutated {
            return None;
        }
        smelt_core::lua::lua_to_serde::<T>(lua, &current)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrepareContextEstimateSource {
    FullRequestEstimate,
    ProviderSnapshot,
    ProviderSnapshotPlusHistoryDelta,
    CheckpointEstimate,
    CheckpointEstimatePlusHistoryDelta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrepareContextEstimate {
    total_context_tokens: u32,
    provider_context_tokens: Option<u32>,
    estimated_delta_tokens: u32,
    latest_snapshot_history_len: Option<usize>,
    current_history_len: usize,
    source: PrepareContextEstimateSource,
}

impl PrepareContextEstimate {
    #[cfg(test)]
    fn from_request(
        current_context_tokens: Option<u32>,
        context_tokens_history_len: Option<usize>,
        current_history: &[protocol::HistoryItem],
        request_messages: &[Message],
        full_request_estimate: u32,
    ) -> Self {
        let base_history_len = context_tokens_history_len.unwrap_or(current_history.len());
        let history_delta = if base_history_len < current_history.len() {
            &current_history[base_history_len..]
        } else {
            &[]
        };
        Self::from_history_delta(
            current_context_tokens,
            context_tokens_history_len,
            None,
            current_history.len(),
            history_delta,
            request_messages,
            full_request_estimate,
        )
    }

    fn from_history_delta(
        current_context_tokens: Option<u32>,
        context_tokens_history_len: Option<usize>,
        checkpoint_context_tokens: Option<(u32, Option<usize>)>,
        current_history_len: usize,
        history_delta: &[protocol::HistoryItem],
        _request_messages: &[Message],
        full_request_estimate: u32,
    ) -> Self {
        let (base, base_history_len, exact_source, delta_source) =
            if let Some(base) = current_context_tokens {
                (
                    base,
                    context_tokens_history_len,
                    PrepareContextEstimateSource::ProviderSnapshot,
                    PrepareContextEstimateSource::ProviderSnapshotPlusHistoryDelta,
                )
            } else if let Some((base, history_len)) = checkpoint_context_tokens {
                (
                    base,
                    history_len,
                    PrepareContextEstimateSource::CheckpointEstimate,
                    PrepareContextEstimateSource::CheckpointEstimatePlusHistoryDelta,
                )
            } else {
                return Self::full_request(full_request_estimate, current_history_len);
            };
        let base_history_len = base_history_len.unwrap_or(current_history_len);

        if base_history_len > current_history_len {
            return Self::full_request(full_request_estimate, current_history_len);
        }

        if base_history_len == current_history_len {
            return Self {
                total_context_tokens: base,
                provider_context_tokens: current_context_tokens,
                estimated_delta_tokens: 0,
                latest_snapshot_history_len: context_tokens_history_len,
                current_history_len,
                source: exact_source,
            };
        }

        let added_messages = protocol::history_to_messages(history_delta);
        let estimated_delta_tokens = smelt_core::session::estimate_message_tokens(&added_messages);
        Self {
            total_context_tokens: base.saturating_add(estimated_delta_tokens),
            provider_context_tokens: current_context_tokens,
            estimated_delta_tokens,
            latest_snapshot_history_len: context_tokens_history_len,
            current_history_len,
            source: delta_source,
        }
    }

    fn full_request(full_request_estimate: u32, current_history_len: usize) -> Self {
        Self {
            total_context_tokens: full_request_estimate,
            provider_context_tokens: None,
            estimated_delta_tokens: full_request_estimate,
            latest_snapshot_history_len: None,
            current_history_len,
            source: PrepareContextEstimateSource::FullRequestEstimate,
        }
    }

    fn into_lua_table(self, lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
        let table = lua.create_table()?;
        table.set("source", self.source.as_str())?;
        table.set("total_context_tokens", self.total_context_tokens)?;
        table.set("provider_context_tokens", self.provider_context_tokens)?;
        table.set("estimated_delta_tokens", self.estimated_delta_tokens)?;
        table.set(
            "latest_snapshot_history_len",
            self.latest_snapshot_history_len,
        )?;
        table.set("current_history_len", self.current_history_len)?;
        Ok(table)
    }
}

impl PrepareContextEstimateSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::FullRequestEstimate => "full_request_estimate",
            Self::ProviderSnapshot => "provider_snapshot",
            Self::ProviderSnapshotPlusHistoryDelta => "provider_snapshot_plus_history_delta",
            Self::CheckpointEstimate => "checkpoint_estimate",
            Self::CheckpointEstimatePlusHistoryDelta => "checkpoint_estimate_plus_history_delta",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{AssistantStep, Content, HistoryItem, ToolInvocation, ToolOutcome};

    #[test]
    fn prepare_context_estimate_uses_full_estimate_without_provider_baseline() {
        let estimate = PrepareContextEstimate::from_request(None, None, &[], &[], 123);

        assert_eq!(estimate.total_context_tokens, 123);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::FullRequestEstimate
        );
    }

    #[test]
    fn prepare_context_estimate_adds_only_history_after_latest_token_snapshot() {
        let history = vec![
            HistoryItem::user(Content::text("old")),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("reply")),
                None,
                Vec::new(),
            )),
            HistoryItem::user(Content::text("new prompt")),
        ];
        let request_messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_request(
            Some(100),
            Some(2),
            &history,
            &request_messages,
            10_000,
        );

        assert!(estimate.total_context_tokens > 100);
        assert!(estimate.total_context_tokens < 10_000);
        assert_eq!(estimate.provider_context_tokens, Some(100));
        assert!(estimate.estimated_delta_tokens > 0);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshotPlusHistoryDelta
        );
    }

    #[test]
    fn prepare_context_estimate_uses_checkpoint_estimate_until_provider_baseline() {
        let history = vec![
            HistoryItem::user(Content::text("checkpoint summary")),
            HistoryItem::user(Content::text("live suffix")),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("new reply")),
                None,
                Vec::new(),
            )),
        ];
        let history_delta = &history[2..];
        let messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_history_delta(
            None,
            None,
            Some((80, Some(2))),
            history.len(),
            history_delta,
            &messages,
            10_000,
        );

        assert!(estimate.total_context_tokens > 80);
        assert!(estimate.total_context_tokens < 10_000);
        assert_eq!(estimate.provider_context_tokens, None);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::CheckpointEstimatePlusHistoryDelta
        );
    }

    #[test]
    fn prepare_context_estimate_stays_at_baseline_when_snapshot_covers_history() {
        let history = vec![
            HistoryItem::user(Content::text("old")),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("reply")),
                None,
                Vec::new(),
            )),
        ];
        let request_messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_request(
            Some(100),
            Some(2),
            &history,
            &request_messages,
            10_000,
        );

        assert_eq!(estimate.total_context_tokens, 100);
        assert_eq!(estimate.estimated_delta_tokens, 0);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshot
        );
    }

    #[test]
    fn prepare_context_estimate_does_not_double_count_snapshotted_tool_output() {
        let history = vec![
            HistoryItem::user(Content::text("run tests")),
            HistoryItem::assistant(AssistantStep::with_invocations(
                None,
                None,
                Vec::new(),
                vec![ToolInvocation {
                    call_id: "call-1".into(),
                    name: "bash".into(),
                    arguments: r#"{"cmd":"cargo nextest run"}"#.into(),
                    result: ToolOutcome {
                        content: "test output\n".repeat(30_000),
                        is_error: false,
                        metadata: None,
                    },
                    elapsed_ms: None,
                }],
            )),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("done")),
                None,
                Vec::new(),
            )),
        ];
        let messages = protocol::history_to_messages(&history);

        assert!(matches!(
            messages
                .get(messages.len().saturating_sub(2))
                .map(|m| m.role),
            Some(protocol::Role::Tool)
        ));
        let estimate = PrepareContextEstimate::from_request(
            Some(171_359),
            Some(history.len()),
            &history,
            &messages,
            250_000,
        );

        assert_eq!(estimate.total_context_tokens, 171_359);
        assert_eq!(estimate.estimated_delta_tokens, 0);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshot
        );
    }

    #[test]
    fn prepare_context_estimate_adds_delta_when_baseline_predates_tool_output() {
        let history = vec![
            HistoryItem::user(Content::text("run tests")),
            HistoryItem::assistant(AssistantStep::with_invocations(
                None,
                None,
                Vec::new(),
                vec![ToolInvocation {
                    call_id: "call-1".into(),
                    name: "bash".into(),
                    arguments: r#"{"cmd":"cargo nextest run"}"#.into(),
                    result: ToolOutcome {
                        content: "test output\n".repeat(30_000),
                        is_error: false,
                        metadata: None,
                    },
                    elapsed_ms: None,
                }],
            )),
        ];
        let messages = protocol::history_to_messages(&history);
        let estimate = PrepareContextEstimate::from_request(
            Some(171_359),
            Some(1), // baseline recorded after user message, before assistant + tool
            &history,
            &messages,
            250_000,
        );

        assert!(estimate.total_context_tokens > 171_359);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshotPlusHistoryDelta
        );
    }

    #[test]
    fn prepare_context_estimate_falls_back_to_full_history_len_when_baseline_len_missing() {
        let history = vec![
            HistoryItem::user(Content::text("run command")),
            HistoryItem::assistant(AssistantStep::with_invocations(
                None,
                None,
                Vec::new(),
                vec![ToolInvocation {
                    call_id: "call-1".into(),
                    name: "bash".into(),
                    arguments: r#"{"cmd":"printf hello"}"#.into(),
                    result: ToolOutcome {
                        content: "hello\n".repeat(100),
                        is_error: false,
                        metadata: None,
                    },
                    elapsed_ms: None,
                }],
            )),
        ];
        let messages = protocol::history_to_messages(&history);
        // No context_tokens_history_len - defaults to current_history.len(),
        // so the baseline is assumed to cover the full history and no delta
        // is added.
        let estimate =
            PrepareContextEstimate::from_request(Some(1_000), None, &history, &messages, 10_000);

        assert_eq!(estimate.total_context_tokens, 1_000);
        assert_eq!(estimate.estimated_delta_tokens, 0);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::ProviderSnapshot
        );
    }

    #[test]
    fn prepare_context_estimate_uses_full_request_when_baseline_cleared() {
        let history = vec![
            HistoryItem::user(Content::text("hello")),
            HistoryItem::assistant(AssistantStep::terminal(
                Some(Content::text("hi")),
                None,
                Vec::new(),
            )),
        ];
        let messages = protocol::history_to_messages(&history);
        let estimate =
            PrepareContextEstimate::from_request(None, None, &history, &messages, 10_000);

        assert_eq!(estimate.total_context_tokens, 10_000);
        assert_eq!(estimate.provider_context_tokens, None);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::FullRequestEstimate
        );
    }

    #[test]
    fn prepare_context_estimate_uses_full_request_when_baseline_is_ahead() {
        let history = vec![HistoryItem::user(Content::text("rewound"))];
        let messages = protocol::history_to_messages(&history);
        let estimate =
            PrepareContextEstimate::from_request(Some(5_000), Some(4), &history, &messages, 900);

        assert_eq!(estimate.total_context_tokens, 900);
        assert_eq!(estimate.provider_context_tokens, None);
        assert_eq!(
            estimate.source,
            PrepareContextEstimateSource::FullRequestEstimate
        );
    }

    #[test]
    fn prepare_request_builds_messages_lazily_on_access() {
        let lua = mlua::Lua::new();
        let messages = vec![Message::user(Content::text("hello"))];
        let request = prepare_request_to_lua(
            &lua,
            messages,
            42,
            PrepareContextEstimate::full_request(42, 1),
        )
        .expect("request table");

        assert!(request.raw_get::<mlua::Value>("messages").unwrap().is_nil());
        assert_eq!(request.get::<u32>("estimated_tokens").unwrap(), 42);

        let messages = request.get::<mlua::Table>("messages").unwrap();
        assert_eq!(
            messages
                .get::<mlua::Table>(1)
                .unwrap()
                .get::<String>("role")
                .unwrap(),
            "user"
        );
        assert!(matches!(
            request.raw_get::<mlua::Value>("messages").unwrap(),
            mlua::Value::Table(_)
        ));
    }
}
