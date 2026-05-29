//! `smelt.metrics` bindings — metrics ledger access plus live perf
//! instrumentation consumed by Lua UI.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "metrics",
        "Metrics ledger access and live perf instrumentation. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "entries",
        "Return raw entries from the on-disk metrics ledger. Lua commands aggregate and render these records into UI.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let entries = crate::metrics::load();
            let out = lua.create_table()?;
            for (i, e) in entries.iter().enumerate() {
                let row = lua.create_table()?;
                row.set("timestamp_ms", e.timestamp_ms)?;
                row.set("prompt_tokens", e.prompt_tokens)?;
                row.set("completion_tokens", e.completion_tokens)?;
                row.set("model", e.model.as_str())?;
                row.set("cost_usd", e.cost_usd)?;
                row.set("cache_read_tokens", e.cache_read_tokens)?;
                row.set("cache_write_tokens", e.cache_write_tokens)?;
                row.set("reasoning_tokens", e.reasoning_tokens)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;

    let perf = m.sub(
        "perf",
        "Perf instrumentation toggle, clear, and snapshot. UiHost-only.",
    )?;

    perf.fn_(
        "set_enabled",
        "Toggle perf instrumentation collection on/off. Disabled by default; enable to populate `snapshot` for the F12 debug panel.",
        &["on"],
        |_, on: bool| -> LuaResult<()> {
            smelt_perf::perf::set_enabled(on);
            Ok(())
        },
    )?;

    perf.fn_(
        "clear",
        "Clear all accumulated perf samples (durations and value gauges).",
        &[],
        |_, ()| -> LuaResult<()> {
            smelt_perf::perf::clear();
            Ok(())
        },
    )?;

    perf.fn_(
        "snapshot",
        "Return the current perf snapshot as `{ durations, values, enabled }` with per-label `count`, `last`, `p50`, `p95`, `p99`, `max`, `total` fields. Powers the F12 debug panel.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
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

    Ok(())
}
