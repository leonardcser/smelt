//! `smelt.prompt` bindings - the main editable input surface.
//!
//! `win()` returns a `Win` userdata for the prompt input so plugins can
//! bind keys / events via the chainable handle API. `text()` snapshots
//! the current buffer; `set_text(s)` replaces it.

use mlua::prelude::*;
use smelt_buffer::attachment::ATTACHMENT_MARKER;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "prompt",
        "The main editable input surface: win handle, text get/set, and cursor control. UiHost-only.",
        Tier::UiHost,
    )?;
    m.fn_(
        "win",
        "Return a `Win` handle for the prompt input. Use `win:key(...)` and `win:on(...)` to attach plugin behaviour.",
        &[],
        |_, ()| Ok(super::win::LuaWin { id: crate::app::PROMPT_WIN }),
    )?;
    m.fn_(
        "text",
        "Return the prompt input buffer's current text. Internal attachment markers are stripped - plugins see only the user-visible characters.",
        &[],
        |_, ()| {
            Ok(crate::lua::try_with_app(|app| {
                app.prompt_buf().source().replace(ATTACHMENT_MARKER, "")
            })
            .unwrap_or_default())
        },
    )?;
    m.fn_(
        "set_text",
        "Replace the prompt buffer with `text`. The cursor lands at the end and undo state is reset.",
        &["text"],
        |_, text: String| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let mut pctx = crate::input::prompt_ctx_mut(&mut app.ui);
                app.input.replace_text(&mut pctx, text);
            });
            Ok(())
        },
    )?;
    m.fn_(
        "cursor",
        "Read or write the prompt cursor as a byte offset into `text()`. Without an argument returns the current offset; with one snaps it to a char boundary and clamps to source length. Returns the resulting offset.",
        &["pos"],
        |_, pos: Option<i64>| -> LuaResult<i64> {
            Ok(crate::lua::with_app(|app| match pos {
                Some(p) => {
                    let requested = p.max(0) as usize;
                    let snapped = {
                        let pctx = crate::input::prompt_ctx_mut(&mut app.ui);
                        let snapped = smelt_buffer::text::snap(pctx.buf.source(), requested);
                        pctx.win.set_cpos(snapped);
                        pctx.win.clear_selection_anchor();
                        pctx.win.clamp_anchors_to_source(pctx.buf.source());
                        snapped
                    };
                    snapped as i64
                }
                None => app.prompt_win().cpos() as i64,
            }))
        },
    )?;
    m.fn_(
        "replace_range",
        "UTF-8-safe replace of the byte range `[start, end)` in the prompt with `text`. Endpoints are snapped to char boundaries and clamped to source length. The cursor lands at `start + #text`. Returns the new cursor offset.",
        &["start", "end", "text"],
        |_, (start, end, text): (i64, i64, String)| -> LuaResult<i64> {
            Ok(crate::lua::with_app(|app| {
                let new_cpos = {
                    let pctx = crate::input::prompt_ctx_mut(&mut app.ui);
                    let src = pctx.buf.source();
                    let start = smelt_buffer::text::snap(src, start.max(0) as usize);
                    let end = smelt_buffer::text::snap(src, end.max(0) as usize).max(start);
                    pctx.buf.text_mut().replace_range(start..end, &text);
                    let new_cpos = start + text.len();
                    pctx.win.set_cpos(new_cpos);
                    pctx.win.clear_selection_anchor();
                    pctx.win.clamp_anchors_to_source(pctx.buf.source());
                    new_cpos
                };
                new_cpos as i64
            }))
        },
    )?;
    m.fn_(
        "set_section",
        "Set the named prompt section (e.g. selection context, attached files) to `content`. Sections render above the editable text and are submitted with the next turn.",
        &["name", "content"],
        |_, (name, content): (String, String)| -> LuaResult<()> {
            crate::lua::with_app(|app| app.prompt_sections.set(&name, content));
            Ok(())
        },
    )?;
    m.fn_(
        "remove_section",
        "Remove the named prompt section. No-op if the section does not exist.",
        &["name"],
        |_, name: String| -> LuaResult<()> {
            crate::lua::with_app(|app| app.prompt_sections.remove(&name));
            Ok(())
        },
    )?;
    m.fn_(
        "queued",
        "Return the queued prompt text rows. Empty when the prompt is idle and no active turn, compaction, or busy work is in flight. The top-bar renderer reads this each frame to surface waiting messages above the input.",
        &[],
        |_, ()| -> LuaResult<Vec<String>> {
            Ok(crate::lua::try_with_app(|app| {
                if app.prompt_input_is_busy() {
                    app.queued_inputs.display_texts()
                } else {
                    Vec::new()
                }
            })
            .unwrap_or_default())
        },
    )?;
    m.fn_(
        "queued_rows",
        "Return queued prompt rows as `{ text, kind }` tables. `kind` is `request` for rows added to the current turn's next request, or `turn` for rows waiting for the next turn.",
        &[],
        |lua, ()| -> LuaResult<Vec<mlua::Table>> {
            let rows = crate::lua::try_with_app(|app| {
                if app.prompt_input_is_busy() {
                    app.queued_inputs.display_rows()
                } else {
                    Vec::new()
                }
            })
            .unwrap_or_default();
            rows.into_iter()
                .map(|row| {
                    let t = lua.create_table()?;
                    t.set("text", row.text)?;
                    t.set("kind", row.stage.as_str())?;
                    Ok(t)
                })
                .collect()
        },
    )?;
    m.fn_(
        "has_stash",
        "Return whether the prompt currently holds a stashed input snapshot (Ctrl+S). The top-bar renderer uses this to surface a `◌ Stashed (ctrl+s to unstash)` row.",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::lua::try_with_app(|app| app.input.stash.is_some()).unwrap_or(false))
        },
    )?;
    Ok(())
}
