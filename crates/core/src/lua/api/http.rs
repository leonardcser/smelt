//! `smelt.http` — asynchronous HTTP get/post. The Rust binding yields the
//! Lua coroutine via `smelt.task.external` and resolves it from a tokio
//! task, so the runtime never blocks on a request. `smelt.http.get` and
//! `smelt.http.post` are defined in `_bootstrap.lua` over the
//! `__get_async_start` / `__post_async_start` primitives below; both
//! require a yieldable context.

use mlua::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::http;
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let http = LuaMod::under(
        lua,
        smelt,
        "http",
        "Asynchronous HTTP get/post. Yields the calling coroutine until the response lands; runtime stays responsive. Errors use the `(value, err_string)` convention.",
        Tier::Host,
    )?;

    {
        let shared = Arc::clone(shared);
        http.fn_(
            "__get_async_start",
            "Begin an async HTTP GET. Resolves `task_id` with `{ status, final_url, body, headers }` on success, `{ __cancelled = true }` if the calling coroutine is cancelled, or `{ err }` on spawn failure. Used internally by `smelt.http.get`.",
            &["task_id", "url", "opts"],
            move |_, (task_id, url, opts): (u64, String, Option<mlua::Table>)| -> LuaResult<()> {
                let parsed = parse_options(opts.as_ref())?;
                spawn_request(&shared, task_id, RequestKind::Get { url }, parsed);
                Ok(())
            },
        )?;
    }
    {
        let shared = Arc::clone(shared);
        http.fn_(
            "__post_async_start",
            "Begin an async HTTP POST with `body` bytes. Resolves `task_id` with `{ status, final_url, body, headers }` on success, `{ __cancelled = true }` if the calling coroutine is cancelled, or `{ err }` on spawn failure. Used internally by `smelt.http.post`.",
            &["task_id", "url", "body", "opts"],
            move |_,
                  (task_id, url, body, opts): (
                u64,
                String,
                Option<mlua::String>,
                Option<mlua::Table>,
            )|
                  -> LuaResult<()> {
                let parsed = parse_options(opts.as_ref())?;
                let body_bytes = body.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
                spawn_request(&shared, task_id, RequestKind::Post { url, body_bytes }, parsed);
                Ok(())
            },
        )?;
    }
    http.fn_(
        "random_user_agent",
        "Return a randomly selected User-Agent string from the built-in pool.",
        &[],
        |_, ()| Ok(http::random_user_agent()),
    )?;

    let cache = http.sub(
        "cache",
        "Process-wide HTTP response cache. Plugins can stash bodies under arbitrary keys to dedupe repeat fetches across a session.",
    )?;
    cache.fn_(
        "read",
        "Look up a cached HTTP response by `key`. Returns the stored string or `nil` if no entry exists.",
        &["key"],
        |_, key: String| Ok(http::cache::get(&key)),
    )?;
    cache.fn_(
        "write",
        "Store `value` in the HTTP response cache under `key`.",
        &["key", "value"],
        |_, (key, value): (String, String)| -> LuaResult<()> {
            http::cache::put(&key, &value);
            Ok(())
        },
    )?;

    Ok(())
}

enum RequestKind {
    Get { url: String },
    Post { url: String, body_bytes: Vec<u8> },
}

fn spawn_request(shared: &Arc<LuaShared>, task_id: u64, kind: RequestKind, opts: http::Options) {
    let cancel = crate::lua::current_task_cancel().unwrap_or_default();
    let sink = shared.resume_sink();
    tokio::spawn(async move {
        let payload = tokio::select! {
            biased;
            _ = cancel.cancelled() => serde_json::json!({ "__cancelled": true }),
            result = run_request(kind, &opts) => match result {
                Ok(resp) => response_to_json(&resp),
                Err(err) => serde_json::json!({ "err": err.to_string() }),
            },
        };
        sink.resolve_json(task_id, payload);
    });
}

async fn run_request(
    kind: RequestKind,
    opts: &http::Options,
) -> Result<http::Response, reqwest::Error> {
    match kind {
        RequestKind::Get { url } => http::get(&url, opts).await,
        RequestKind::Post { url, body_bytes } => http::post(&url, body_bytes, opts).await,
    }
}

fn parse_options(opts: Option<&mlua::Table>) -> LuaResult<http::Options> {
    let Some(t) = opts else {
        return Ok(http::Options::default());
    };

    let mut headers = HashMap::new();
    if let Some(h) = t.get::<Option<mlua::Table>>("headers")? {
        for pair in h.pairs::<String, String>() {
            let (k, v) = pair?;
            headers.insert(k, v);
        }
    }

    Ok(http::Options {
        timeout: t
            .get::<Option<u64>>("timeout_secs")?
            .map(Duration::from_secs),
        max_redirects: t.get::<Option<usize>>("max_redirects")?,
        headers,
    })
}

fn response_to_json(resp: &http::Response) -> serde_json::Value {
    // Body crosses the tokio→Lua boundary as a JSON string. We lossy-decode
    // UTF-8 here; binary payloads would need a side channel. Every current
    // caller (upgrade.lua, web_fetch, web_search) consumes text, so this
    // limitation is documented rather than worked around.
    serde_json::json!({
        "status": resp.status,
        "final_url": resp.final_url,
        "body": String::from_utf8_lossy(&resp.body).into_owned(),
        "headers": resp.headers,
    })
}
