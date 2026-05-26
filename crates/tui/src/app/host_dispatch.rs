//! Drain `engine::HostCall` requests and run the matching Lua hooks
//! on the TUI main thread. Each `HostCall` carries its own
//! `oneshot::Sender` for the reply; we send back at most once.
//!
//! Phase A handles `ProviderRequest` / `ProviderResponse`. The
//! `DispatchTool` / `EvalHooks` / `AskPermission` variants are
//! reserved for Phase B, which migrates today's
//! `EngineEvent::*Request` + `UiCommand::*Response` pairs onto this
//! channel. The engine never emits them yet, so the match arms here
//! exist only to keep the dispatcher exhaustive against future
//! additions to `HostCall`.

use crate::app::TuiApp;
use engine::HostCall;
use protocol::Message;
use smelt_core::lua::{HookRegistry, LuaShared};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

type MessageReply = oneshot::Sender<Option<Vec<Message>>>;
type MessageReplySlot = Arc<Mutex<Option<MessageReply>>>;

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
            HostCall::ProviderRequest { messages, reply } => {
                let mutated =
                    self.run_middleware_chain::<Vec<Message>>(messages, "on_request", |s| {
                        &s.hooks.provider_request
                    });
                let _ = reply.send(mutated);
            }
            HostCall::ProviderResponse { message, reply } => {
                let mutated = self.run_middleware_chain::<Message>(message, "on_response", |s| {
                    &s.hooks.provider_response
                });
                let _ = reply.send(mutated);
            }
            HostCall::RecoverFromContextLimit { messages, reply } => {
                self.dispatch_recover_from_context_limit(messages, reply);
            }
            HostCall::PrepareRequest {
                messages,
                estimated_tokens,
                reply,
            } => {
                self.dispatch_prepare_request(messages, estimated_tokens, reply);
            }
            // Phase B targets. The engine doesn't emit these on the host
            // channel yet — they still travel as `EngineEvent::*Request`
            // pairs handled in `engine_events.rs`. Once migrated, the
            // existing match arms there get deleted and the logic lands
            // here, sharing the same dispatcher loop.
            HostCall::DispatchTool { reply, .. } => drop(reply),
            HostCall::EvalHooks { reply, .. } => drop(reply),
            HostCall::AskPermission { reply, .. } => drop(reply),
        }
    }

    /// Hand the first registered `smelt.engine.on_context_limit` hook the
    /// truncated history along with a Lua `reply` function whose body
    /// holds the engine's `oneshot::Sender`. The hook MUST call `reply`
    /// exactly once with either a shorter messages array (engine swaps
    /// and retries) or `nil` (engine aborts the turn). If the hook
    /// vanishes without calling `reply`, the wrapping closure is GC'd,
    /// the Sender drops, and the engine's `.await` resolves to `None`.
    fn dispatch_recover_from_context_limit(&self, messages: Vec<Message>, reply: MessageReply) {
        self.call_message_reply_hook(
            "on_context_limit",
            |s| &s.hooks.context_limit,
            messages,
            reply,
            |_, func, messages_table, reply_fn| func.call::<()>((messages_table, reply_fn)),
        );
    }

    /// Hand the first registered `smelt.engine.on_prepare_request`
    /// hook the exact non-system message slice the engine is about to
    /// send, plus a conservative token estimate for that slice. The
    /// hook can return a replacement via `reply(messages)`;
    /// `nil` means "send the original request".
    fn dispatch_prepare_request(
        &mut self,
        messages: Vec<Message>,
        estimated_tokens: u32,
        reply: MessageReply,
    ) {
        while let Ok(ev) = self.core.engine.try_recv() {
            self.dispatch_engine_event(ev);
        }
        let context_estimate = PrepareContextEstimate::from_request(
            self.core.session.context_tokens,
            self.core.session.context_tokens_history_len,
            &self.core.session.history,
            &messages,
            estimated_tokens,
        );
        self.call_message_reply_hook(
            "on_prepare_request",
            |s| &s.hooks.prepare_request,
            messages,
            reply,
            |lua, func, messages_table, reply_fn| {
                let request = lua.create_table()?;
                request.set("messages", messages_table)?;
                request.set("estimated_tokens", estimated_tokens)?;
                request.set(
                    "estimated_context_tokens",
                    context_estimate.total_context_tokens,
                )?;
                request.set("context_estimate", context_estimate.into_lua_table(lua)?)?;
                func.call::<()>((request, reply_fn))
            },
        );
    }

    fn call_message_reply_hook(
        &self,
        label: &'static str,
        registry: impl Fn(&LuaShared) -> &Arc<HookRegistry>,
        messages: Vec<Message>,
        reply: MessageReply,
        call: impl FnOnce(&mlua::Lua, mlua::Function, mlua::Value, mlua::Function) -> mlua::Result<()>,
    ) {
        let lua = self.lua.lua();
        let funcs = registry(self.lua.core_shared()).snapshot_for(lua, "");
        let Some(func) = funcs.into_iter().next() else {
            let _ = reply.send(None);
            return;
        };
        let messages_table = match smelt_core::lua::serde_to_lua(lua, &messages) {
            Ok(v) => v,
            Err(e) => {
                self.lua
                    .record_error(format!("{label}: serialize messages: {e}"));
                let _ = reply.send(None);
                return;
            }
        };
        let reply_slot: MessageReplySlot = Arc::new(Mutex::new(Some(reply)));
        let reply_for_closure = Arc::clone(&reply_slot);
        let reply_fn = match lua.create_function(move |inner_lua, value: mlua::Value| {
            let Some(tx) = reply_for_closure.lock().ok().and_then(|mut g| g.take()) else {
                // Already replied; subsequent calls are silent no-ops so
                // hook authors aren't punished for defensive double-calls.
                return Ok(());
            };
            let shorter: Option<Vec<Message>> = match value {
                mlua::Value::Nil => None,
                v => smelt_core::lua::lua_to_serde::<Vec<Message>>(inner_lua, &v),
            };
            let _ = tx.send(shorter);
            Ok(())
        }) {
            Ok(f) => f,
            Err(e) => {
                self.lua.record_error(format!("{label}: build reply: {e}"));
                if let Some(tx) = reply_slot.lock().ok().and_then(|mut g| g.take()) {
                    let _ = tx.send(None);
                }
                return;
            }
        };
        if let Err(e) = call(lua, func, messages_table, reply_fn) {
            self.lua.record_error(format!("{label}: {e}"));
            if let Some(tx) = reply_slot.lock().ok().and_then(|mut g| g.take()) {
                let _ = tx.send(None);
            }
        }
    }

    /// Snapshot a `HookRegistry`, serialize `payload` into Lua via serde,
    /// call each hook in registration order (passing the previous hook's
    /// returned table as input), and deserialize the final value back
    /// into `T`. Returns `None` when no hook is registered or no hook
    /// returned a replacement table — caller treats `None` as "no
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

#[cfg(test)]
mod compaction_tests {
    use super::*;
    use crate::app::test_harness::TestAppBuilder;
    use protocol::{Content, EngineAskError, EngineAskErrorKind, EngineEvent, Message, UiCommand};
    use tokio::sync::oneshot;

    fn user(text: &str) -> Message {
        Message::user(Content::text(text))
    }

    fn assistant(text: &str) -> Message {
        Message::assistant(Some(Content::text(text)), None, None)
    }

    fn ask_messages(cmds: Vec<UiCommand>) -> Vec<(String, Vec<Message>)> {
        cmds.into_iter()
            .filter_map(|cmd| match cmd {
                UiCommand::EngineAsk {
                    system, messages, ..
                } => Some((system, messages)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prepare_request_compaction_preserves_session_prefix_and_appends_summary_instruction() {
        let mut t = TestAppBuilder::default().build();
        t.app.core.config.context_window = Some(100);
        t.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(Content::text("u1")));
        t.push_assistant_text("a1");
        t.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::user(Content::text("u2")));

        let full_history = protocol::history_to_messages(&t.app.model_history());
        let expected_prefix = &full_history[..2];
        let (tx, rx) = oneshot::channel();
        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_host_call(HostCall::PrepareRequest {
                messages: full_history.clone(),
                estimated_tokens: 200,
                reply: tx,
            });
        }

        let asks = ask_messages(t.drain_engine_sends());
        assert_eq!(asks.len(), 1, "compaction should issue one EngineAsk");
        let (system, messages) = &asks[0];
        assert_eq!(system, &t.app.assemble_system_prompt());
        assert_eq!(
            &messages[..expected_prefix.len()],
            expected_prefix,
            "initial compaction attempt must preserve the exact session prefix up to the current boundary"
        );
        let last_text = messages
            .last()
            .and_then(|m| m.content.as_ref())
            .map(|c| c.text_content());
        let last_text = last_text.expect("summary task");
        assert!(last_text.contains("CONTEXT CHECKPOINT COMPACTION"));
        assert!(last_text.contains("Under no circumstances use tools"));
        assert!(last_text.contains("# Goal"));

        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_engine_event(EngineEvent::EngineAskResponse {
                id: t.pending_ask_id().expect("pending ask id"),
                message: Some(Message::assistant(
                    Some(Content::text("# Goal\nok")),
                    None,
                    None,
                )),
                error: None,
            });
        }
        let replacement = rx
            .await
            .expect("prepare_request reply")
            .expect("replacement");
        let replacement_text = replacement
            .first()
            .and_then(|m| m.content.as_ref())
            .map(|c| c.text_content());
        let expected = format!("{}\n# Goal\nok", engine::SUMMARY_PREFIX.trim_end());
        assert_eq!(replacement_text.as_deref(), Some(expected.as_str()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn context_limit_recovery_moves_boundary_earlier_on_context_window() {
        let mut t = TestAppBuilder::default().build();
        let messages = vec![
            user("u1"),
            assistant("a1"),
            user("u2"),
            assistant("a2"),
            user("u3"),
        ];
        let (tx, rx) = oneshot::channel();
        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_host_call(HostCall::RecoverFromContextLimit {
                messages: messages.clone(),
                reply: tx,
            });
        }

        let first = ask_messages(t.drain_engine_sends());
        assert_eq!(first.len(), 1);
        let first_messages = &first[0].1;
        assert_eq!(
            &first_messages[..4],
            &messages[..4],
            "keep_recent_groups=1 should compact everything before the last group"
        );

        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_engine_event(EngineEvent::EngineAskResponse {
                id: t.pending_ask_id().expect("pending ask id"),
                message: None,
                error: Some(EngineAskError {
                    kind: EngineAskErrorKind::ContextWindow,
                    message: "too large".into(),
                }),
            });
        }

        let second = ask_messages(t.drain_engine_sends());
        assert_eq!(second.len(), 1);
        let second_messages = &second[0].1;
        assert_eq!(
            &second_messages[..3],
            &messages[..3],
            "retry should move the boundary one group earlier"
        );

        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_engine_event(EngineEvent::EngineAskResponse {
                id: t.pending_ask_id().expect("pending ask id"),
                message: Some(Message::assistant(
                    Some(Content::text("# Goal\nok")),
                    None,
                    None,
                )),
                error: None,
            });
        }

        let replacement = rx.await.expect("recovery reply").expect("replacement");
        assert_eq!(replacement.len(), 3);
        assert_eq!(replacement[1], messages[3]);
        assert_eq!(replacement[2], messages[4]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn context_limit_recovery_denies_tool_calls_without_moving_boundary() {
        let mut t = TestAppBuilder::default().build();
        let messages = vec![user("u1"), assistant("a1"), user("u2")];
        let (tx, rx) = oneshot::channel();
        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_host_call(HostCall::RecoverFromContextLimit {
                messages: messages.clone(),
                reply: tx,
            });
        }

        let first = ask_messages(t.drain_engine_sends());
        assert_eq!(first.len(), 1);
        let first_messages = first[0].1.clone();

        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_engine_event(EngineEvent::EngineAskResponse {
                id: t.pending_ask_id().expect("pending ask id"),
                message: Some(Message::assistant(
                    None,
                    None,
                    Some(vec![protocol::ToolCall::new(
                        "call-1".into(),
                        protocol::FunctionCall {
                            name: "read_file".into(),
                            arguments: "{}".into(),
                        },
                    )]),
                )),
                error: None,
            });
        }

        let second = ask_messages(t.drain_engine_sends());
        assert_eq!(second.len(), 1);
        let second_messages = &second[0].1;
        assert_eq!(
            &second_messages[..first_messages.len()],
            first_messages.as_slice(),
            "tool denial retry must keep the same boundary prefix"
        );
        assert_eq!(
            second_messages[first_messages.len()].role,
            protocol::Role::Assistant
        );
        assert_eq!(
            second_messages[first_messages.len() + 1].role,
            protocol::Role::Tool
        );
        assert!(second_messages[first_messages.len() + 1].is_error);

        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_engine_event(EngineEvent::EngineAskResponse {
                id: t.pending_ask_id().expect("pending ask id"),
                message: Some(Message::assistant(
                    Some(Content::text("# Goal\nok")),
                    None,
                    None,
                )),
                error: None,
            });
        }

        let replacement = rx.await.expect("recovery reply").expect("replacement");
        assert_eq!(replacement.first().unwrap().role, protocol::Role::User);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn context_limit_recovery_returns_none_when_no_earlier_boundary_fits() {
        let mut t = TestAppBuilder::default().build();
        let messages = vec![user("u1"), user("u2")];
        let (tx, rx) = oneshot::channel();
        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_host_call(HostCall::RecoverFromContextLimit {
                messages,
                reply: tx,
            });
        }

        {
            let _guard = crate::lua::install_app_ptr(&mut t.app);
            t.app.dispatch_engine_event(EngineEvent::EngineAskResponse {
                id: t.pending_ask_id().expect("pending ask id"),
                message: None,
                error: Some(EngineAskError {
                    kind: EngineAskErrorKind::ContextWindow,
                    message: "too large".into(),
                }),
            });
        }

        assert!(rx.await.expect("recovery reply").is_none());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrepareContextEstimateSource {
    FullRequestEstimate,
    ProviderSnapshot,
    ProviderSnapshotPlusHistoryDelta,
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
    fn from_request(
        current_context_tokens: Option<u32>,
        context_tokens_history_len: Option<usize>,
        current_history: &[protocol::HistoryItem],
        _request_messages: &[Message],
        full_request_estimate: u32,
    ) -> Self {
        let Some(base) = current_context_tokens else {
            return Self::full_request(full_request_estimate, current_history.len());
        };
        let base_history_len = context_tokens_history_len.unwrap_or(current_history.len());

        if base_history_len == current_history.len() {
            // Baseline exactly covers current history.
            return Self {
                total_context_tokens: base,
                provider_context_tokens: current_context_tokens,
                estimated_delta_tokens: 0,
                latest_snapshot_history_len: context_tokens_history_len,
                current_history_len: current_history.len(),
                source: PrepareContextEstimateSource::ProviderSnapshot,
            };
        }

        if base_history_len < current_history.len() {
            // Baseline is stale; compute a delta for appended messages.
            let added_messages =
                protocol::history_to_messages(&current_history[base_history_len..]);
            let estimated_delta_tokens =
                smelt_core::session::estimate_message_tokens(&added_messages);
            return Self {
                total_context_tokens: base.saturating_add(estimated_delta_tokens),
                provider_context_tokens: current_context_tokens,
                estimated_delta_tokens,
                latest_snapshot_history_len: context_tokens_history_len,
                current_history_len: current_history.len(),
                source: PrepareContextEstimateSource::ProviderSnapshotPlusHistoryDelta,
            };
        }

        // Baseline history len is somehow ahead of current history (shouldn't
        // happen in practice, but treat it as an exact match to be safe).
        Self {
            total_context_tokens: base,
            provider_context_tokens: current_context_tokens,
            estimated_delta_tokens: 0,
            latest_snapshot_history_len: context_tokens_history_len,
            current_history_len: current_history.len(),
            source: PrepareContextEstimateSource::ProviderSnapshot,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{AssistantTurn, Content, HistoryItem, ToolInvocation, ToolOutcome};

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
            HistoryItem::assistant(AssistantTurn::terminal(
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
    fn prepare_context_estimate_stays_at_baseline_when_snapshot_covers_history() {
        let history = vec![
            HistoryItem::user(Content::text("old")),
            HistoryItem::assistant(AssistantTurn::terminal(
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
            HistoryItem::assistant(AssistantTurn::with_invocations(
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
            HistoryItem::assistant(AssistantTurn::terminal(
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
            HistoryItem::assistant(AssistantTurn::with_invocations(
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
            HistoryItem::assistant(AssistantTurn::with_invocations(
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
        // No context_tokens_history_len — defaults to current_history.len(),
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
            HistoryItem::assistant(AssistantTurn::terminal(
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
}
