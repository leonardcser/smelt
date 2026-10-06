//! Host-safe construction and callback dispatch for auxiliary model requests.

use mlua::{Function, Lua, Value};
use protocol::{EngineEvent, ModelCatalogMetadata, ModelTarget, ReasoningEffort, UiCommand};

use super::{LuaHandle, LuaShared};
use crate::{ActiveModel, RuntimeState};

/// Frontend-owned inputs inherited by an auxiliary request.
pub struct AskContext {
    pub system: String,
    pub tools: Vec<protocol::ToolDef>,
    pub reasoning_effort: ReasoningEffort,
    pub fast_mode: bool,
    pub session_id: String,
    pub persistence: protocol::PersistenceScope,
}

#[derive(Default)]
pub struct AskRequest {
    pub id: u64,
    pub messages: Vec<protocol::Message>,
    pub model: Option<String>,
    pub question: Option<String>,
    pub response_format: Option<protocol::AskResponseFormat>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub stream: bool,
    pub visible_retries: bool,
}

pub fn select_model(config: &RuntimeState, reference: Option<&str>) -> Result<ActiveModel, String> {
    match reference {
        Some(reference) => crate::config::resolve_model_ref(&config.available_models, reference)
            .map(ActiveModel::from_resolved)
            .map_err(|error| error.to_string()),
        None => config
            .active_model()
            .cloned()
            .ok_or_else(|| "no usable model is available".into()),
    }
}

pub fn prepare_request(
    config: &RuntimeState,
    context: AskContext,
    mut request: AskRequest,
    target: ModelTarget,
    catalog: &ModelCatalogMetadata,
) -> Result<UiCommand, String> {
    let reasoning_effort = match request.reasoning_effort {
        Some(effort) if catalog.supports_reasoning_effort(&effort) => effort,
        Some(effort) => {
            return Err(format!(
                "reasoning effort '{}' is not supported by model '{}'",
                effort.label(),
                target.model
            ))
        }
        None => catalog.reconcile_reasoning_effort(context.reasoning_effort),
    };
    if let Some(question) = request.question {
        request
            .messages
            .push(protocol::Message::user(protocol::Content::text(question)));
    }
    Ok(UiCommand::EngineAsk {
        id: request.id,
        system: context.system,
        messages: request.messages,
        target: Box::new(target),
        request_config: config.request_runtime_config(),
        tools: context.tools,
        reasoning_effort,
        fast_mode: context.fast_mode,
        session_id: context.session_id,
        persistence: context.persistence,
        response_format: request.response_format,
        stream: request.stream,
        visible_retries: request.visible_retries,
    })
}

/// Register callbacks in the runtime's single auxiliary-request ID namespace.
pub fn register_callbacks(
    shared: &LuaShared,
    lua: &Lua,
    response: Option<Function>,
    delta: Option<Function>,
    rejected: Option<Function>,
) -> mlua::Result<(u64, bool)> {
    let id = shared
        .next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stream = delta.is_some();
    let callbacks = super::AskCallbacks {
        response: response.map(|f| LuaHandle::from_func(lua, f)).transpose()?,
        delta: delta.map(|f| LuaHandle::from_func(lua, f)).transpose()?,
        rejected: rejected.map(|f| LuaHandle::from_func(lua, f)).transpose()?,
    };
    if callbacks.response.is_some() || callbacks.delta.is_some() || callbacks.rejected.is_some() {
        shared
            .ask_callbacks
            .lock()
            .map_err(|_| mlua::Error::external("ask callback registry is unavailable"))?
            .insert(id, callbacks);
    }
    Ok((id, stream))
}

pub fn remove_callbacks(shared: &LuaShared, id: u64) {
    if let Ok(mut callbacks) = shared.ask_callbacks.lock() {
        callbacks.remove(&id);
    }
}

/// Returns whether the event belongs to a registered auxiliary request.
/// Registry locks are released before invoking Lua, which may register another ask.
pub fn dispatch_callbacks(
    shared: &LuaShared,
    lua: &Lua,
    event: &EngineEvent,
) -> mlua::Result<bool> {
    match event {
        EngineEvent::EngineAskResponse { id, message, error } => {
            let callbacks = shared
                .ask_callbacks
                .lock()
                .map_err(|_| mlua::Error::external("ask callback registry is unavailable"))?
                .remove(id);
            let Some(callbacks) = callbacks else {
                return Ok(false);
            };
            if let Some(handle) = callbacks.response {
                let callback: Function = lua.registry_value(&handle.key)?;
                let message = message
                    .as_ref()
                    .map(|message| super::serde_to_lua_preserving_nulls(lua, message))
                    .transpose()?
                    .unwrap_or(Value::Nil);
                let error = error
                    .as_ref()
                    .map(|error| super::serde_to_lua_preserving_nulls(lua, error))
                    .transpose()?
                    .unwrap_or(Value::Nil);
                let _perf = smelt_perf::perf::begin("lua:ask_cb");
                callback.call::<()>((message, error))?;
            }
            Ok(true)
        }
        EngineEvent::EngineAskDelta { id, .. } | EngineEvent::EngineAskDraftRejected { id } => {
            let callback: Option<Function> = {
                let callbacks = shared
                    .ask_callbacks
                    .lock()
                    .map_err(|_| mlua::Error::external("ask callback registry is unavailable"))?;
                let Some(callbacks) = callbacks.get(id) else {
                    return Ok(false);
                };
                let handle = match event {
                    EngineEvent::EngineAskDelta { .. } => &callbacks.delta,
                    _ => &callbacks.rejected,
                };
                handle
                    .as_ref()
                    .map(|handle| lua.registry_value(&handle.key))
                    .transpose()?
            };
            if let Some(callback) = callback {
                match event {
                    EngineEvent::EngineAskDelta { delta, .. } => {
                        let _perf = smelt_perf::perf::begin("lua:ask_delta_cb");
                        callback.call::<()>(delta.clone())?
                    }
                    _ => {
                        let _perf = smelt_perf::perf::begin("lua:ask_rejected_cb");
                        callback.call::<()>(())?
                    }
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    #[test]
    fn rejection_is_scoped_and_retains_callbacks_until_response() {
        let lua = Lua::new();
        let shared = LuaShared::default();
        lua.load("draft = ''; resets = 0; responses = 0")
            .exec()
            .unwrap();
        let delta = lua
            .load("return function(d) draft = draft .. d end")
            .eval()
            .unwrap();
        let rejected = lua
            .load("return function() draft = ''; resets = resets + 1 end")
            .eval()
            .unwrap();
        let response = lua.load("return function(message, err) assert(message == nil and err == nil); responses = responses + 1 end").eval().unwrap();
        let (id, stream) =
            register_callbacks(&shared, &lua, Some(response), Some(delta), Some(rejected)).unwrap();
        assert!(stream);
        assert!(!dispatch_callbacks(
            &shared,
            &lua,
            &EngineEvent::EngineAskDraftRejected { id: id + 1 }
        )
        .unwrap());
        for event in [
            EngineEvent::EngineAskDelta {
                id,
                delta: "rejected".into(),
            },
            EngineEvent::EngineAskDraftRejected { id },
            EngineEvent::EngineAskDelta {
                id,
                delta: "accepted".into(),
            },
        ] {
            assert!(dispatch_callbacks(&shared, &lua, &event).unwrap());
        }
        assert_eq!(lua.globals().get::<String>("draft").unwrap(), "accepted");
        assert_eq!(lua.globals().get::<u64>("resets").unwrap(), 1);
        let response = EngineEvent::EngineAskResponse {
            id,
            message: None,
            error: None,
        };
        assert!(dispatch_callbacks(&shared, &lua, &response).unwrap());
        assert!(!dispatch_callbacks(&shared, &lua, &response).unwrap());
        assert_eq!(lua.globals().get::<u64>("responses").unwrap(), 1);
        assert!(shared.ask_callbacks.lock().unwrap().is_empty());
    }

    #[test]
    fn callbacks_can_register_another_request_without_registry_lock() {
        let lua = Lua::new();
        let shared = Rc::new(LuaShared::default());
        let registry = Rc::clone(&shared);
        let callback = lua
            .create_function(move |lua, ()| {
                register_callbacks(
                    &registry,
                    lua,
                    None,
                    None,
                    Some(lua.create_function(|_, ()| Ok(()))?),
                )?;
                Ok(())
            })
            .unwrap();
        let (id, _) =
            register_callbacks(&shared, &lua, Some(callback.clone()), None, Some(callback))
                .unwrap();
        assert!(
            dispatch_callbacks(&shared, &lua, &EngineEvent::EngineAskDraftRejected { id }).unwrap()
        );
        assert!(dispatch_callbacks(
            &shared,
            &lua,
            &EngineEvent::EngineAskResponse {
                id,
                message: None,
                error: None
            }
        )
        .unwrap());
        assert_eq!(shared.ask_callbacks.lock().unwrap().len(), 2);
    }

    #[test]
    fn cleanup_discards_callbacks_and_response_errors_do_not_leak() {
        let lua = Lua::new();
        let shared = LuaShared::default();
        let callback: Function = lua
            .load("return function() error('callback failed') end")
            .eval()
            .unwrap();
        let (id, _) =
            register_callbacks(&shared, &lua, Some(callback.clone()), None, None).unwrap();
        remove_callbacks(&shared, id);
        assert!(!dispatch_callbacks(
            &shared,
            &lua,
            &EngineEvent::EngineAskResponse {
                id,
                message: None,
                error: None
            }
        )
        .unwrap());
        let (id, _) = register_callbacks(&shared, &lua, Some(callback), None, None).unwrap();
        assert!(dispatch_callbacks(
            &shared,
            &lua,
            &EngineEvent::EngineAskResponse {
                id,
                message: None,
                error: None
            }
        )
        .is_err());
        assert!(shared.ask_callbacks.lock().unwrap().is_empty());
    }
}
