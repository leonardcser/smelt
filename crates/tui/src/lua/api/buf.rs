//! `smelt.buf` bindings — Buffer creation, line/source mutation,
//! highlight extmarks. UiHost-only.

use super::app_read;
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
                let readonly: bool = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("readonly").ok())
                    .unwrap_or(false);
                let id = s
                    .next_buf_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::lua::with_app(|app| {
                    match app.ui.buf_create_with_id(
                        crate::ui::BufId(id),
                        crate::ui::BufCreateOpts::default(),
                    ) {
                        Ok(bid) => {
                            if let Some(buf) = app.ui.buf_mut(bid) {
                                buf.readonly = readonly;
                                if let Some(fmt) = format {
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
        "set_readonly",
        lua.create_function(|_, (id, ro): (u64, bool)| {
            crate::lua::with_app(|app| {
                if let Some(buf) = app.ui.buf_mut(crate::ui::BufId(id)) {
                    buf.readonly = ro;
                }
            });
            Ok(())
        })?,
    )?;
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
    // `smelt.buf.clear_namespace(buf, ns, line_start?, line_end?)` —
    // drops every extmark in `ns` between `[line_start, line_end)`
    // (1-based, inclusive start, exclusive end). Defaults clear the
    // whole buffer so plugins that repaint a namespace each tick
    // (perf panel, completer ghost text) don't have to track ids.
    buf_tbl.set(
        "clear_namespace",
        lua.create_function(
            |_, (id, ns, start, end_): (u64, u32, Option<i64>, Option<i64>)| {
                use smelt_core::buffer::NsId;
                let start_line = match start {
                    Some(n) if n > 0 => (n as usize).saturating_sub(1),
                    _ => 0,
                };
                let end_line = match end_ {
                    Some(n) if n > 0 => n as usize,
                    _ => usize::MAX,
                };
                crate::lua::with_app(|app| {
                    if let Some(buf) = app.ui.buf_mut(crate::ui::BufId(id)) {
                        buf.clear_namespace(NsId(ns), start_line, end_line);
                    }
                });
                Ok(())
            },
        )?,
    )?;
    smelt.set("buf", buf_tbl)?;
    Ok(())
}

/// `smelt.buf.set_extmark(buf, ns, row, col, opts) -> extmark_id`.
/// Mirrors `nvim_buf_set_extmark`'s keyset. `row` is 1-based to
/// match every other Lua row index in smelt; convert to 0-based
/// internally. `opts.id` retargets an existing mark across re-runs.
///
/// Highlight payload: `hl_group` names a theme highlight group whose
/// full Style is applied; `fg` / `bg` name groups whose `.fg` / `.bg`
/// axis is pulled in to override; `bold / dim / italic` override
/// individual attribute axes. Unknown group names silently resolve
/// to default (nvim policy). VirtText payload: pass `virt_text` (and
/// optionally `virt_text_pos`).
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
    use smelt_core::style::Style;

    // Highlight groups are looked up via `theme.get(name)` (nvim
    // parity): unknown names silently resolve to default rather than
    // erroring, so a stale theme reference paints unstyled instead of
    // crashing the caller. `hl_group` sets the full base Style;
    // `fg` / `bg` strings name groups whose `.fg` / `.bg` axis is
    // pulled in. Per-attribute Lua bools override individual axes.
    let resolve_group =
        |name: &str| -> Style { crate::lua::with_app(|app| app.ui.theme().get(name)) };

    let mut style = match t.get::<Option<String>>("hl_group").ok().flatten() {
        Some(name) => resolve_group(&name),
        None => Style::default(),
    };
    if let Some(name) = t.get::<Option<String>>("fg").ok().flatten() {
        style.fg = resolve_group(&name).fg;
    }
    if let Some(name) = t.get::<Option<String>>("bg").ok().flatten() {
        style.bg = resolve_group(&name).bg;
    }
    if let Some(b) = t.get::<Option<bool>>("bold")? {
        style.bold = b;
    }
    if let Some(b) = t.get::<Option<bool>>("dim")? {
        style.dim = b;
    }
    if let Some(b) = t.get::<Option<bool>>("italic")? {
        style.italic = b;
    }
    Ok(style)
}

fn parse_meta(t: &mlua::Table) -> LuaResult<smelt_core::buffer::SpanMeta> {
    use smelt_core::buffer::SpanMeta;
    Ok(SpanMeta {
        selectable: t.get::<Option<bool>>("selectable")?.unwrap_or(true),
        copy_as: t.get::<Option<String>>("yank_as")?,
    })
}
