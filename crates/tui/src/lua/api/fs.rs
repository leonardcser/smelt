//! `smelt.fs` bindings — sync filesystem primitives over `tui::fs`.
//! Host-tier (works in tui and headless) — no Ui touch.
//!
//! Errors flow through the `(value, err)` Lua convention: success
//! returns `(value, nil)`, failure returns `(nil, error_string)`. This
//! lets plugin code do `local data, err = smelt.fs.read(p)` without
//! `pcall`.

use mlua::prelude::*;
use std::path::PathBuf;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let fs = lua.create_table()?;

    fs.set(
        "read",
        lua.create_function(|_, p: String| match crate::fs::read_to_string(&p) {
            Ok(s) => Ok((Some(s), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "write",
        lua.create_function(|_, (p, contents): (String, mlua::String)| {
            match crate::fs::write(&p, contents.as_bytes()) {
                Ok(()) => Ok((true, None)),
                Err(err) => Ok((false, Some(err.to_string()))),
            }
        })?,
    )?;

    fs.set(
        "exists",
        lua.create_function(|_, p: String| Ok(crate::fs::exists(&p)))?,
    )?;

    fs.set(
        "is_file",
        lua.create_function(|_, p: String| Ok(crate::fs::is_file(&p)))?,
    )?;

    fs.set(
        "is_dir",
        lua.create_function(|_, p: String| Ok(crate::fs::is_dir(&p)))?,
    )?;

    fs.set(
        "read_dir",
        lua.create_function(|_, p: String| match crate::fs::read_dir(&p) {
            Ok(entries) => Ok((Some(paths_to_strings(entries)), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "mkdir",
        lua.create_function(|_, p: String| match crate::fs::mkdir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "mkdir_all",
        lua.create_function(|_, p: String| match crate::fs::mkdir_all(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "remove_file",
        lua.create_function(|_, p: String| match crate::fs::remove_file(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "remove_dir",
        lua.create_function(|_, p: String| match crate::fs::remove_dir(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "remove_dir_all",
        lua.create_function(|_, p: String| match crate::fs::remove_dir_all(&p) {
            Ok(()) => Ok((true, None)),
            Err(err) => Ok((false, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "rename",
        lua.create_function(|_, (from, to): (String, String)| {
            match crate::fs::rename(&from, &to) {
                Ok(()) => Ok((true, None)),
                Err(err) => Ok((false, Some(err.to_string()))),
            }
        })?,
    )?;

    fs.set(
        "copy",
        lua.create_function(
            |_, (from, to): (String, String)| match crate::fs::copy(&from, &to) {
                Ok(n) => Ok((Some(n), None)),
                Err(err) => Ok((None, Some(err.to_string()))),
            },
        )?,
    )?;

    fs.set(
        "mtime",
        lua.create_function(|_, p: String| match crate::fs::mtime_secs(&p) {
            Ok(value) => Ok((value, None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    fs.set(
        "size",
        lua.create_function(|_, p: String| match crate::fs::size(&p) {
            Ok(n) => Ok((Some(n), None)),
            Err(err) => Ok((None, Some(err.to_string()))),
        })?,
    )?;

    smelt.set("fs", fs)?;
    Ok(())
}

fn paths_to_strings(paths: Vec<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}
