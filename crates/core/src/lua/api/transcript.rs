//! `smelt.transcript` host-tier renderer hooks.

use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::lua::doc::{record_module_doc, Tier};
use crate::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "transcript",
        "Transcript display policy and rendered transcript inspection. Host-tier renderer hooks are layered with UiHost read APIs when a TUI is active.",
        Tier::Host,
    )?;
    record_module_doc(
        "smelt.transcript.defaults",
        "Bundled default transcript renderers. These are ordinary Lua helpers used by the default root renderer and available for user renderers to call or compose.",
    );

    let shared_set = Arc::clone(shared);
    m.private_fn(
        "__set_renderer",
        &["renderer"],
        move |lua, renderer: mlua::Function| -> LuaResult<u64> {
            let handle = LuaHandle::from_func(lua, renderer)?;
            let mut slot = shared_set
                .transcript_renderer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *slot = Some(handle);
            Ok(shared_set
                .transcript_renderer_generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1))
        },
    )?;

    let shared_get = Arc::clone(shared);
    m.private_fn(
        "__get_renderer",
        &[],
        move |lua, ()| -> LuaResult<Option<mlua::Function>> {
            let slot = shared_get
                .transcript_renderer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(handle) = slot.as_ref() else {
                return Ok(None);
            };
            lua.registry_value::<mlua::Function>(&handle.key).map(Some)
        },
    )?;

    let shared_invalidate = Arc::clone(shared);
    m.private_fn(
        "__invalidate_renderer",
        &[],
        move |_, ()| -> LuaResult<u64> {
            Ok(shared_invalidate
                .transcript_renderer_generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1))
        },
    )?;

    let shared_generation = Arc::clone(shared);
    m.private_fn(
        "__renderer_generation",
        &[],
        move |_, ()| -> LuaResult<u64> {
            Ok(shared_generation
                .transcript_renderer_generation
                .load(Ordering::Acquire))
        },
    )?;

    Ok(())
}
