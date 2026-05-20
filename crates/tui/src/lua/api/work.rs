//! `smelt.work` — push background work-state tokens. Distinct from
//! `smelt.spinner`, which is just the shared animation primitive: this
//! module is the engine/state-machine surface. Tokens here drive the
//! reactive `work_*` cells and the prompt top-bar indicator. UiHost-only.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::lua::reg::LuaReg;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "work",
        "Push background work-state tokens. Tokens drive the prompt \
top-bar indicator and the reactive `work_*` cells; plugins observe \
state by subscribing to those cells. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "busy",
        "Push a busy token onto the per-app stack and return a `Reg` \
whose `:remove()` pops it. While any token is live the prompt top-bar \
indicator animates with the top token's `label`, and `work_state` \
flips to `\"busy\"` (unless an engine-side turn is also live, in \
which case engine state wins). Multiple plugins can hold tokens \
concurrently; the most recently pushed label wins for display.",
        &["label"],
        |_, label: String| -> LuaResult<LuaReg> {
            let id = crate::lua::with_app(|app| app.busy_stack.push(label));
            Ok(LuaReg::new(move || {
                crate::lua::try_with_app(|app| app.busy_stack.release(id)).unwrap_or(false)
            }))
        },
    )?;
    m.fn_(
        "is_busy",
        "Return `true` while at least one `smelt.work.busy` token is \
live. Plugins that need richer state (top label, full stack, retry \
countdown, archived outcome) subscribe to the reactive `work_*` cells \
instead.",
        &[],
        |_, ()| Ok(crate::lua::try_with_app(|app| app.busy_stack.is_busy()).unwrap_or(false)),
    )?;
    Ok(())
}
