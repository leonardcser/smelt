//! `smelt.auth` - authenticated provider helpers that keep credentials inside Rust.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let auth = LuaMod::supported(
        lua,
        smelt,
        "auth",
        "Authenticated provider helpers. Requests use smelt-managed credentials without exposing bearer tokens to Lua.",
        Tier::Host,
    )?;

    auth.fn_(
        "usage_scope",
        "Return an opaque login scope for account-usage caches, or nil when managed credentials are unavailable (including externally authenticated providers). Never contains credentials.",
        &["provider"],
        |_, provider: String| {
            if provider == "claude-code" { return Ok(None); }
            let provider = parse_provider(&provider)?;
            Ok(engine::auth::usage_scope(provider))
        },
    )?;

    {
        let shared = Arc::clone(shared);
        auth.private_live_only_fn(
            "__start_request",
            &["task_id", "provider", "opts"],
            move |_, (task_id, provider, opts): (u64, String, mlua::Table)| -> LuaResult<()> {
                let req = parse_request(provider, opts)?;
                spawn_authenticated_request(&shared, task_id, req);
                Ok(())
            },
        )?;
    }

    {
        let shared = Arc::clone(shared);
        auth.private_live_only_fn(
            "__start_managed_usage",
            &["task_id", "provider"],
            move |_, (task_id, provider): (u64, String)| -> LuaResult<()> {
                if provider == "claude-code" {
                    spawn_auth_result(&shared, task_id, engine::claude_code_usage());
                } else {
                    let provider = parse_provider(&provider)?;
                    spawn_auth_result(&shared, task_id, async move {
                        let client = smelt_provider::http_client();
                        engine::auth::managed_usage(provider, &client).await
                    });
                }
                Ok(())
            },
        )?;
    }

    {
        let shared = Arc::clone(shared);
        auth.private_live_only_fn(
            "__start_redeem_usage_reset",
            &["task_id", "scope", "request_id"],
            move |_, (task_id, scope, request_id): (u64, String, String)| -> LuaResult<()> {
                spawn_auth_result(&shared, task_id, async move {
                    let message = engine::auth::redeem_usage_reset(
                        &scope,
                        &request_id,
                        &smelt_provider::http_client(),
                    )
                    .await?;
                    Ok(serde_json::json!({ "message": message }))
                });
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

fn parse_provider(provider: &str) -> LuaResult<engine::auth::AuthProvider> {
    match provider {
        "codex" => Ok(engine::auth::AuthProvider::Codex),
        "copilot" => Ok(engine::auth::AuthProvider::Copilot),
        "kimi-code" => Ok(engine::auth::AuthProvider::KimiCode),
        other => Err(mlua::Error::external(format!(
            "unsupported authenticated provider: {other}"
        ))),
    }
}

fn parse_request(provider: String, opts: mlua::Table) -> LuaResult<AuthRequest> {
    let provider = parse_provider(&provider)?;
    let method = opts
        .get::<Option<String>>("method")?
        .unwrap_or_else(|| "GET".to_string());
    let path = opts.get::<String>("path")?;
    let body = opts
        .get::<Option<mlua::LuaString>>("body")?
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
        let client = smelt_provider::http_client();
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

fn spawn_auth_result<T: serde::Serialize + Send + 'static>(
    shared: &Arc<LuaShared>,
    task_id: u64,
    operation: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
) {
    let cancel = crate::lua::current_task_cancel().unwrap_or_default();
    let sink = shared.resume_sink();
    tokio::spawn(async move {
        let payload = tokio::select! {
            biased;
            _ = cancel.cancelled() => serde_json::json!({ "__cancelled": true }),
            result = operation => match result {
                Ok(value) => serde_json::to_value(value).unwrap_or_else(|err| serde_json::json!({ "err": err.to_string() })),
                Err(err) => serde_json::json!({ "err": err }),
            },
        };
        sink.resolve_json(task_id, payload);
    });
}
