//! `smelt.metrics` bindings - metrics ledger access plus live perf
//! instrumentation consumed by Lua UI.

use mlua::prelude::*;
use smelt_core::lua::doc::{ApiClassification, Tier};
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
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
            let entries = crate::lua::try_with_platform_host(|host| host.metrics_entries())
                .unwrap_or_default();
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

    let perf = m.sub_with_classification(
        "perf",
        "Perf instrumentation toggle, clear, and snapshot. UiHost-only.",
        ApiClassification::Advanced,
    )?;

    perf.live_only_fn(
        "set_enabled",
        "Toggle perf instrumentation collection on/off. Disabled by default; enable to populate `snapshot` for the F12 debug panel.",
        &["on"],
        |_, on: bool| -> LuaResult<()> {
            smelt_perf::perf::set_enabled(on);
            Ok(())
        },
    )?;

    perf.live_only_fn(
        "clear",
        "Clear all accumulated perf samples (durations and value gauges).",
        &[],
        |_, ()| -> LuaResult<()> {
            smelt_perf::perf::clear();
            Ok(())
        },
    )?;

    perf.fn_(
        "enabled",
        "Return whether perf instrumentation collection is enabled.",
        &[],
        |_, ()| -> LuaResult<bool> { Ok(smelt_perf::perf::enabled()) },
    )?;

    perf.fn_(
        "snapshot",
        "Return the current perf snapshot as `{ durations, values, enabled }` with per-label `count`, `last`, `p50`, `p95`, `p99`, `max`, `total` fields. Powers detailed diagnostics; prefer `snapshot_top` for live UI panels.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let _perf = smelt_perf::perf::begin("metrics:perf_snapshot");
            perf_snapshot_to_lua(lua, smelt_perf::perf::snapshot())
        },
    )?;

    perf.fn_(
        "snapshot_top",
        "Return a cheap live perf snapshot with only the top `limit` duration rows and no value rows. Intended for panels that refresh frequently.",
        &[
            "limit",
        ],
        |lua, limit: Option<u64>| -> LuaResult<mlua::Table> {
            let _perf = smelt_perf::perf::begin("metrics:perf_snapshot_top");
            perf_snapshot_to_lua(lua, smelt_perf::perf::snapshot_top(limit.unwrap_or(16) as usize))
        },
    )?;

    Ok(())
}

fn perf_snapshot_to_lua(lua: &Lua, snap: smelt_perf::perf::Snapshot) -> LuaResult<mlua::Table> {
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
}
