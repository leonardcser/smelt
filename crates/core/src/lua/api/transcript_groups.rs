use crate::lua::shared::{
    RegisteredTranscriptGroup, TranscriptGroupBucket, TranscriptGroupRegistry,
    TranscriptGroupSelector, TranscriptGroupSpec,
};
use crate::lua::LuaHandle;
use mlua::prelude::*;

pub(crate) fn cache_key_hash(registry: &TranscriptGroupRegistry) -> u64 {
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

pub(crate) fn parse_registration(
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

fn spec_to_lua(lua: &Lua, spec: &TranscriptGroupSpec) -> LuaResult<mlua::Table> {
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

pub(crate) fn specs_to_lua(lua: &Lua, specs: Vec<TranscriptGroupSpec>) -> LuaResult<mlua::Table> {
    let out = lua.create_table()?;
    for (i, spec) in specs.iter().enumerate() {
        out.set(i + 1, spec_to_lua(lua, spec)?)?;
    }
    Ok(out)
}
