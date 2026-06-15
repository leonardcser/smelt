//! `smelt.transcript` host-tier renderer hooks.

use crate::lua::{LuaHandle, LuaShared};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::lua::doc::{record_module_doc, Tier};
use crate::lua::module::LuaMod;
use crate::lua::shared::{
    RegisteredTranscriptGroup, TranscriptGroupBucket, TranscriptGroupRegistry,
    TranscriptGroupSelector, TranscriptGroupSpec,
};

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

fn groups_cache_key_hash(registry: &TranscriptGroupRegistry) -> u64 {
    let specs = registry.specs();
    if specs.is_empty() || specs.iter().any(|spec| spec.cache_key.is_none()) {
        return 0;
    }
    let hash = crate::utils::hash_serializable(&specs);
    if hash == 0 {
        1
    } else {
        hash
    }
}

fn non_empty_string(value: Option<String>, label: &str) -> LuaResult<String> {
    let Some(value) = value else {
        return Err(mlua::Error::external(format!("{label} is required")));
    };
    if value.is_empty() {
        return Err(mlua::Error::external(format!("{label} must be non-empty")));
    }
    Ok(value)
}

fn optional_non_empty_string(
    table: &mlua::Table,
    key: &str,
    label: &str,
) -> LuaResult<Option<String>> {
    let value: Option<String> = table.get(key)?;
    if value.as_deref() == Some("") {
        return Err(mlua::Error::external(format!("{label} must be non-empty")));
    }
    Ok(value)
}

fn parse_selector(table: mlua::Table) -> LuaResult<TranscriptGroupSelector> {
    let selector = TranscriptGroupSelector {
        kind: optional_non_empty_string(&table, "kind", "selector.kind")?,
        name: optional_non_empty_string(&table, "name", "selector.name")?,
        terminal: table.get("terminal")?,
    };
    if selector.kind.is_none() && selector.name.is_none() && selector.terminal.is_none() {
        return Err(mlua::Error::external(
            "selector must set at least one of kind, name, or terminal",
        ));
    }
    Ok(selector)
}

fn parse_bucket(value: mlua::Value) -> LuaResult<Option<TranscriptGroupBucket>> {
    match value {
        mlua::Value::Nil => Ok(None),
        mlua::Value::String(s) => {
            let field = s.to_string_lossy();
            if field.is_empty() {
                return Err(mlua::Error::external("bucket field must be non-empty"));
            }
            Ok(Some(TranscriptGroupBucket {
                fields: vec![field],
            }))
        }
        mlua::Value::Table(table) => {
            let mut fields = Vec::new();
            for value in table.sequence_values::<String>() {
                let value = value?;
                if value.is_empty() {
                    return Err(mlua::Error::external("bucket fields must be non-empty"));
                }
                fields.push(value);
            }
            if fields.is_empty() {
                if let Some(field) = optional_non_empty_string(&table, "field", "bucket.field")? {
                    fields.push(field);
                }
            }
            if fields.is_empty() {
                return Err(mlua::Error::external(
                    "bucket must be a string or a non-empty array of strings",
                ));
            }
            Ok(Some(TranscriptGroupBucket { fields }))
        }
        other => Err(mlua::Error::external(format!(
            "bucket must be a string or table, got {}",
            other.type_name()
        ))),
    }
}

fn parse_group_registration(
    lua: &Lua,
    spec: mlua::Table,
    order: u64,
) -> LuaResult<RegisteredTranscriptGroup> {
    let name = non_empty_string(spec.get("name")?, "name")?;
    let cache_key = optional_non_empty_string(&spec, "cache_key", "cache_key")?;
    let priority = spec.get::<Option<i64>>("priority")?.unwrap_or(0);
    let min = spec.get::<Option<usize>>("min")?.unwrap_or(2);
    if min < 2 {
        return Err(mlua::Error::external("min must be at least 2"));
    }
    let default_view = optional_non_empty_string(&spec, "default_view", "default_view")?;
    let selector_table = spec.get::<mlua::Table>("selector")?;
    let selector = parse_selector(selector_table)?;
    let bucket = parse_bucket(spec.get::<mlua::Value>("bucket")?)?;
    let render = spec.get::<mlua::Function>("render")?;
    let handle = LuaHandle::from_func(lua, render)?;
    Ok(RegisteredTranscriptGroup {
        spec: TranscriptGroupSpec {
            name,
            cache_key,
            priority,
            registration_order: order,
            min,
            default_view,
            selector,
            bucket,
        },
        render: handle,
        token: order,
    })
}

fn group_spec_to_lua(lua: &Lua, spec: &TranscriptGroupSpec) -> LuaResult<mlua::Table> {
    let out = lua.create_table()?;
    out.set("name", spec.name.clone())?;
    out.set("cache_key", spec.cache_key.clone())?;
    out.set("priority", spec.priority)?;
    out.set("registration_order", spec.registration_order)?;
    out.set("min", spec.min)?;
    out.set("default_view", spec.default_view.clone())?;
    let selector = lua.create_table()?;
    selector.set("kind", spec.selector.kind.clone())?;
    selector.set("name", spec.selector.name.clone())?;
    selector.set("terminal", spec.selector.terminal)?;
    out.set("selector", selector)?;
    if let Some(bucket) = &spec.bucket {
        let fields = lua.create_table()?;
        for (i, field) in bucket.fields.iter().enumerate() {
            fields.set(i + 1, field.clone())?;
        }
        out.set("bucket", fields)?;
    }
    Ok(out)
}

fn group_specs_to_lua(lua: &Lua, specs: Vec<TranscriptGroupSpec>) -> LuaResult<mlua::Table> {
    let out = lua.create_table()?;
    for (i, spec) in specs.iter().enumerate() {
        out.set(i + 1, group_spec_to_lua(lua, spec)?)?;
    }
    Ok(out)
}

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
        move |lua, spec: mlua::Table| -> LuaResult<u64> {
            let mut registry = shared_register_group
                .transcript_groups
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let order = registry.next_order;
            registry.next_order = registry.next_order.wrapping_add(1).max(1);
            let group = parse_group_registration(lua, spec, order)?;
            let token = group.token;
            registry.entries.insert(group.spec.name.clone(), group);
            let cache_key = groups_cache_key_hash(&registry);
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
            let cache_key = groups_cache_key_hash(&registry);
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
        group_specs_to_lua(lua, registry.specs())
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
