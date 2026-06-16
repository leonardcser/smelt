use crate::lua::shared::{
    RegisteredTranscriptGroup, TranscriptGroupBucket, TranscriptGroupFieldMatch,
    TranscriptGroupRegistry, TranscriptGroupSelector, TranscriptGroupSpec,
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

fn selector_match_value(value: mlua::Value, label: &str) -> LuaResult<Option<String>> {
    match value {
        mlua::Value::Nil => Ok(None),
        mlua::Value::String(s) => {
            let value = s.to_string_lossy();
            if value.is_empty() {
                return Err(mlua::Error::external(format!("{label} must be non-empty")));
            }
            Ok(Some(value))
        }
        mlua::Value::Integer(n) => Ok(Some(n.to_string())),
        mlua::Value::Number(n) if n.fract() == 0.0 => Ok(Some(format!("{n:.0}"))),
        other => Err(mlua::Error::external(format!(
            "{label} must be a string or integer, got {}",
            other.type_name()
        ))),
    }
}

fn canonical_selector_field(field: &str) -> &str {
    match field {
        "event_type" => "event",
        other => other,
    }
}

fn set_selector_field(
    fields: &mut Vec<TranscriptGroupFieldMatch>,
    field: &str,
    value: mlua::Value,
    label: &str,
) -> LuaResult<()> {
    let Some(value) = selector_match_value(value, label)? else {
        return Ok(());
    };
    let field = canonical_selector_field(field);
    if let Some(existing) = fields.iter().find(|existing| existing.field == field) {
        if existing.value != value {
            return Err(mlua::Error::external(format!(
                "{label} conflicts with earlier selector.{}",
                existing.field
            )));
        }
        return Ok(());
    }
    fields.push(TranscriptGroupFieldMatch {
        field: field.to_string(),
        value,
    });
    Ok(())
}

fn parse_selector(table: mlua::Table) -> LuaResult<TranscriptGroupSelector> {
    let mut fields = Vec::new();
    for field in ["event", "event_type", "process_id", "exit_code"] {
        set_selector_field(
            &mut fields,
            field,
            table.get::<mlua::Value>(field)?,
            &format!("selector.{field}"),
        )?;
    }
    if let Some(field_table) = table.get::<Option<mlua::Table>>("fields")? {
        for pair in field_table.pairs::<String, mlua::Value>() {
            let (field, value) = pair?;
            if field.is_empty() {
                return Err(mlua::Error::external(
                    "selector.fields keys must be non-empty",
                ));
            }
            set_selector_field(
                &mut fields,
                &field,
                value,
                &format!("selector.fields.{field}"),
            )?;
        }
    }
    let selector = TranscriptGroupSelector {
        kind: optional_non_empty_string(&table, "kind", "selector.kind")?,
        name: optional_non_empty_string(&table, "name", "selector.name")?,
        terminal: table.get("terminal")?,
        fields,
    };
    if selector.kind.is_none()
        && selector.name.is_none()
        && selector.terminal.is_none()
        && selector.fields.is_empty()
    {
        return Err(mlua::Error::external(
            "selector must set at least one of kind, name, terminal, or fields",
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
    if !spec.selector.fields.is_empty() {
        let fields = lua.create_table()?;
        for field in &spec.selector.fields {
            fields.set(field.field.as_str(), field.value.clone())?;
            match field.field.as_str() {
                "event" => {
                    selector.set("event", field.value.clone())?;
                    selector.set("event_type", field.value.clone())?;
                }
                "process_id" | "exit_code" => {
                    selector.set(field.field.as_str(), field.value.clone())?;
                }
                _ => {}
            }
        }
        selector.set("fields", fields)?;
    }
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
