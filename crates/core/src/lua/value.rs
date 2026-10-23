//! Bounded conversion between Lua values and JSON.
//!
//! Every Lua-to-JSON path goes through [`to_json`] or [`tool_metadata`]. Both
//! walk Lua data without invoking metamethods and fail, rather than truncate,
//! on cycles, unsupported values, non-finite numbers, invalid UTF-8 and
//! exhausted depth, node or byte budgets.
//!
//! Tables with contiguous `1..N` integer keys are arrays, and so are empty
//! tables created by [`new_array`] or [`mark_array`]. Other tables are
//! objects; integer keys become their decimal strings.

use mlua::{Error, Lua, Result, Table, Value};
use serde_json::{Map, Value as Json};
use std::collections::HashMap;

const ARRAY_REGISTRY_KEY: &str = "__smelt_json_arrays";

const MAX_DEPTH: usize = 128;

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    label: &'static str,
    depth: usize,
    nodes: usize,
    bytes: usize,
    key_bytes: usize,
}

const DATA: Limits = Limits {
    label: "JSON value",
    depth: MAX_DEPTH,
    nodes: 1_000_000,
    bytes: 64 * 1024 * 1024,
    key_bytes: 16 * 1024,
};

const METADATA: Limits = Limits {
    label: "tool metadata",
    depth: protocol::TOOL_METADATA_MAX_DEPTH,
    nodes: protocol::TOOL_METADATA_MAX_NODES,
    bytes: protocol::TOOL_METADATA_MAX_BYTES,
    key_bytes: protocol::TOOL_METADATA_MAX_KEY_BYTES,
};

fn array_registry(lua: &Lua) -> Result<Table> {
    if let Ok(registry) = lua.named_registry_value::<Table>(ARRAY_REGISTRY_KEY) {
        return Ok(registry);
    }
    let registry = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.raw_set("__mode", "k")?;
    registry.set_metatable(Some(metatable))?;
    lua.set_named_registry_value(ARRAY_REGISTRY_KEY, registry.clone())?;
    Ok(registry)
}

/// Create an empty table that encodes as a JSON array.
pub fn new_array(lua: &Lua) -> Result<Table> {
    let table = lua.create_table()?;
    mark_array(lua, &table)?;
    Ok(table)
}

/// Make an empty `table` encode as a JSON array rather than an object.
pub fn mark_array(lua: &Lua, table: &Table) -> Result<()> {
    array_registry(lua)?.raw_set(table.clone(), true)
}

fn is_marked_array(lua: &Lua, table: &Table) -> bool {
    array_registry(lua)
        .and_then(|registry| registry.raw_get::<bool>(table.clone()))
        .unwrap_or(false)
}

struct ToJson<'a> {
    lua: &'a Lua,
    limits: Limits,
    active_tables: Vec<usize>,
    nodes: usize,
    bytes: usize,
}

impl<'a> ToJson<'a> {
    fn new(lua: &'a Lua, limits: Limits) -> Self {
        Self {
            lua,
            limits,
            active_tables: Vec::new(),
            nodes: 0,
            bytes: 0,
        }
    }

    fn exceeds(&self, what: &str, limit: impl std::fmt::Display, unit: &str) -> String {
        format!(
            "{} {what}exceeds the {limit}-{unit} limit",
            self.limits.label
        )
    }

    fn visit(&mut self, depth: usize) -> std::result::Result<(), String> {
        if depth > self.limits.depth {
            return Err(self.exceeds("", self.limits.depth, "level depth"));
        }
        self.nodes += 1;
        if self.nodes > self.limits.nodes {
            return Err(self.exceeds("", self.limits.nodes, "node"));
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> std::result::Result<(), String> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.limits.bytes {
            return Err(self.exceeds("", self.limits.bytes, "byte"));
        }
        Ok(())
    }

    fn convert(
        &mut self,
        value: &Value,
        depth: usize,
        ignored_root_keys: &[&str],
    ) -> std::result::Result<Json, String> {
        self.visit(depth)?;
        match value {
            Value::Nil => Ok(Json::Null),
            Value::LightUserData(null) if null.0.is_null() => Ok(Json::Null),
            Value::Boolean(value) => {
                self.charge(1)?;
                Ok(Json::Bool(*value))
            }
            Value::Integer(value) => {
                self.charge(std::mem::size_of::<i64>())?;
                Ok(Json::from(*value))
            }
            Value::Number(value) => {
                self.charge(std::mem::size_of::<f64>())?;
                serde_json::Number::from_f64(*value)
                    .map(Json::Number)
                    .ok_or_else(|| format!("{} contains a non-finite number", self.limits.label))
            }
            Value::String(value) => {
                let value = value.to_str().map_err(|_| {
                    format!(
                        "{} contains a string that is not valid UTF-8",
                        self.limits.label
                    )
                })?;
                self.charge(value.len())?;
                Ok(Json::String(value.to_string()))
            }
            Value::Table(table) => {
                let pointer = table.to_pointer() as usize;
                if self.active_tables.contains(&pointer) {
                    return Err(format!("{} contains a table cycle", self.limits.label));
                }
                self.active_tables.push(pointer);
                let result = self.table(table, depth, ignored_root_keys);
                self.active_tables.pop();
                result
            }
            other => Err(format!(
                "{} contains unsupported Lua {} value",
                self.limits.label,
                other.type_name()
            )),
        }
    }

    fn table(
        &mut self,
        table: &Table,
        depth: usize,
        ignored_root_keys: &[&str],
    ) -> std::result::Result<Json, String> {
        let mut pairs = Vec::new();
        for pair in table.pairs::<Value, Value>() {
            let pair = pair.map_err(|error| format!("{}: {error}", self.limits.label))?;
            if pairs.len() >= self.limits.nodes {
                return Err(self.exceeds("", self.limits.nodes, "node"));
            }
            pairs.push(pair);
        }

        if self.is_array(table, &pairs) {
            let mut array = Vec::with_capacity(pairs.len());
            for index in 1..=pairs.len() {
                let value = table
                    .raw_get::<Value>(index)
                    .map_err(|error| format!("{}: {error}", self.limits.label))?;
                array.push(self.convert(&value, depth + 1, &[])?);
            }
            return Ok(Json::Array(array));
        }

        let mut object = Map::with_capacity(pairs.len());
        for (key, value) in pairs {
            let key = match key {
                Value::String(key) => key
                    .to_str()
                    .map_err(|_| format!("{} key is not valid UTF-8", self.limits.label))?
                    .to_string(),
                Value::Integer(key) => key.to_string(),
                other => {
                    return Err(format!(
                        "{} object contains unsupported Lua {} key",
                        self.limits.label,
                        other.type_name()
                    ));
                }
            };
            if depth == 0 && ignored_root_keys.contains(&key.as_str()) {
                continue;
            }
            if key.len() > self.limits.key_bytes {
                return Err(self.exceeds("key ", self.limits.key_bytes, "byte"));
            }
            self.charge(key.len())?;
            if object.contains_key(&key) {
                return Err(format!(
                    "{} contains duplicate key `{key}`",
                    self.limits.label
                ));
            }
            let value = self.convert(&value, depth + 1, &[])?;
            object.insert(key, value);
        }
        Ok(Json::Object(object))
    }

    fn is_array(&self, table: &Table, pairs: &[(Value, Value)]) -> bool {
        if pairs.is_empty() {
            return is_marked_array(self.lua, table);
        }
        // Integer keys are unique, so 1..N are all present exactly when every
        // key is a positive integer and the largest equals the count.
        let mut max = 0;
        for (key, _) in pairs {
            match key {
                Value::Integer(index) if *index > 0 => max = max.max(*index as usize),
                _ => return false,
            }
        }
        max == pairs.len()
    }
}

fn runtime_error(message: String) -> Error {
    Error::runtime(message)
}

/// Convert Lua data to JSON. Shared subtrees are allowed; cycles, unsupported
/// values, non-finite numbers, invalid UTF-8 and exhausted budgets are errors.
pub fn to_json(lua: &Lua, value: &Value) -> Result<Json> {
    ToJson::new(lua, DATA)
        .convert(value, 0, &[])
        .map_err(runtime_error)
}

pub fn table_to_json(lua: &Lua, table: &Table) -> Result<Json> {
    to_json(lua, &Value::Table(table.clone()))
}

/// Convert an argument record. Arrays are errors, not empty argument maps.
pub fn table_to_object(lua: &Lua, table: &Table) -> Result<HashMap<String, Json>> {
    match table_to_json(lua, table)? {
        Json::Object(object) => Ok(object.into_iter().collect()),
        _ => Err(Error::runtime("expected a JSON object, got an array")),
    }
}

/// Convert tool result metadata under the protocol's metadata limits, leaving
/// out `ignored_root_keys`. The message of a rejected value names the limit.
pub(crate) fn tool_metadata(
    lua: &Lua,
    value: &Value,
    ignored_root_keys: &[&str],
) -> std::result::Result<Json, String> {
    ToJson::new(lua, METADATA).convert(value, 0, ignored_root_keys)
}

/// Project JSON into Lua, mapping null to nil.
pub fn from_json(lua: &Lua, value: &Json) -> Result<Value> {
    from_json_with_null(lua, value, &Value::Nil)
}

/// Project JSON into Lua with null as mlua's non-nil sentinel, so protocol data
/// round-trips through [`to_json`] without losing null members or elements.
pub fn from_json_preserving_nulls(lua: &Lua, value: &Json) -> Result<Value> {
    from_json_with_null(lua, value, &Value::NULL)
}

fn from_json_with_null(lua: &Lua, value: &Json, null: &Value) -> Result<Value> {
    project(lua, value, null, 0)
}

fn project(lua: &Lua, value: &Json, null: &Value, depth: usize) -> Result<Value> {
    if depth > MAX_DEPTH {
        return Err(Error::runtime(format!(
            "JSON value exceeds the {MAX_DEPTH}-level depth limit"
        )));
    }
    Ok(match value {
        Json::Null => null.clone(),
        Json::Bool(value) => Value::Boolean(*value),
        Json::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(integer), _) => Value::Integer(integer),
            (None, Some(float)) => Value::Number(float),
            (None, None) => Value::Nil,
        },
        Json::String(value) => Value::String(lua.create_string(value)?),
        Json::Array(values) => {
            let table = new_array(lua)?;
            for (index, value) in values.iter().enumerate() {
                table.raw_set(index + 1, project(lua, value, null, depth + 1)?)?;
            }
            Value::Table(table)
        }
        Json::Object(values) => {
            let table = lua.create_table_with_capacity(0, values.len())?;
            for (key, value) in values {
                table.raw_set(key.as_str(), project(lua, value, null, depth + 1)?)?;
            }
            Value::Table(table)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn eval(lua: &Lua, code: &str) -> Value {
        lua.load(code).eval().unwrap()
    }

    fn encode(lua: &Lua, code: &str) -> Result<Json> {
        to_json(lua, &eval(lua, code))
    }

    fn message(result: Result<Json>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn tables_split_into_arrays_and_objects() {
        let lua = Lua::new();
        assert_eq!(
            encode(&lua, "return {1, 'a', true}").unwrap(),
            json!([1, "a", true])
        );
        assert_eq!(encode(&lua, "return {}").unwrap(), json!({}));
        assert_eq!(
            encode(&lua, "return {a = 1, [2] = 'x'}").unwrap(),
            json!({"a": 1, "2": "x"})
        );
        assert_eq!(
            encode(&lua, "return {[1] = 1, [3] = 3}").unwrap(),
            json!({"1": 1, "3": 3})
        );
        assert_eq!(
            encode(&lua, "return {[0] = 'zero'}").unwrap(),
            json!({"0": "zero"})
        );
    }

    #[test]
    fn marked_empty_tables_encode_as_arrays_until_populated() {
        let lua = Lua::new();
        let table = new_array(&lua).unwrap();
        assert_eq!(table_to_json(&lua, &table).unwrap(), json!([]));
        table.raw_set("key", 1).unwrap();
        assert_eq!(table_to_json(&lua, &table).unwrap(), json!({"key": 1}));
    }

    #[test]
    fn json_round_trips_with_preserved_nulls_and_containers() {
        let lua = Lua::new();
        for json in [
            json!(null),
            json!([null, {}, [], null]),
            json!({"content": null, "summary": [], "meta": {"items": [null, {}, [], [null, true]]}}),
        ] {
            let value = from_json_preserving_nulls(&lua, &json).unwrap();
            assert_eq!(to_json(&lua, &value).unwrap(), json);
        }
    }

    #[test]
    fn plain_projection_maps_null_to_nil() {
        let lua = Lua::new();
        let Value::Table(table) = from_json(&lua, &json!({"a": null, "b": [null]})).unwrap() else {
            panic!("expected a table");
        };
        assert!(table.raw_get::<Value>("a").unwrap().is_nil());
        assert_eq!(table.raw_len(), 0);
    }

    #[test]
    fn cycles_fail_without_recursing() {
        let lua = Lua::new();
        let text = message(encode(&lua, "local t = {} t.self = t return t"));
        assert!(text.contains("table cycle"), "{text}");
        let text = message(encode(
            &lua,
            "local a, b = {}, {} a[1] = b b[1] = a return a",
        ));
        assert!(text.contains("table cycle"), "{text}");
    }

    #[test]
    fn shared_subtrees_are_not_cycles() {
        let lua = Lua::new();
        let json = encode(&lua, "local s = {1} return {s, s}").unwrap();
        assert_eq!(json, json!([[1], [1]]));
    }

    #[test]
    fn deep_nesting_fails_before_exhausting_the_stack() {
        let lua = Lua::new();
        let text = message(encode(
            &lua,
            "local t = {} local c = t for _ = 1, 100000 do local n = {} c[1] = n c = n end return t",
        ));
        assert!(text.contains("level depth limit"), "{text}");

        let mut json = json!(0);
        for _ in 0..1000 {
            json = json!([json]);
        }
        let text = from_json(&lua, &json).unwrap_err().to_string();
        assert!(text.contains("level depth limit"), "{text}");
    }

    #[test]
    fn unrepresentable_values_are_errors() {
        let lua = Lua::new();
        for code in [
            "return {x = 0/0}",
            "return {x = math.huge}",
            "return {x = function() end}",
            "return {x = '\\255'}",
            "return {['\\255'] = 1}",
            "return {[true] = 1}",
            "return {[1] = 'a', ['1'] = 'b', x = 1}",
        ] {
            assert!(encode(&lua, code).is_err(), "{code}");
        }
        assert_eq!(
            encode(&lua, "return {x = 1, y = 1.5}").unwrap(),
            json!({"x": 1, "y": 1.5})
        );
    }

    #[test]
    fn null_sentinel_encodes_as_json_null() {
        let lua = Lua::new();
        lua.globals().set("null", Value::NULL).unwrap();
        assert_eq!(
            encode(&lua, "return {null, a = null}").unwrap(),
            json!({"1": null, "a": null})
        );
    }

    #[test]
    fn node_and_byte_budgets_are_enforced() {
        let lua = Lua::new();
        let code = format!(
            "return string.rep('x', {})",
            protocol::TOOL_METADATA_MAX_BYTES + 1
        );
        let value = eval(&lua, &code);
        assert!(tool_metadata(&lua, &value, &[])
            .unwrap_err()
            .contains("exceeds the"));
        assert!(to_json(&lua, &value).is_ok());

        let wide = eval(
            &lua,
            &format!(
                "local t = {{}} for i = 1, {} do t[i] = i end return t",
                protocol::TOOL_METADATA_MAX_NODES + 1
            ),
        );
        assert!(tool_metadata(&lua, &wide, &[])
            .unwrap_err()
            .contains("node limit"));
    }

    #[test]
    fn metadata_leaves_out_ignored_root_keys_only() {
        let lua = Lua::new();
        let value = eval(&lua, "return {skip = 'x', keep = {skip = 'y'}}");
        assert_eq!(
            tool_metadata(&lua, &value, &["skip"]).unwrap(),
            json!({"keep": {"skip": "y"}})
        );
    }

    #[test]
    fn argument_records_reject_arrays() {
        let lua = Lua::new();
        let Value::Table(array) = eval(&lua, "return {1, 2}") else {
            panic!("expected a table");
        };
        assert!(table_to_object(&lua, &array).is_err());
        let Value::Table(record) = eval(&lua, "return {path = 'a'}") else {
            panic!("expected a table");
        };
        assert_eq!(table_to_object(&lua, &record).unwrap()["path"], json!("a"));
    }
}
