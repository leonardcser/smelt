//! `smelt.http` bindings — synchronous HTTP fetch over `app::http`.
//! Host-tier (works in tui and headless) — no Ui touch.
//!
//! Errors flow through the `(value, err)` Lua convention.

use mlua::prelude::*;
use std::collections::HashMap;
use std::time::Duration;

use crate::core::http;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let http_tbl = lua.create_table()?;

    http_tbl.set(
        "get",
        lua.create_function(|lua, (url, opts): (String, Option<mlua::Table>)| {
            let parsed = parse_options(opts.as_ref())?;
            match http::get(&url, &parsed) {
                Ok(resp) => Ok((Some(response_to_lua(lua, &resp)?), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            }
        })?,
    )?;
    http_tbl.set(
        "post",
        lua.create_function(
            |lua, (url, body, opts): (String, Option<mlua::String>, Option<mlua::Table>)| {
                let parsed = parse_options(opts.as_ref())?;
                let body_bytes = body.map(|s| s.as_bytes().to_vec()).unwrap_or_default();
                match http::post(&url, body_bytes, &parsed) {
                    Ok(resp) => Ok((Some(response_to_lua(lua, &resp)?), None)),
                    Err(err) => Ok((None, Some(err.to_string()))),
                }
            },
        )?,
    )?;
    http_tbl.set(
        "random_user_agent",
        lua.create_function(|_, ()| Ok(http::random_user_agent()))?,
    )?;
    let cache_tbl = lua.create_table()?;
    cache_tbl.set(
        "get",
        lua.create_function(|_, key: String| Ok(http::cache::get(&key)))?,
    )?;
    cache_tbl.set(
        "put",
        lua.create_function(|_, (key, value): (String, String)| {
            http::cache::put(&key, &value);
            Ok(())
        })?,
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
