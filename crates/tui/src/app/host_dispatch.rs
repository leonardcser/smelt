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
use mlua::prelude::*;
use protocol::Message;

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
                let mutated = self.run_provider_request_hooks(messages);
                let _ = reply.send(mutated);
            }
            HostCall::ProviderResponse { message, reply } => {
                let mutated = self.run_provider_response_hooks(message);
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

    fn run_provider_request_hooks(&self, messages: Vec<Message>) -> Option<Vec<Message>> {
        let lua = self.lua.lua();
        let funcs = self
            .lua
            .core_shared()
            .hooks
            .provider_request
            .snapshot_for(lua, "");
        if funcs.is_empty() {
            return None;
        }

        let payload = match messages_to_lua(lua, &messages) {
            Ok(t) => t,
            Err(_) => return None,
        };
        let mut current = payload;
        let mut mutated = false;
        for func in funcs {
            match func.call::<mlua::Value>(current.clone()) {
                Ok(mlua::Value::Table(t)) => {
                    current = t;
                    mutated = true;
                }
                Ok(_) => {}
                Err(e) => {
                    self.lua
                        .record_error(format!("provider.middleware on_request: {e}"));
                }
            }
        }
        if !mutated {
            return None;
        }
        lua_to_messages(lua, &current)
    }

    fn run_provider_response_hooks(&self, message: Message) -> Option<Message> {
        let lua = self.lua.lua();
        let funcs = self
            .lua
            .core_shared()
            .hooks
            .provider_response
            .snapshot_for(lua, "");
        if funcs.is_empty() {
            return None;
        }
        let table = match message_to_lua(lua, &message) {
            Ok(t) => t,
            Err(_) => return None,
        };
        let mut current = table;
        let mut mutated = false;
        for func in funcs {
            match func.call::<mlua::Value>(current.clone()) {
                Ok(mlua::Value::Table(t)) => {
                    current = t;
                    mutated = true;
                }
                Ok(_) => {}
                Err(e) => {
                    self.lua
                        .record_error(format!("provider.middleware on_response: {e}"));
                }
            }
        }
        if !mutated {
            return None;
        }
        lua_to_message(lua, &current)
    }
}

fn message_to_lua(lua: &Lua, msg: &Message) -> LuaResult<mlua::Table> {
    let value = serde_json::to_value(msg).map_err(mlua::Error::external)?;
    match smelt_core::lua::json_to_lua(lua, &value)? {
        mlua::Value::Table(t) => Ok(t),
        _ => Err(mlua::Error::RuntimeError(
            "Message did not serialize to a table".into(),
        )),
    }
}

fn messages_to_lua(lua: &Lua, msgs: &[Message]) -> LuaResult<mlua::Table> {
    let value = serde_json::to_value(msgs).map_err(mlua::Error::external)?;
    match smelt_core::lua::json_to_lua(lua, &value)? {
        mlua::Value::Table(t) => Ok(t),
        _ => Err(mlua::Error::RuntimeError(
            "Vec<Message> did not serialize to a table".into(),
        )),
    }
}

fn lua_to_message(lua: &Lua, table: &mlua::Table) -> Option<Message> {
    let value = smelt_core::lua::api::lua_table_to_json(lua, table);
    serde_json::from_value(value).ok()
}

fn lua_to_messages(lua: &Lua, table: &mlua::Table) -> Option<Vec<Message>> {
    let value = smelt_core::lua::api::lua_table_to_json(lua, table);
    serde_json::from_value(value).ok()
}
