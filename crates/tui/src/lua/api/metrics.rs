//! `smelt.metrics` bindings — preformatted text for the `/stats` and
//! `/cost` dialogs. Each fn returns a single string ready to drop into
//! a `kind = "content"` dialog panel.

use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let metrics_tbl = lua.create_table()?;

    metrics_tbl.set(
        "stats_text",
        lua.create_function(|_, ()| {
            let entries = crate::metrics::load();
            let stats = crate::metrics::render_stats(&entries);
            Ok(crate::metrics::render_stats_text(&stats))
        })?,
    )?;

    metrics_tbl.set(
        "session_cost_text",
        lua.create_function(|_, ()| {
            let text = crate::lua::try_with_app(|app| {
                let turns = app.user_turns().len();
                let resolved = engine::pricing::resolve(
                    &app.core.config.model,
                    &app.core.config.provider_type,
                    &app.core.config.model_config,
                );
                let lines = crate::metrics::render_session_cost(
                    app.core.session.session_cost_usd,
                    &app.core.config.model,
                    turns,
                    &resolved,
                );
                crate::metrics::render_cost_text(&lines)
            })
            .unwrap_or_default();
            Ok(text)
        })?,
    )?;

    smelt.set("metrics", metrics_tbl)?;
    Ok(())
}
