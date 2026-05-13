//! `smelt.http` — synchronous HTTP get/post. Errors use `(value, err_string)` convention.

use mlua::prelude::*;
use std::collections::HashMap;
use std::time::Duration;

use crate::http;
use crate::lua::doc::register_fn;
use lua_doc_derive::lua_module;

#[lua_module(
    name = "smelt.http",
    doc = "Synchronous HTTP get/post with redirect following and header support. Errors use the `(value, err_string)` convention."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let http_tbl = lua.create_table()?;
    register_fn(
        &http_tbl,
        "smelt.http",
        "get",
        "Perform a synchronous HTTP GET against `url`. `opts` accepts `headers`, `timeout_secs`, and `max_redirects`. Returns `({ status, final_url, body, headers }, nil)` on success or `(nil, err_string)` on failure.",
        &["url", "opts"],
        lua,
        |lua, (url, opts): (String, Option<mlua::Table>)| -> LuaResult<(Option<mlua::Table>, Option<String>)> {
            let parsed = parse_options(opts.as_ref())?;
            match http::get(&url, &parsed) {
                Ok(resp) => Ok((Some(response_to_lua(lua, &resp)?), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        },
    )?;
    register_fn(
        &http_tbl,
        "smelt.http",
        "post",
        "Perform a synchronous HTTP POST against `url` with `body` bytes. `opts` accepts `headers`, `timeout_secs`, and `max_redirects`. Returns `({ status, final_url, body, headers }, nil)` on success or `(nil, err_string)` on failure.",
        &["url", "body", "opts"],
        lua,
        |lua, (url, body, opts): (String, Option<mlua::String>, Option<mlua::Table>)| -> LuaResult<(Option<mlua::Table>, Option<String>)> {
            let parsed = parse_options(opts.as_ref())?;
            let body_bytes = body.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
            match http::post(&url, body_bytes, &parsed) {
                Ok(resp) => Ok((Some(response_to_lua(lua, &resp)?), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        },
    )?;
    register_fn(
        &http_tbl,
        "smelt.http",
        "random_user_agent",
        "Return a randomly selected User-Agent string from the built-in pool.",
        &[],
        lua,
        |_, ()| Ok(http::random_user_agent()),
    )?;
    let cache_tbl = lua.create_table()?;
    register_fn(
        &cache_tbl,
        "smelt.http",
        "get",
        "Look up a cached HTTP response by `key`. Returns the stored string or `nil` if no entry exists.",
        &["key"],
        lua,
        |_, key: String| Ok(http::cache::get(&key)),
    )?;
    register_fn(
        &cache_tbl,
        "smelt.http",
        "put",
        "Store `value` in the HTTP response cache under `key`.",
        &["key", "value"],
        lua,
        |_, (key, value): (String, String)| -> LuaResult<()> {
            http::cache::put(&key, &value);
            Ok(())
        },
    )?;
    http_tbl.set("cache", cache_tbl)?;

    smelt.set("http", http_tbl)?;
    Ok(())
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

fn response_to_lua(lua: &Lua, resp: &http::Response) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    t.set("status", resp.status)?;
    t.set("final_url", resp.final_url.clone())?;
    // Lua strings are byte-safe; pass raw bytes so binary responses
    // (e.g. images) survive the boundary intact. Text consumers can
    // still treat the value as a string.
    t.set("body", lua.create_string(&resp.body)?)?;
    let h = lua.create_table()?;
    for (k, v) in &resp.headers {
        h.set(k.clone(), v.clone())?;
    }
    t.set("headers", h)?;
    Ok(t)
}
