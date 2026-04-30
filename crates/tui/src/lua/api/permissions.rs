//! `smelt.permissions` bindings — list current session + workspace
//! rules, sync a Lua-built ruleset back through the App. Pre-P5
//! surface over `RuntimeApprovals` + [`crate::permissions::store`];
//! grows the rest of the `tui::permissions` capability surface in
//! P5.c when engine permission policy lands here.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let permissions_tbl = lua.create_table()?;
    permissions_tbl.set(
        "list",
        lua.create_function(|lua, ()| {
            let (session_entries, cwd) = crate::lua::try_with_app(|app| {
                let entries = app
                    .session_permission_entries()
                    .into_iter()
                    .map(|e| (e.tool, e.pattern))
                    .collect::<Vec<_>>();
                (entries, app.cwd.clone())
            })
            .unwrap_or_default();
            let out = lua.create_table()?;
            let session_arr = lua.create_table()?;
            for (i, (tool, pattern)) in session_entries.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("tool", tool)?;
                row.set("pattern", pattern)?;
                session_arr.set(i + 1, row)?;
            }
            out.set("session", session_arr)?;
            let workspace_arr = lua.create_table()?;
            for (i, rule) in crate::permissions::store::load(&cwd)
                .into_iter()
                .enumerate()
            {
                let row = lua.create_table()?;
                row.set("tool", rule.tool)?;
                let pats = lua.create_table()?;
                for (j, p) in rule.patterns.into_iter().enumerate() {
                    pats.set(j + 1, p)?;
                }
                row.set("patterns", pats)?;
                workspace_arr.set(i + 1, row)?;
            }
            out.set("workspace", workspace_arr)?;
            Ok(out)
        })?,
    )?;
    permissions_tbl.set(
        "sync",
        lua.create_function(|_, spec: mlua::Table| {
            let mut session_entries: Vec<crate::app::transcript_model::PermissionEntry> =
                Vec::new();
            if let Ok(arr) = spec.get::<mlua::Table>("session") {
                for row in arr.sequence_values::<mlua::Table>().flatten() {
                    let tool: String = row.get("tool").unwrap_or_default();
                    let pattern: String = row.get("pattern").unwrap_or_default();
                    session_entries
                        .push(crate::app::transcript_model::PermissionEntry { tool, pattern });
                }
            }
            let mut workspace_rules: Vec<crate::permissions::store::Rule> = Vec::new();
            if let Ok(arr) = spec.get::<mlua::Table>("workspace") {
                for row in arr.sequence_values::<mlua::Table>().flatten() {
                    let tool: String = row.get("tool").unwrap_or_default();
                    let mut patterns: Vec<String> = Vec::new();
                    if let Ok(pats) = row.get::<mlua::Table>("patterns") {
                        for p in pats.sequence_values::<String>().flatten() {
                            patterns.push(p);
                        }
                    }
                    workspace_rules.push(crate::permissions::store::Rule { tool, patterns });
                }
            }
            crate::lua::with_app(|app| app.sync_permissions(session_entries, workspace_rules));
            Ok(())
        })?,
    )?;
    smelt.set("permissions", permissions_tbl)?;
    Ok(())
}
