//! `smelt.session` bindings — current session metadata, turn list,
//! messages snapshot, rewind, list / load / delete persisted sessions.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

/// Convert a Lua sequence of `{ role, content?, reasoning_content?, tool_calls?, tool_call_id?, is_error? }`
/// rows into `Vec<protocol::Message>` via serde. Rows that fail to
/// deserialize (unknown role, malformed shape) are silently dropped so a
/// single bad entry doesn't poison the whole replacement list.
fn lua_messages_to_protocol(lua: &Lua, table: &mlua::Table) -> Vec<protocol::Message> {
    let mut out = Vec::new();
    for value in table.clone().sequence_values::<mlua::Value>().flatten() {
        if let Some(msg) = smelt_core::lua::lua_to_serde::<protocol::Message>(lua, &value) {
            out.push(msg);
        }
    }
    out
}

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

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "session",
        "Current session metadata, turn list, message snapshots, rewind, and persisted session management. UiHost-only.",
        Tier::UiHost,
    )?;
    // smelt.session.title() reads; title(t) writes title + derived slug;
    // title(t, s) writes both. Returns the current title on read.
    let title = m.sub(
        "title",
        "Session title. Callable: `title()` reads the current title, `title(t)` writes the title and derives a slug via `smelt.text.slugify`, `title(t, s)` writes both. Writes also update the task label and save the session.",
    )?;
    title.callable(
        |lua,
         (_tbl, t, s): (mlua::Table, Option<String>, Option<String>)|
         -> LuaResult<mlua::Value> {
            match (t, s) {
                (Some(title), maybe_slug) => {
                    crate::lua::with_app(|app| {
                        let slug = maybe_slug.unwrap_or_else(|| engine::provider::slugify(&title));
                        app.core.session.title = Some(title);
                        app.core.session.slug = Some(slug.clone());
                        app.set_task_label(slug);
                        app.save_session();
                    });
                    Ok(mlua::Value::Nil)
                }
                (None, _) => {
                    let cur = crate::lua::try_with_app(|app| app.core.session.title.clone())
                        .unwrap_or_default();
                    match cur {
                        Some(t) => Ok(mlua::Value::String(lua.create_string(&t)?)),
                        None => Ok(mlua::Value::Nil),
                    }
                }
            }
        },
    )?;
    let slug = m.sub(
        "slug",
        "Session slug (read-only). Writing flows through `smelt.session.title(t, s)`.",
    )?;
    slug.callable(|lua, (_tbl,): (mlua::Table,)| -> LuaResult<mlua::Value> {
        let cur = crate::lua::try_with_app(|app| app.core.session.slug.clone()).unwrap_or_default();
        match cur {
            Some(s) => Ok(mlua::Value::String(lua.create_string(&s)?)),
            None => Ok(mlua::Value::Nil),
        }
    })?;
    m.fn_(
        "cwd",
        "Working directory the session was launched from. Stable across the session.",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.cwd.clone()).unwrap_or_default()),
    )?;
    m.fn_(
        "cost",
        "Cumulative session cost in USD across every model call this session has made.",
        &[],
        |_, ()| -> LuaResult<f64> {
            Ok(
                crate::lua::try_with_app(|app| app.core.session.session_cost_usd)
                    .unwrap_or_default(),
            )
        },
    )?;
    m.fn_(
        "context_tokens",
        "Most recent prompt-token count reported by the provider, or `nil` if no turn has completed yet.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| app.core.session.context_tokens).unwrap_or_default())
        },
    )?;
    m.fn_(
        "context_window",
        "Configured context-window size in tokens for the active model. `nil` when the model entry has no declared limit.",
        &[],
        |_, ()| -> LuaResult<Option<u32>> {
            Ok(crate::lua::try_with_app(|app| app.core.config.context_window).unwrap_or_default())
        },
    )?;
    m.fn_(
        "created_at_ms",
        "Unix-epoch timestamp (milliseconds) at which this session was started.",
        &[],
        |_, ()| -> LuaResult<u64> {
            Ok(crate::lua::try_with_app(|app| app.core.session.created_at_ms).unwrap_or_default())
        },
    )?;
    m.fn_(
        "id",
        "Stable session id (matches the on-disk session filename).",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.core.session.id.clone()).unwrap_or_default()),
    )?;
    m.fn_(
        "dir",
        "Absolute path of the on-disk session directory (transcript JSONL, attachments, ledger).",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(crate::lua::try_with_app(|app| {
                smelt_core::session::dir_for(&app.core.session)
                    .display()
                    .to_string()
            })
            .unwrap_or_default())
        },
    )?;
    // smelt.session.messages() reads (optional opts table filters);
    // smelt.session.messages(list) atomically replaces the message list,
    // clears snapshots, restores the screen, and saves the session.
    let msgs = m.sub(
        "messages",
        "Session messages. Callable: `messages()` (or `messages(opts)`) returns `{ role, content?, tool_calls?, tool_call_id?, is_error? }` rows; pass `opts.roles`, `opts.include_tool`, `opts.since_index`, `opts.limit` to filter. `messages(list)` (a sequence of `{ role, content? }` rows) atomically replaces `session.messages`, drops token/cost/turn-meta snapshots, repaints the transcript, and saves the session.",
    )?;
    msgs.callable(
        |lua, (_tbl, arg): (mlua::Table, Option<mlua::Table>)| -> LuaResult<mlua::Value> {
            let Some(arg) = arg else {
                let messages = crate::lua::try_with_app(|app| app.core.session.messages.clone())
                    .unwrap_or_default();
                let out = messages_to_lua(lua, &messages)?;
                return Ok(mlua::Value::Table(out));
            };
            // Distinguish "filter opts table" (has named keys) from
            // "messages list" (sequence of {role,...} entries).
            let len = arg.raw_len();
            let looks_like_list = len > 0
                && arg
                    .raw_get::<mlua::Value>(1)
                    .map(|v| matches!(v, mlua::Value::Table(_)))
                    .unwrap_or(false);
            if looks_like_list {
                let new_msgs = lua_messages_to_protocol(lua, &arg);
                crate::lua::with_app(|app| app.replace_messages(new_msgs));
                return Ok(mlua::Value::Nil);
            }
            // Filter-opts path.
            let roles = arg.get::<Option<Vec<String>>>("roles")?;
            let include_tool = arg.get::<Option<bool>>("include_tool")?.unwrap_or(true);
            let since_index = arg.get::<Option<usize>>("since_index")?;
            let limit = arg.get::<Option<usize>>("limit")?;
            let messages = crate::lua::try_with_app(|app| app.core.session.messages.clone())
                .unwrap_or_default();
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
            let out = messages_to_lua(lua, &filtered)?;
            Ok(mlua::Value::Table(out))
        },
    )?;
    m.fn_(
        "turns",
        "Return user turns as `{ block_idx, label }` rows where `label` is the first line of the user message. Used by the rewind dialog.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
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
    m.fn_(
        "rewind_to",
        "Rewind the session to a prior user turn. `block_idx = nil` rewinds to before the first turn; `opts.restore_vim_insert = true` re-enters vim insert mode after the rewind.",
        &["block_idx", "opts"],
        |_, (block_idx, opts): (Option<usize>, Option<mlua::Table>)| -> LuaResult<()> {
            let restore_vim_insert = opts
                .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                .unwrap_or(false);
            crate::lua::with_app(|app| app.rewind_to_block(block_idx, restore_vim_insert));
            Ok(())
        },
    )?;
    m.fn_(
        "list",
        "List persisted sessions other than the current one. Each row carries `id`, `title`, `subtitle`, `cwd`, `parent_id`, `updated_at_ms`, `created_at_ms`, and `size_bytes` when available.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
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
    m.fn_(
        "load",
        "Switch the UI to the persisted session with `id`. Replays its message log and resets transient state.",
        &["id"],
        |_, id: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.load_session_by_id(&id));
            Ok(())
        },
    )?;
    m.fn_(
        "delete",
        "Delete the persisted session with `id`. Refuses to delete the currently active session.",
        &["id"],
        |_, id: String| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                if id != app.core.session.id {
                    smelt_core::session::delete(&id);
                }
            });
            Ok(())
        },
    )?;
    m.fn_(
        "fork",
        "Fork the current session: clone its messages into a new session id and switch to it. Useful for branching off an experiment without losing the original timeline.",
        &[],
        |_, ()| -> LuaResult<()> {
            crate::lua::with_app(|app| app.fork_session());
            Ok(())
        },
    )?;
    m.fn_(
        "reset",
        "Cancel any in-flight agent and clear the session to a blank slate. Logs an `agent_stop` event with reason `user_cancel_and_clear`.",
        &[],
        |_, ()| -> LuaResult<()> {
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
    Ok(())
}
