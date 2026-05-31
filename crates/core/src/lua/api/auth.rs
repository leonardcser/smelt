//! `smelt.auth` — authenticated provider helpers that keep credentials inside Rust.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let auth = LuaMod::under(
        lua,
        smelt,
        "auth",
        "Authenticated provider helpers. Requests use smelt-managed credentials without exposing bearer tokens to Lua.",
        Tier::Host,
    )?;

    {
        let shared = Arc::clone(shared);
        auth.fn_(
            "__request_async_start",
            "Begin an async authenticated provider request. Resolves `task_id` with `{ status, body }` or `{ err }`. Used internally by `smelt.auth.request`.",
            &["task_id", "provider", "opts"],
            move |_, (task_id, provider, opts): (u64, String, mlua::Table)| -> LuaResult<()> {
                let req = parse_request(provider, opts)?;
                spawn_authenticated_request(&shared, task_id, req);
                Ok(())
            },
        )?;
    }

    Ok(())
}

struct AuthRequest {
    provider: engine::auth::AuthProvider,
    method: String,
    path: String,
    body: Option<Vec<u8>>,
}

fn parse_request(provider: String, opts: mlua::Table) -> LuaResult<AuthRequest> {
    let provider = match provider.as_str() {
        "codex" => engine::auth::AuthProvider::Codex,
        "copilot" => engine::auth::AuthProvider::Copilot,
        other => {
            return Err(mlua::Error::external(format!(
                "unsupported authenticated provider: {other}"
            )))
        }
    };
    let method = opts
        .get::<Option<String>>("method")?
        .unwrap_or_else(|| "GET".to_string());
    let path = opts.get::<String>("path")?;
    let body = opts
        .get::<Option<mlua::String>>("body")?
        .map(|s| s.as_bytes().to_vec());
    Ok(AuthRequest {
        provider,
        method,
        path,
        body,
    })
}

fn spawn_authenticated_request(shared: &Arc<LuaShared>, task_id: u64, req: AuthRequest) {
    let cancel = crate::lua::current_task_cancel().unwrap_or_default();
    let sink = shared.resume_sink();
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let payload = tokio::select! {
            biased;
            _ = cancel.cancelled() => serde_json::json!({ "__cancelled": true }),
            result = engine::auth::authenticated_request(req.provider, &req.method, &req.path, req.body, &client) => match result {
                Ok(resp) => serde_json::json!({ "status": resp.status, "body": resp.body }),
                Err(err) => serde_json::json!({ "err": err }),
            },
        };
        sink.resolve_json(task_id, payload);
    });
}
