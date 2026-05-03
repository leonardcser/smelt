//! `smelt.permissions` bindings — list current session + workspace
//! rules, sync a Lua-built ruleset back through the App. Pre-P5
//! surface over `RuntimeApprovals` + [`crate::permissions::store`];
//! grows the rest of the `app::permissions` capability surface in
//! P5.c when engine permission policy lands here.

use mlua::prelude::*;
use std::sync::Arc;

fn parse_ruleset(
    _lua: &Lua,
    t: &mlua::Table,
) -> LuaResult<crate::permissions::rules::RawRuleSet> {
    let mut allow = Vec::new();
    let mut ask = Vec::new();
    let mut deny = Vec::new();
    if let Ok(arr) = t.get::<mlua::Table>("allow") {
        for v in arr.sequence_values::<String>().flatten() {
            allow.push(v);
        }
    }
    if let Ok(arr) = t.get::<mlua::Table>("ask") {
        for v in arr.sequence_values::<String>().flatten() {
            ask.push(v);
        }
    }
    if let Ok(arr) = t.get::<mlua::Table>("deny") {
        for v in arr.sequence_values::<String>().flatten() {
            deny.push(v);
        }
    }
    Ok(crate::permissions::rules::RawRuleSet { allow, ask, deny })
}

fn parse_mode_perms(
    lua: &Lua,
    t: &mlua::Table,
) -> LuaResult<crate::permissions::rules::RawModePerms> {
    let tools = t
        .get::<Option<mlua::Table>>("tools")
        .ok()
        .flatten()
        .map(|tbl| parse_ruleset(lua, &tbl))
        .transpose()?
        .unwrap_or_default();
    let bash = t
        .get::<Option<mlua::Table>>("bash")
        .ok()
        .flatten()
        .map(|tbl| parse_ruleset(lua, &tbl))
        .transpose()?
        .unwrap_or_default();
    let web_fetch = t
        .get::<Option<mlua::Table>>("web_fetch")
        .ok()
        .flatten()
        .map(|tbl| parse_ruleset(lua, &tbl))
        .transpose()?
        .unwrap_or_default();
    let mcp = t
        .get::<Option<mlua::Table>>("mcp")
        .ok()
        .flatten()
        .map(|tbl| parse_ruleset(lua, &tbl))
        .transpose()?
        .unwrap_or_default();
    Ok(crate::permissions::rules::RawModePerms {
        tools,
        bash,
        web_fetch,
        mcp,
    })
}

pub(super) fn register(
    lua: &Lua,
    smelt: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
) -> LuaResult<()> {
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
            let mut session_entries: Vec<crate::transcript_model::PermissionEntry> =
                Vec::new();
            if let Ok(arr) = spec.get::<mlua::Table>("session") {
                for row in arr.sequence_values::<mlua::Table>().flatten() {
                    let tool: String = row.get("tool").unwrap_or_default();
                    let pattern: String = row.get("pattern").unwrap_or_default();
                    session_entries
                        .push(crate::transcript_model::PermissionEntry { tool, pattern });
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
    permissions_tbl.set(
        "set_rules",
        lua.create_function({
            let shared = Arc::clone(shared);
            move |lua, spec: mlua::Table| {
                let default = spec
                    .get::<Option<mlua::Table>>("default")
                    .ok()
                    .flatten()
                    .map(|t| parse_mode_perms(lua, &t))
                    .transpose()?
                    .unwrap_or_default();
                let normal = spec
                    .get::<Option<mlua::Table>>("normal")
                    .ok()
                    .flatten()
                    .map(|t| parse_mode_perms(lua, &t))
                    .transpose()?
                    .unwrap_or_default();
                let plan = spec
                    .get::<Option<mlua::Table>>("plan")
                    .ok()
                    .flatten()
                    .map(|t| parse_mode_perms(lua, &t))
                    .transpose()?
                    .unwrap_or_default();
                let apply = spec
                    .get::<Option<mlua::Table>>("apply")
                    .ok()
                    .flatten()
                    .map(|t| parse_mode_perms(lua, &t))
                    .transpose()?
                    .unwrap_or_default();
                let yolo = spec
                    .get::<Option<mlua::Table>>("yolo")
                    .ok()
                    .flatten()
                    .map(|t| parse_mode_perms(lua, &t))
                    .transpose()?
                    .unwrap_or_default();
                let rules = crate::permissions::rules::RawPerms {
                    default,
                    normal,
                    plan,
                    apply,
                    yolo,
                };
                let mut guard = shared
                    .permission_rules
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *guard = Some(rules);
                Ok(())
            }
        })?,
    )?;
    smelt.set("permissions", permissions_tbl)?;
    Ok(())
}
