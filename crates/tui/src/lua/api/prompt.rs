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
                let trace = app.prompt_trace_enabled();
                let new_len = text.len();
                let new_hash = trace.then(|| crate::app::TuiApp::prompt_text_hash(&text));
                if trace {
                    app.trace_prompt_event(
                        "lua_prompt_set_text_before",
                        serde_json::json!({ "new_len": new_len, "new_hash": new_hash }),
                    );
                }
                {
                    let mut pctx = crate::input::prompt_ctx_mut(&mut app.ui);
                    app.input.replace_text(&mut pctx, text);
                }
                if trace {
                    app.trace_prompt_event(
                        "lua_prompt_set_text_after",
                        serde_json::json!({ "new_len": new_len, "new_hash": new_hash }),
                    );
                }
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
                    let before_cpos = app.prompt_win().cpos();
                    let snapped = {
                        let pctx = crate::input::prompt_ctx_mut(&mut app.ui);
                        let snapped = smelt_buffer::text::snap(pctx.buf.source(), requested);
                        pctx.win.set_cpos(snapped);
                        pctx.win.clear_selection_anchor();
                        pctx.win.clamp_anchors_to_source(pctx.buf.source());
                        snapped
                    };
                    if app.prompt_trace_enabled() {
                        app.trace_prompt_event(
                            "lua_prompt_cursor_set",
                            serde_json::json!({
                                "requested": requested,
                                "before_cpos": before_cpos,
                                "after_cpos": snapped,
                            }),
                        );
                    }
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
                let before_cpos = app.prompt_win().cpos();
                let (start, end, new_cpos) = {
                    let pctx = crate::input::prompt_ctx_mut(&mut app.ui);
                    let src = pctx.buf.source();
                    let start = smelt_buffer::text::snap(src, start.max(0) as usize);
                    let end = smelt_buffer::text::snap(src, end.max(0) as usize).max(start);
                    pctx.buf.text_mut().replace_range(start..end, &text);
                    let new_cpos = start + text.len();
                    pctx.win.set_cpos(new_cpos);
                    pctx.win.clear_selection_anchor();
                    pctx.win.clamp_anchors_to_source(pctx.buf.source());
                    (start, end, new_cpos)
                };
                if app.prompt_trace_enabled() {
                    app.trace_prompt_event(
                        "lua_prompt_replace_range",
                        serde_json::json!({
                            "start": start,
                            "end": end,
                            "inserted_len": text.len(),
                            "inserted_hash": crate::app::TuiApp::prompt_text_hash(&text),
                            "before_cpos": before_cpos,
                            "after_cpos": new_cpos,
                        }),
                    );
                }
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
        "Return the array of messages currently queued behind the active turn. Empty when the agent is idle and no busy work is in flight. The top-bar renderer reads this each frame to surface waiting messages above the input.",
        &[],
        |_, ()| -> LuaResult<Vec<String>> {
            Ok(crate::lua::try_with_app(|app| {
                let agent_running = app.agent_is_running();
                let show_queued = agent_running || app.busy_stack.is_busy();
                if show_queued {
                    app.queued_inputs
                        .iter()
                        .map(crate::app::QueuedInput::display)
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            })
            .unwrap_or_default())
        },
    )?;
    m.fn_(
        "has_stash",
        "Return whether the prompt currently holds a stashed input snapshot (Ctrl+S). The top-bar renderer uses this to surface a `» Stashed (ctrl+s to unstash)` row.",
        &[],
        |_, ()| -> LuaResult<bool> {
            Ok(crate::lua::try_with_app(|app| app.input.stash.is_some()).unwrap_or(false))
        },
    )?;
    Ok(())
}
