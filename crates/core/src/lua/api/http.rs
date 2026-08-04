//! `smelt.http` - asynchronous HTTP get/post. The Rust binding yields the
//! Lua coroutine via `smelt.task.external` and resolves it from a tokio
//! task, so the runtime never blocks on a request. `smelt.http.get` and
//! `smelt.http.post` are defined in `_bootstrap.lua` over the
//! `__start_get` / `__start_post` primitives below; both
//! require a yieldable context.

use mlua::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::http;
use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<LuaShared>,
    cache_root: &std::path::Path,
) -> LuaResult<()> {
    let http = LuaMod::supported(
        lua,
        smelt,
        "http",
        "Asynchronous HTTP get/post. Yields the calling coroutine until the response lands; runtime stays responsive. Errors use the `(value, err_string)` convention.",
        Tier::Host,
    )?;

    {
        let shared = Arc::clone(shared);
        http.private_live_only_fn(
            "__start_get",
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
        http.private_live_only_fn(
            "__start_post",
            &["task_id", "url", "body", "opts"],
            move |_,
                  (task_id, url, body, opts): (
                u64,
                String,
                Option<mlua::LuaString>,
                Option<mlua::Table>,
            )|
                  -> LuaResult<()> {
                let parsed = parse_options(opts.as_ref())?;
                let body_bytes = body.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
                spawn_request(
                    &shared,
                    task_id,
                    RequestKind::Post { url, body_bytes },
                    parsed,
                );
                Ok(())
            },
        )?;
    }
    let cache = http.sub(
        "cache",
        "Runtime-owned HTTP response cache. Plugins can stash bodies under arbitrary keys to dedupe repeat fetches across a session.",
    )?;
    let read_cache_root = cache_root.to_path_buf();
    cache.fn_(
        "read",
        "Look up a cached HTTP response by `key`. Returns the stored string or `nil` if no entry exists.",
        &["key"],
        move |_, key: String| Ok(http::cache::get(&read_cache_root, &key)),
    )?;
    let write_cache_root = cache_root.to_path_buf();
    cache.live_only_fn(
        "write",
        "Store `value` in the HTTP response cache under `key`.",
        &["key", "value"],
        move |_, (key, value): (String, String)| -> LuaResult<()> {
            http::cache::put(&write_cache_root, &key, &value);
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
    let client = Arc::clone(&shared.http_client);
    tokio::spawn(async move {
        let payload = tokio::select! {
            biased;
            _ = cancel.cancelled() => serde_json::json!({ "__cancelled": true }),
            result = run_request(&client, kind, &opts) => match result {
                Ok(resp) => response_to_json(&resp),
                Err(err) => serde_json::json!({ "err": err.to_string() }),
            },
        };
        sink.resolve_json(task_id, payload);
    });
}

async fn run_request(
    client: &http::Client,
    kind: RequestKind,
    opts: &http::Options,
) -> Result<http::Response, http::Error> {
    match kind {
        RequestKind::Get { url } => client.get(&url, opts).await,
        RequestKind::Post { url, body_bytes } => client.post(&url, body_bytes, opts).await,
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
        max_response_bytes: t.get::<Option<usize>>("max_response_bytes")?,
        max_retries: t.get::<Option<usize>>("max_retries")?,
        headers,
    })
}

fn response_to_json(resp: &http::Response) -> serde_json::Value {
    let content_type = resp
        .headers
        .get("content-type")
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("");
    let textual = content_type.starts_with("text/")
        || content_type.ends_with("+json")
        || content_type.ends_with("+xml")
        || matches!(
            content_type,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-www-form-urlencoded"
        );
    let utf8 = std::str::from_utf8(&resp.body).ok();
    let (body, body_encoding) = if let Some(body) =
        utf8.filter(|_| textual || content_type.is_empty())
    {
        (body.to_owned(), None)
    } else {
        let data_url = engine::image::data_url_from_bytes(&resp.body, "application/octet-stream");
        let encoded = data_url
            .split_once(',')
            .map(|(_, encoded)| encoded)
            .unwrap_or_default()
            .to_owned();
        (encoded, Some("base64"))
    };
    serde_json::json!({
        "status": resp.status,
        "final_url": resp.final_url,
        "body": body,
        "body_encoding": body_encoding,
        "headers": resp.headers,
        "truncated": resp.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_response_crosses_json_boundary_as_base64() {
        let response = http::Response {
            status: 200,
            final_url: "https://example.com/image.png".into(),
            headers: HashMap::from([("content-type".into(), "image/png".into())]),
            body: vec![0, 159, 146, 150],
            truncated: false,
        };
        let json = response_to_json(&response);
        assert_eq!(json["body"], "AJ+Slg==");
        assert_eq!(json["body_encoding"], "base64");
    }
}
