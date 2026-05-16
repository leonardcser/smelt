//! `smelt.session` bindings — current session metadata, turn list,
//! messages snapshot, rewind, list / load / delete persisted sessions.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

fn messages_to_lua(lua: &Lua, msgs: &[protocol::Message]) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    for (i, msg) in msgs.iter().enumerate() {
        let entry = lua.create_table()?;
        let role = match msg.role {
            protocol::Role::System => "system",
            protocol::Role::User => "user",
            protocol::Role::Assistant => "assistant",
            protocol::Role::Tool => "tool",
        };
        entry.set("role", role)?;
        if let Some(ref c) = msg.content {
            entry.set("content", c.text_content())?;
        }
        if let Some(ref tc) = msg.tool_calls {
            let calls = lua.create_table()?;
            for (j, call) in tc.iter().enumerate() {
                let ct = lua.create_table()?;
                ct.set("id", call.id.as_str())?;
                ct.set("name", call.function.name.as_str())?;
                ct.set("arguments", call.function.arguments.as_str())?;
                calls.set(j + 1, ct)?;
            }
            entry.set("tool_calls", calls)?;
        }
        if let Some(ref id) = msg.tool_call_id {
            entry.set("tool_call_id", id.as_str())?;
        }
        if msg.is_error {
            entry.set("is_error", true)?;
        }
        tbl.set(i + 1, entry)?;
    }
    Ok(tbl)
}

#[lua_module(
    name = "smelt.session",
    doc = "Current session metadata, turn list, message snapshots, rewind, and persisted session management. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let session_tbl = lua.create_table()?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "title",
        "Session title (a short summary derived from the first user message), or `nil` until the engine assigns one.",
        &[],
        lua,
        |_, ()| -> LuaResult<Option<String>> {
            Ok(crate::lua::try_with_app(|app| app.core.session.title.clone()).unwrap_or_default())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "cwd",
        "Working directory the session was launched from. Stable across the session.",
        &[],
        lua,
        |_, ()| Ok(crate::lua::try_with_app(|app| app.cwd.clone()).unwrap_or_default()),
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "cost",
        "Cumulative session cost in USD across every model call this session has made.",
        &[],
        lua,
        |_, ()| -> LuaResult<f64> {
            Ok(
                crate::lua::try_with_app(|app| app.core.session.session_cost_usd)
                    .unwrap_or_default(),
            )
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "context_tokens",
        "Most recent prompt-token count reported by the provider, or `nil` if no turn has completed yet.",
        &[],
        lua,
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| app.core.session.context_tokens).unwrap_or_default())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "context_window",
        "Configured context-window size in tokens for the active model. `nil` when the model entry has no declared limit.",
        &[],
        lua,
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| app.core.config.context_window).unwrap_or_default())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "created_at_ms",
        "Unix-epoch timestamp (milliseconds) at which this session was started.",
        &[],
        lua,
        |_, ()| -> LuaResult<u64> {
            Ok(crate::lua::try_with_app(|app| app.core.session.created_at_ms).unwrap_or_default())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "id",
        "Stable session id (matches the on-disk session filename).",
        &[],
        lua,
        |_, ()| Ok(crate::lua::try_with_app(|app| app.core.session.id.clone()).unwrap_or_default()),
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "dir",
        "Absolute path of the on-disk session directory (transcript JSONL, attachments, ledger).",
        &[],
        lua,
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                smelt_core::session::dir_for(&app.core.session)
                    .display()
                    .to_string()
            })
            .unwrap_or_default())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "messages",
        "Snapshot the current session messages as `{ role, content?, tool_calls?, tool_call_id?, is_error? }` rows. Roles are `system`/`user`/`assistant`/`tool`. `opts.roles` (array of role strings) filters by role; `opts.include_tool = false` drops `role = \"tool\"` rows; `opts.since_index` returns rows with 1-based index `>= since_index`; `opts.limit` caps row count from the start of the (filtered) result.",
        &["opts"],
        lua,
        |lua, opts: Option<mlua::Table>| -> LuaResult<mlua::Table> {
            let messages = crate::lua::try_with_app(|app| app.core.session.messages.clone())
                .unwrap_or_default();
            let (roles, include_tool, since_index, limit) = match opts {
                Some(t) => (
                    t.get::<Option<Vec<String>>>("roles")?,
                    t.get::<Option<bool>>("include_tool")?.unwrap_or(true),
                    t.get::<Option<usize>>("since_index")?,
                    t.get::<Option<usize>>("limit")?,
                ),
                None => (None, true, None, None),
            };
            let role_filter: Option<std::collections::HashSet<String>> =
                roles.map(|v| v.into_iter().collect());
            let filtered: Vec<protocol::Message> = messages
                .into_iter()
                .enumerate()
                .filter(|(idx, m)| {
                    if let Some(ref s) = since_index {
                        if idx + 1 < *s {
                            return false;
                        }
                    }
                    if !include_tool && matches!(m.role, protocol::Role::Tool) {
                        return false;
                    }
                    if let Some(ref rf) = role_filter {
                        let r = match m.role {
                            protocol::Role::System => "system",
                            protocol::Role::User => "user",
                            protocol::Role::Assistant => "assistant",
                            protocol::Role::Tool => "tool",
                        };
                        if !rf.contains(r) {
                            return false;
                        }
                    }
                    true
                })
                .map(|(_, m)| m)
                .take(limit.unwrap_or(usize::MAX))
                .collect();
            messages_to_lua(lua, &filtered)
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "turns",
        "Return user turns as `{ block_idx, label }` rows where `label` is the first line of the user message. Used by the rewind dialog.",
        &[],
        lua,
        |lua, ()|  -> LuaResult<mlua::Table>{
            let turns = crate::lua::try_with_app(|app| app.user_turns()).unwrap_or_default();
            let out = lua.create_table()?;
            for (i, (block_idx, text)) in turns.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("block_idx", block_idx)?;
                let label = text.lines().next().unwrap_or("").to_string();
                row.set("label", label)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "rewind_to",
        "Rewind the session to a prior user turn. `block_idx = nil` rewinds to before the first turn; `opts.restore_vim_insert = true` re-enters vim insert mode after the rewind.",
        &["block_idx", "opts"],
        lua,
        |_, (block_idx, opts): (Option<usize>, Option<mlua::Table>)|  -> LuaResult<()>{
            let restore_vim_insert = opts
                .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                .unwrap_or(false);
            crate::lua::with_app(|app| app.rewind_to_block(block_idx, restore_vim_insert));
            Ok(())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "list",
        "List persisted sessions other than the current one. Each row carries `id`, `title`, `subtitle`, `cwd`, `parent_id`, `updated_at_ms`, `created_at_ms`, and `size_bytes` when available.",
        &[],
        lua,
        |lua, ()|  -> LuaResult<mlua::Table>{
            let current_id =
                crate::lua::try_with_core(|core| core.session.id.clone()).unwrap_or_default();
            let sessions = smelt_core::session::list_sessions();
            let out = lua.create_table()?;
            let mut idx = 1;
            for meta in sessions {
                if meta.id == current_id {
                    continue;
                }
                let row = lua.create_table()?;
                row.set("id", meta.id)?;
                row.set("title", meta.title.unwrap_or_default())?;
                row.set("subtitle", meta.first_user_message.unwrap_or_default())?;
                row.set("cwd", meta.cwd.unwrap_or_default())?;
                row.set("parent_id", meta.parent_id.unwrap_or_default())?;
                row.set("updated_at_ms", meta.updated_at_ms)?;
                row.set("created_at_ms", meta.created_at_ms)?;
                if let Some(size) = meta.text_bytes {
                    row.set("size_bytes", size)?;
                }
                out.set(idx, row)?;
                idx += 1;
            }
            Ok(out)
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "load",
        "Switch the UI to the persisted session with `id`. Replays its message log and resets transient state.",
        &["id"],
        lua,
        |_, id: String|  -> LuaResult<()>{
            crate::lua::with_app(|app| app.load_session_by_id(&id));
            Ok(())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "delete",
        "Delete the persisted session with `id`. Refuses to delete the currently active session.",
        &["id"],
        lua,
        |_, id: String| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if id != app.core.session.id {
                    smelt_core::session::delete(&id);
                }
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "fork",
        "Fork the current session: clone its messages into a new session id and switch to it. Useful for branching off an experiment without losing the original timeline.",
        &[],
        lua,
        |_, ()|  -> LuaResult<()>{
            crate::lua::with_app(|app| app.fork_session());
            Ok(())
        },
    )?;
    register_ui_fn(
        &session_tbl,
        "smelt.session",
        "reset",
        "Cancel any in-flight agent and clear the session to a blank slate. Logs an `agent_stop` event with reason `user_cancel_and_clear`.",
        &[],
        lua,
        |_, ()|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({ "reason": "user_cancel_and_clear" }),
                );
                app.reset_session();
                app.agent = None;
            });
            Ok(())
        },
    )?;
    smelt.set("session", session_tbl)?;
    Ok(())
}
