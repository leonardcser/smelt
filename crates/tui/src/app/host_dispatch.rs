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
use std::sync::Arc;

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
                let mutated = self
                    .run_middleware_chain::<Vec<Message>>(messages, "on_request", |s| {
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
