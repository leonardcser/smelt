//! `smelt.transcript` host-tier renderer hooks.

use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::lua::doc::{record_module_doc, ApiClassification, Tier};
use crate::lua::module::LuaMod;

fn renderer_cache_key_hash(cache_key: Option<String>) -> u64 {
    let Some(cache_key) = cache_key else {
        return 0;
    };
    let hash = crate::utils::hash_serializable(&cache_key);
    if hash == 0 {
        1
    } else {
        hash
    }
}

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
        "Bundled default transcript renderers. These are ordinary Lua helpers used by the default root renderer and available for user renderers to call or compose.",
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
        &["renderer", "cache_key"],
        move |lua, (renderer, cache_key): (mlua::Function, Option<String>)| -> LuaResult<u64> {
            let handle = LuaHandle::from_func(lua, renderer)?;
            let mut slot = shared_set
                .transcript_renderer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *slot = Some(handle);
            shared_set
                .transcript_renderer_cache_key
                .store(renderer_cache_key_hash(cache_key), Ordering::Release);
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
            shared_invalidate
                .transcript_renderer_cache_key
                .store(0, Ordering::Release);
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
            let cache_key = super::transcript_groups::cache_key_hash(&registry);
            shared_register_group
                .transcript_groups_cache_key
                .store(cache_key, Ordering::Release);
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
            let cache_key = super::transcript_groups::cache_key_hash(&registry);
            shared_unregister_group
                .transcript_groups_cache_key
                .store(cache_key, Ordering::Release);
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

    let shared_groups_cache_key = Arc::clone(shared);
    m.private_fn(
        "__groups_cache_key",
        &[],
        move |_, ()| -> LuaResult<Option<u64>> {
            let key = shared_groups_cache_key
                .transcript_groups_cache_key
                .load(Ordering::Acquire);
            Ok((key != 0).then_some(key))
        },
    )?;

    Ok(())
}
