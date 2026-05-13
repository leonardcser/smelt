//! `smelt.metrics` bindings — preformatted text for the `/stats` and
//! `/cost` dialogs, plus a live perf snapshot consumed by the F12
//! debug panel.

use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};

#[lua_module(
    name = "smelt.metrics",
    doc = "Preformatted stats text and live perf instrumentation. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let metrics_tbl = lua.create_table()?;
    register_ui_fn(
        &metrics_tbl,
        "smelt.metrics",
        "stats_text",
        "Return preformatted text for the `/stats` dialog (per-model token totals and request counts loaded from the on-disk metrics ledger).",
        &[],
        lua,
        |_, ()|  -> LuaResult<String>{
            let entries = crate::metrics::load();
            let stats = crate::metrics::render_stats(&entries);
            Ok(crate::metrics::render_stats_text(&stats))
        },
    )?;

    register_ui_fn(
        &metrics_tbl,
        "smelt.metrics",
        "session_cost_text",
        "Return preformatted text for the `/cost` dialog showing the current session's cost, per-turn average, and resolved pricing for the active model.",
        &[],
        lua,
        |_, ()|  -> LuaResult<String>{
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
        },
    )?;

    let perf_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.metrics.perf",
        "Perf instrumentation toggle, clear, and snapshot. UiHost-only.",
    );

    register_ui_fn(
        &perf_tbl,
        "smelt.metrics.perf",
        "set_enabled",
        "Toggle perf instrumentation collection on/off. Disabled by default; enable to populate `snapshot` for the F12 debug panel.",
        &["on"],
        lua,
        |_, on: bool|  -> LuaResult<()>{
            smelt_perf::perf::set_enabled(on);
            Ok(())
        },
    )?;

    register_ui_fn(
        &perf_tbl,
        "smelt.metrics.perf",
        "clear",
        "Clear all accumulated perf samples (durations and value gauges).",
        &[],
        lua,
        |_, ()| -> LuaResult<()> {
            smelt_perf::perf::clear();
            Ok(())
        },
    )?;

    register_ui_fn(
        &perf_tbl,
        "smelt.metrics.perf",
        "snapshot",
        "Return the current perf snapshot as `{ durations, values, enabled }` with per-label `count`, `last`, `p50`, `p95`, `p99`, `max`, `total` fields. Powers the F12 debug panel.",
        &[],
        lua,
        |lua, ()|  -> LuaResult<mlua::Table>{
            let snap = smelt_perf::perf::snapshot();
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
            out.set("enabled", smelt_perf::perf::enabled())?;
            Ok(out)
        },
    )?;

    metrics_tbl.set("perf", perf_tbl)?;
    smelt.set("metrics", metrics_tbl)?;
    Ok(())
}
