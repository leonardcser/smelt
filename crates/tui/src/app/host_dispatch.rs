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
use protocol::{Message, Role};
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
        let estimated_context_tokens = estimate_prepare_context_tokens(
            self.core.session.context_tokens,
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
                request.set("estimated_context_tokens", estimated_context_tokens)?;
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

fn estimate_prepare_context_tokens(
    current_context_tokens: Option<u32>,
    messages: &[Message],
    full_request_estimate: u32,
) -> u32 {
    let Some(base) = current_context_tokens else {
        return full_request_estimate;
    };
    let Some(last_assistant_idx) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
        return full_request_estimate;
    };
    let added = &messages[last_assistant_idx.saturating_add(1)..];
    if added.is_empty() {
        return base;
    }
    let added_bytes = serde_json::to_vec(added)
        .map(|v| v.len())
        .unwrap_or_default();
    let added_tokens = added_bytes.div_ceil(4).min(u32::MAX as usize) as u32;
    base.saturating_add(added_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Content;

    #[test]
    fn prepare_context_estimate_uses_full_estimate_without_provider_baseline() {
        let messages = vec![Message::user(Content::text("hello"))];
        assert_eq!(estimate_prepare_context_tokens(None, &messages, 123), 123);
    }

    #[test]
    fn prepare_context_estimate_adds_only_messages_after_last_assistant() {
        let messages = vec![
            Message::user(Content::text("old")),
            Message::assistant(Some(Content::text("reply")), None, None),
            Message::user(Content::text("new prompt")),
        ];

        let estimated = estimate_prepare_context_tokens(Some(100), &messages, 10_000);

        assert!(estimated > 100);
        assert!(estimated < 10_000);
    }

    #[test]
    fn prepare_context_estimate_stays_at_baseline_without_new_messages() {
        let messages = vec![
            Message::user(Content::text("old")),
            Message::assistant(Some(Content::text("reply")), None, None),
        ];

        assert_eq!(
            estimate_prepare_context_tokens(Some(100), &messages, 10_000),
            100
        );
    }
}
