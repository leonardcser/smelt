//! `smelt.transcript` host-tier renderer hooks.

use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::lua::doc::{record_module_doc, ApiClassification, Tier};
use crate::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "transcript",
        "Transcript display policy and rendered transcript inspection. Host-tier renderer hooks are layered with UiHost read APIs when a TUI is active.",
        Tier::Host,
    )?;
    record_module_doc(
        "smelt.transcript.defaults",
        "Bundled default transcript entry points for rendering semantic blocks and raw tool output.",
        ApiClassification::Advanced,
    );
    record_module_doc(
        "smelt.transcript.groups",
        "Declarative transcript display grouping. Register adjacent-run group rules while the host owns deterministic planning and the composed root renderer presents resulting group nodes.",
        ApiClassification::Supported,
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

    let shared_register_group = Arc::clone(shared);
    m.private_fn(
        "__register_group",
        &["spec"],
        move |_, spec: mlua::Table| -> LuaResult<u64> {
            let mut registry = shared_register_group
                .transcript_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let order = registry.next_order;
            registry.next_order = registry.next_order.wrapping_add(1).max(1);
            let group = super::transcript_groups::parse_registration(spec, order)?;
            let token = group.token;
            registry.entries.insert(group.spec.name.clone(), group);
            shared_register_group
                .transcript_groups_generation
                .fetch_add(1, Ordering::AcqRel);
            Ok(token)
        },
    )?;

    let shared_unregister_group = Arc::clone(shared);
    m.private_fn(
        "__unregister_group",
        &["name", "token"],
        move |_, (name, token): (String, u64)| -> LuaResult<bool> {
            let mut registry = shared_unregister_group
                .transcript_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(entry) = registry.entries.get(&name) else {
                return Ok(false);
            };
            if entry.token != token {
                return Ok(false);
            }
            registry.entries.remove(&name);
            shared_unregister_group
                .transcript_groups_generation
                .fetch_add(1, Ordering::AcqRel);
            Ok(true)
        },
    )?;

    let shared_groups = Arc::clone(shared);
    m.private_fn("__groups", &[], move |lua, ()| -> LuaResult<mlua::Table> {
        let registry = shared_groups
            .transcript_groups
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        super::transcript_groups::specs_to_lua(lua, registry.specs())
    })?;

    let shared_groups_generation = Arc::clone(shared);
    m.private_fn("__groups_generation", &[], move |_, ()| -> LuaResult<u64> {
        Ok(shared_groups_generation
            .transcript_groups_generation
            .load(Ordering::Acquire))
    })?;

    Ok(())
}
