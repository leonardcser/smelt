//! `smelt.http` bindings — synchronous HTTP fetch over `tui::http`.
//! Host-tier (works in tui and headless) — no Ui touch.
//!
//! Errors flow through the `(value, err)` Lua convention.

use mlua::prelude::*;
use std::collections::HashMap;
use std::time::Duration;

use crate::http;

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
    t.set("body", resp.text())?;
    let h = lua.create_table()?;
    for (k, v) in &resp.headers {
        h.set(k.clone(), v.clone())?;
    }
    t.set("headers", h)?;
    Ok(t)
}
