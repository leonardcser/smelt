//! Strict option records, including readers shared by layered constructors.
use mlua::prelude::*;

pub fn validate_keys(table: &mlua::Table, keys: &[&str], context: &str) -> LuaResult<()> {
    for pair in table.clone().pairs::<mlua::Value, mlua::Value>() {
        let (key, _) = pair?;
        if !matches!(&key, mlua::Value::String(name) if keys.contains(&name.to_str()?.as_ref())) {
            return Err(LuaError::external(format!(
                "{context}: unknown option {key:?}"
            )));
        }
    }
    Ok(())
}

/// Consumes field names, not the caller's table. Layered constructors read
/// their fields in order and reject unconsumed keys before allocating anything.
pub struct LuaOptions {
    table: mlua::Table,
    keys: Vec<&'static str>,
    context: &'static str,
}

impl LuaOptions {
    pub fn new(table: mlua::Table, context: &'static str) -> Self {
        Self {
            table,
            keys: Vec::new(),
            context,
        }
    }

    pub fn get<T: FromLua>(&mut self, key: &'static str) -> LuaResult<T> {
        self.keys.push(key);
        self.table.get(key)
    }

    pub fn string(&mut self, key: &'static str) -> LuaResult<Option<String>> {
        match self.get::<mlua::Value>(key)? {
            mlua::Value::Nil => Ok(None),
            mlua::Value::String(value) => Ok(Some(value.to_str()?.to_string())),
            _ => Err(LuaError::external(format!(
                "{}.{key}: expected string",
                self.context
            ))),
        }
    }

    pub fn finish(self) -> LuaResult<()> {
        validate_keys(&self.table, &self.keys, self.context)
    }
}
