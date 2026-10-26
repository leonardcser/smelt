//! Host-tier Lua API bindings (TUI and headless).

#[macro_export]
macro_rules! host_read {
    ($lua:expr, |$host:ident| $body:expr) => {{
        $lua.create_function(|_, ()| {
            Ok($crate::host::try_with_core(|$host| $body).unwrap_or_default())
        })?
    }};
}

pub(crate) mod agent;
mod auth;
mod builtins;
mod cli;
mod clipboard;
mod cmd;
mod defaults;
mod diff;
pub mod document;
mod events;
mod files;
mod frontend;
mod fs;
mod fuzzy;
mod grep;
mod html;
mod http;
mod image;
mod json;
pub mod layout;
mod lifecycle;
mod log;
mod lsp;
mod mcp;
mod messages;
pub mod mode;
mod notebook;
mod os;
mod parse;
mod path;
mod perf;
mod phase;
mod process;
mod provider;
pub mod reasoning;
mod reg;
mod remember;
mod shell;
mod signal;
mod skills;
mod spawn;
mod state;
mod task;
mod time;
mod timer;
mod tools;
mod transcript;
mod transcript_groups;
mod trust;

use mlua::prelude::*;
use std::path::Path;
use std::sync::Arc;

/// Register all Host-tier namespaces on the `smelt` table.
pub fn register_host_api(
    lua: &Lua,
    smelt: &mlua::Table,
    _smelt_keymap: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
    state_root: &Path,
    cache_root: &Path,
) -> LuaResult<()> {
    crate::lua::reg::register_class_doc();
    agent::register(lua, smelt)?;
    auth::register(lua, smelt, shared)?;
    builtins::register(lua, smelt, shared)?;
    time::register(lua, smelt)?;
    cli::register(lua, smelt, shared)?;
    clipboard::register(lua, smelt)?;
    cmd::register(lua, smelt, shared)?;
    defaults::register(lua, smelt, shared)?;
    document::register(lua, smelt)?;
    diff::register(lua, smelt, shared)?;
    events::register(lua, smelt, shared)?;
    files::register(lua, smelt, shared)?;
    frontend::register(lua, smelt)?;
    fs::register(lua, smelt, shared)?;
    fuzzy::register(lua, smelt)?;
    grep::register(lua, smelt, shared)?;
    html::register(lua, smelt)?;
    http::register(lua, smelt, shared, cache_root)?;
    image::register(lua, smelt, shared)?;
    json::register(lua, smelt)?;
    layout::register(lua, smelt, shared)?;
    lifecycle::register(lua, smelt, shared)?;
    log::register(lua, smelt, shared)?;
    lsp::register(lua, smelt, shared)?;
    mcp::register(lua, smelt, shared)?;
    messages::register(lua, smelt, shared)?;
    notebook::register(lua, smelt, shared)?;
    mode::register(lua, smelt)?;
    os::register(lua, smelt, shared)?;
    reasoning::register(lua, smelt)?;
    reg::register(lua, smelt)?;
    remember::register(lua, smelt, shared)?;
    parse::register(lua, smelt)?;
    path::register(lua, smelt, shared)?;
    perf::register(lua, smelt)?;
    phase::register(lua, smelt, shared)?;
    process::register(lua, smelt, shared)?;
    provider::register(lua, smelt, shared)?;
    shell::register(lua, smelt)?;
    signal::register(lua, smelt, shared)?;
    skills::register(lua, smelt, shared)?;
    spawn::register(lua, smelt, shared)?;
    state::register(lua, smelt, state_root)?;
    task::register(lua, smelt, shared)?;
    timer::register(lua, smelt, shared)?;
    tools::register(lua, smelt, shared)?;
    transcript::register(lua, smelt, shared)?;
    trust::register(lua, smelt, shared, state_root)?;
    Ok(())
}
