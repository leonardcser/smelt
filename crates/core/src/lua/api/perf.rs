//! `smelt.perf` - timing helpers for Lua.

use crate::lua::doc::Tier;
use crate::lua::lua_type::LuaCallback;
use crate::lua::module::LuaMod;
use mlua::prelude::*;
use std::collections::HashSet;
use std::sync::Mutex;

// `smelt_perf::perf::begin` takes a `&'static str` so the sample map can
// key labels without copying. We accept labels that look like real
// identifiers (alphanumeric / `_` / `:` / `-`, ≤ 64 chars) and intern them
// - a real plugin's bounded set of buckets leaks at most once each. Anything
// else (random binary bytes, oversized labels) buckets into a single static
// fallback so callers can't make us malloc per call.
const FALLBACK: &str = "lua:user";
const MAX_LABEL_LEN: usize = 64;
const MAX_DISTINCT_LABELS: usize = 256;

fn label_is_clean(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_LABEL_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.'))
}

fn intern_label(label: &str) -> &'static str {
    if !label_is_clean(label) {
        return FALLBACK;
    }
    static INTERNED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut guard = INTERNED.lock().unwrap();
    let set = guard.get_or_insert_with(HashSet::new);
    if let Some(&existing) = set.get(label) {
        return existing;
    }
    if set.len() >= MAX_DISTINCT_LABELS {
        return FALLBACK;
    }
    let leaked: &'static str = Box::leak(label.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "perf",
        "Lightweight scope timers that feed `smelt.metrics.perf_snapshot`. Wrap a hot Lua block to see where time goes when perf collection is enabled.",
        Tier::Host,
    )?;

    m.fn_(
        "time",
        "Run `fn()` and record its elapsed time under `label`. Returns whatever `fn` returns (single value). Cheap when perf collection is disabled.",
        &["label", "fn"],
        |_, (label, cb): (String, LuaCallback<(), mlua::Value>)| -> LuaResult<mlua::Value> {
            let label = intern_label(&label);
            let _perf = smelt_perf::perf::begin(label);
            cb.as_function().call::<mlua::Value>(())
        },
    )?;

    Ok(())
}
