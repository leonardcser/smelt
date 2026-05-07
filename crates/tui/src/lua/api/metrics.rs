//! `smelt.metrics` bindings — preformatted text for the `/stats` and
//! `/cost` dialogs, plus a live perf snapshot consumed by the F12
//! debug panel.

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

    metrics_tbl.set(
        "perf_set_enabled",
        lua.create_function(|_, on: bool| {
            smelt_core::perf::set_enabled(on);
            Ok(())
        })?,
    )?;

    metrics_tbl.set(
        "perf_clear",
        lua.create_function(|_, ()| {
            smelt_core::perf::clear();
            Ok(())
        })?,
    )?;

    metrics_tbl.set(
        "perf_snapshot",
        lua.create_function(|lua, ()| {
            let snap = smelt_core::perf::snapshot();
            let out = lua.create_table()?;
            let durs = lua.create_table()?;
            for (i, row) in snap.durations.iter().enumerate() {
                let r = lua.create_table()?;
                r.set("label", row.label)?;
                r.set("count", row.count)?;
                r.set("last_us", row.last_us)?;
                r.set("p50_us", row.p50_us)?;
                r.set("p95_us", row.p95_us)?;
                r.set("p99_us", row.p99_us)?;
                r.set("max_us", row.max_us)?;
                r.set("total_us", row.total_us)?;
                durs.set(i + 1, r)?;
            }
            out.set("durations", durs)?;
            let vals = lua.create_table()?;
            for (i, row) in snap.values.iter().enumerate() {
                let r = lua.create_table()?;
                r.set("label", row.label)?;
                r.set("count", row.count)?;
                r.set("last", row.last)?;
                r.set("p50", row.p50)?;
                r.set("p95", row.p95)?;
                r.set("p99", row.p99)?;
                r.set("max", row.max)?;
                r.set("total", row.total)?;
                vals.set(i + 1, r)?;
            }
            out.set("values", vals)?;
            out.set("enabled", smelt_core::perf::enabled())?;
            Ok(out)
        })?,
    )?;

    smelt.set("metrics", metrics_tbl)?;
    Ok(())
}
