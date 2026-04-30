//! `smelt.buf` bindings — Buffer creation, line/source mutation,
//! highlight extmarks. UiHost-only.

use super::{app_read, theme_role_get};
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let buf_tbl = lua.create_table()?;
    buf_tbl.set(
        "text",
        app_read!(lua, |app| app.input.win.edit_buf.buf.clone()),
    )?;
    {
        let s = shared.clone();
        buf_tbl.set(
            "create",
            lua.create_function(move |_, opts: Option<mlua::Table>| {
                let format = match opts.as_ref() {
                    Some(t) => match t.get::<Option<String>>("mode")? {
                        Some(mode) => Some(
                            crate::format::BufFormat::from_lua_spec(&mode, t)
                                .map_err(|e| LuaError::RuntimeError(format!("buf.create: {e}")))?,
                        ),
                        None => None,
                    },
                    None => None,
                };
                let id = s
                    .next_buf_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::lua::with_app(|app| {
                    match app.ui.buf_create_with_id(
                        ui::BufId(id),
                        ui::buffer::BufCreateOpts {
                            buftype: ui::buffer::BufType::Scratch,
                            ..Default::default()
                        },
                    ) {
                        Ok(bid) => {
                            if let Some(fmt) = format {
                                if let Some(buf) = app.ui.buf_mut(bid) {
                                    buf.set_parser(fmt.into_parser());
                                }
                            }
                        }
                        Err(clash) => {
                            app.notify_error(format!("buf.create: id {} already in use", clash.0));
                        }
                    }
                });
                Ok(id)
            })?,
        )?;
    }
    buf_tbl.set(
        "set_lines",
        lua.create_function(|_, (id, lines): (u64, mlua::Table)| {
            let lines: Vec<String> = lines
                .sequence_values::<String>()
                .filter_map(|v| v.ok())
                .collect();
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(ui::BufId(id)) {
                    buf.set_all_lines(lines);
                }
            });
            Ok(())
        })?,
    )?;
    // `smelt.buf.get_line(buf_id, line_idx)` — line_idx is
    // 1-based to match every other Lua-facing line index in the
    // codebase (`smelt.buf.add_highlight`, etc.). Returns `nil`
    // when out of range.
    buf_tbl.set(
        "get_line",
        lua.create_function(|_, (id, line_idx): (u64, u64)| {
            let line0 = match line_idx.checked_sub(1) {
                Some(n) => n as usize,
                None => return Ok(None),
            };
            let text = crate::lua::with_app(|app| {
                app.ui
                    .buf(ui::BufId(id))
                    .and_then(|b| b.get_line(line0).map(|s| s.to_string()))
            });
            Ok(text)
        })?,
    )?;
    buf_tbl.set(
        "set_source",
        lua.create_function(|_, (id, source): (u64, String)| {
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(ui::BufId(id)) {
                    buf.set_source(source);
                }
            });
            Ok(())
        })?,
    )?;
    buf_tbl.set(
        "add_highlight",
        lua.create_function(
            |_,
             (id, line, col_start, col_end, style): (
                u64,
                u64,
                u64,
                u64,
                Option<mlua::Table>,
            )| {
                let Some(line0) = line.checked_sub(1) else {
                    return Ok(());
                };
                if col_end <= col_start {
                    return Ok(());
                }
                let (fg, bold, italic, dim) = match style {
                    Some(t) => {
                        let fg = match t.get::<Option<String>>("fg").ok().flatten() {
                            Some(role) => Some(
                                crate::lua::with_app(|app| {
                                    theme_role_get(app.ui.theme(), &role)
                                })
                                .ok_or_else(|| {
                                    LuaError::RuntimeError(format!(
                                        "unknown theme role: {role}"
                                    ))
                                })?,
                            ),
                            None => None,
                        };
                        (
                            fg,
                            t.get::<bool>("bold").unwrap_or(false),
                            t.get::<bool>("italic").unwrap_or(false),
                            t.get::<bool>("dim").unwrap_or(false),
                        )
                    }
                    None => (None, false, false, false),
                };
                crate::lua::with_app(|app| {
                    if let Some(buf) = app.ui.buf_mut(ui::BufId(id)) {
                        if (line0 as usize) < buf.line_count() {
                            buf.add_highlight(
                                line0 as usize,
                                col_start.min(u16::MAX as u64) as u16,
                                col_end.min(u16::MAX as u64) as u16,
                                ui::buffer::SpanStyle {
                                    fg,
                                    bg: None,
                                    bold,
                                    dim,
                                    italic,
                                },
                            );
                        }
                    }
                });
                Ok(())
            },
        )?,
    )?;
    buf_tbl.set(
        "add_dim",
        lua.create_function(|_, (id, line, col_start, col_end): (u64, u64, u64, u64)| {
            let Some(line0) = line.checked_sub(1) else {
                return Ok(());
            };
            if col_end <= col_start {
                return Ok(());
            }
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(ui::BufId(id)) {
                    if (line0 as usize) < buf.line_count() {
                        buf.add_highlight(
                            line0 as usize,
                            col_start.min(u16::MAX as u64) as u16,
                            col_end.min(u16::MAX as u64) as u16,
                            ui::buffer::SpanStyle {
                                fg: None,
                                bg: None,
                                bold: false,
                                dim: true,
                                italic: false,
                            },
                        );
                    }
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("buf", buf_tbl)?;
    Ok(())
}
