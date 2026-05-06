//! `smelt.buf` bindings — Buffer creation, line/source mutation,
//! highlight extmarks. UiHost-only.

use super::{app_read, theme_role_get};
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::Arc;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let buf_tbl = lua.create_table()?;
    buf_tbl.set("text", app_read!(lua, |app| app.input.win.text.clone()))?;
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
                        crate::ui::BufId(id),
                        crate::ui::BufCreateOpts::default(),
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
                if let Some(buf) = app.ui.buf_mut(crate::ui::BufId(id)) {
                    buf.set_all_lines(lines);
                }
            });
            Ok(())
        })?,
    )?;
    // `smelt.buf.get_line(buf_id, line_idx)` — line_idx is
    // 1-based to match every other Lua-facing line index in the
    // codebase. Returns `nil` when out of range.
    buf_tbl.set(
        "get_line",
        lua.create_function(|_, (id, line_idx): (u64, u64)| {
            let line0 = match line_idx.checked_sub(1) {
                Some(n) => n as usize,
                None => return Ok(None),
            };
            let text = crate::lua::with_app(|app| {
                app.ui
                    .buf(crate::ui::BufId(id))
                    .and_then(|b| b.get_line(line0).map(|s| s.to_string()))
            });
            Ok(text)
        })?,
    )?;
    buf_tbl.set(
        "set_source",
        lua.create_function(|_, (id, source): (u64, String)| {
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(crate::ui::BufId(id)) {
                    buf.set_source(source);
                }
            });
            Ok(())
        })?,
    )?;
    buf_tbl.set("set_extmark", lua.create_function(set_extmark)?)?;
    buf_tbl.set(
        "create_namespace",
        lua.create_function(|_, name: String| Ok(smelt_core::buffer::create_namespace(&name).0))?,
    )?;
    smelt.set("buf", buf_tbl)?;
    Ok(())
}

/// `smelt.buf.set_extmark(buf, ns, row, col, opts) -> extmark_id`.
/// Mirrors `nvim_buf_set_extmark`'s keyset. `row` is 1-based to
/// match every other Lua row index in smelt; convert to 0-based
/// internally. `opts.id` retargets an existing mark across re-runs.
///
/// Highlight payload: pick whichever of `hl_group` (theme name),
/// or per-attribute `fg / bg / bold / dim / italic` is set.
/// VirtText payload: pass `virt_text` (and optionally
/// `virt_text_pos`).
fn set_extmark(
    lua: &Lua,
    (id, ns, row, col, opts): (u64, u32, u64, u64, Option<mlua::Table>),
) -> LuaResult<u64> {
    use crate::ui::BufId;
    use smelt_core::buffer::{ExtmarkId, ExtmarkOpts, NsId};

    let Some(row0) = row.checked_sub(1) else {
        return Ok(0);
    };
    let row0 = row0 as usize;
    let col0 = col as usize;

    let opts_tbl = match opts {
        Some(t) => t,
        None => lua.create_table()?,
    };

    let end_row: Option<usize> = opts_tbl
        .get::<Option<u64>>("end_row")?
        .and_then(|n| n.checked_sub(1).map(|x| x as usize));
    let end_col: Option<usize> = opts_tbl.get::<Option<u64>>("end_col")?.map(|n| n as usize);
    let priority: u32 = opts_tbl.get::<Option<u32>>("priority")?.unwrap_or(0);
    let right_gravity: bool = opts_tbl
        .get::<Option<bool>>("right_gravity")?
        .unwrap_or(true);
    let end_right_gravity: bool = opts_tbl
        .get::<Option<bool>>("end_right_gravity")?
        .unwrap_or(false);
    let mark_id: Option<ExtmarkId> = opts_tbl.get::<Option<u32>>("id")?.map(ExtmarkId);

    let virt_text: Option<String> = opts_tbl.get::<Option<String>>("virt_text")?;

    let mut payload_opts = if let Some(text) = virt_text {
        let hl_group: Option<String> = opts_tbl.get::<Option<String>>("virt_text_hl")?;
        let mut o = ExtmarkOpts::virt_text(text, hl_group);
        if let Some(pos) = opts_tbl.get::<Option<String>>("virt_text_pos")? {
            o = o.with_virt_pos(parse_virt_pos(&pos));
        }
        o
    } else {
        let style = parse_highlight_style(&opts_tbl)?;
        let meta = parse_meta(&opts_tbl)?;
        let mut o = ExtmarkOpts::highlight(end_col.unwrap_or(col0), style, meta);
        if let Some(true) = opts_tbl.get::<Option<bool>>("hl_eol")? {
            o = o.with_hl_eol(true);
        }
        o
    };

    payload_opts.end_row = end_row;
    if !matches!(
        payload_opts.payload,
        smelt_core::buffer::ExtmarkPayload::Highlight { .. }
    ) {
        payload_opts.end_col = end_col;
    }
    payload_opts.priority = priority;
    payload_opts.right_gravity = right_gravity;
    payload_opts.end_right_gravity = end_right_gravity;
    payload_opts.id = mark_id;

    let new_id = crate::lua::with_app(|app| {
        app.ui
            .buf_mut(BufId(id))
            .map(|buf| buf.set_extmark(NsId(ns), row0, col0, payload_opts))
    })
    .map(|eid: ExtmarkId| eid.0 as u64)
    .unwrap_or(0);
    Ok(new_id)
}

fn parse_virt_pos(s: &str) -> smelt_core::buffer::VirtTextPos {
    use smelt_core::buffer::VirtTextPos;
    match s {
        "inline" => VirtTextPos::Inline,
        "overlay" => VirtTextPos::Overlay,
        "right_align" => VirtTextPos::RightAlign,
        _ => VirtTextPos::Eol,
    }
}

fn parse_highlight_style(t: &mlua::Table) -> LuaResult<crate::ui::SpanStyle> {
    let resolve_role = |role: &str| -> LuaResult<smelt_core::style::Color> {
        crate::lua::with_app(|app| theme_role_get(app.ui.theme(), role))
            .ok_or_else(|| LuaError::RuntimeError(format!("unknown theme role: {role}")))
    };
    // hl_group sets fg by name (today's theme groups carry fg
    // primarily). Per-attribute fields override individual axes.
    let fg = match t.get::<Option<String>>("fg").ok().flatten() {
        Some(role) => Some(resolve_role(&role)?),
        None => match t.get::<Option<String>>("hl_group").ok().flatten() {
            Some(role) => Some(resolve_role(&role)?),
            None => None,
        },
    };
    let bg = match t.get::<Option<String>>("bg").ok().flatten() {
        Some(role) => Some(resolve_role(&role)?),
        None => None,
    };
    Ok(crate::ui::SpanStyle {
        fg,
        bg,
        bold: t.get::<bool>("bold").unwrap_or(false),
        dim: t.get::<bool>("dim").unwrap_or(false),
        italic: t.get::<bool>("italic").unwrap_or(false),
        ..Default::default()
    })
}

fn parse_meta(t: &mlua::Table) -> LuaResult<smelt_core::buffer::SpanMeta> {
    use smelt_core::buffer::SpanMeta;
    Ok(SpanMeta {
        selectable: t.get::<Option<bool>>("selectable")?.unwrap_or(true),
        copy_as: t.get::<Option<String>>("yank_as")?,
    })
}
