//! `smelt.session` bindings — current session metadata, turn list,
//! rewind, list / load / delete persisted sessions.

use super::app_read;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let session_tbl = lua.create_table()?;
    session_tbl.set(
        "title",
        app_read!(lua, |app| app.core.session.title.clone()),
    )?;
    session_tbl.set("cwd", app_read!(lua, |app| app.cwd.clone()))?;
    session_tbl.set(
        "created_at_ms",
        app_read!(lua, |app| app.core.session.created_at_ms),
    )?;
    session_tbl.set("id", app_read!(lua, |app| app.core.session.id.clone()))?;
    session_tbl.set(
        "dir",
        app_read!(lua, |app| crate::session::dir_for(&app.core.session)
            .display()
            .to_string()),
    )?;
    session_tbl.set(
        "turns",
        lua.create_function(|lua, ()| {
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
        })?,
    )?;
    session_tbl.set(
        "rewind_to",
        lua.create_function(
            |_, (block_idx, opts): (Option<usize>, Option<mlua::Table>)| {
                let restore_vim_insert = opts
                    .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                    .unwrap_or(false);
                crate::lua::with_app(|app| app.rewind_to_block(block_idx, restore_vim_insert));
                Ok(())
            },
        )?,
    )?;
    session_tbl.set(
        "list",
        lua.create_function(|lua, ()| {
            let current_id =
                crate::lua::try_with_host(|host| host.session().id.clone()).unwrap_or_default();
            let sessions = crate::session::list_sessions();
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
        })?,
    )?;
    session_tbl.set(
        "load",
        lua.create_function(|_, id: String| {
            crate::lua::with_app(|app| app.load_session_by_id(&id));
            Ok(())
        })?,
    )?;
    session_tbl.set(
        "delete",
        lua.create_function(|_, id: String| {
            crate::lua::with_app(|app| {
                if id != app.core.session.id {
                    crate::session::delete(&id);
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("session", session_tbl)?;
    Ok(())
}
