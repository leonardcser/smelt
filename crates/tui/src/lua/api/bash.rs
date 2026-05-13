//! `smelt.bash.render(buf_id, command)` — paint bash highlighting into
//! a Buffer the caller owns. One-line-per-line, leading-space gutter.
//!
//! `smelt.bash.render_line(buf_id, line)` — paint one line of bash into
//! row 0 of the buffer with no leading gutter. Used by tool
//! `render_summary` callbacks: the host hands the tool a scratch Buffer
//! per wrapped summary line and replays row 0 inline.

use crate::content::highlight::BashHighlighter;
use crate::content::to_buffer::render_into_buffer;
use crate::smelt_term::BufId;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let bash = lua.create_table()?;
    record_module_doc(
        "smelt.bash",
        "Paint bash syntax highlighting into a Buffer. UiHost-only.",
    );

    register_ui_fn(
        &bash,
        "smelt.bash",
        "render",
        "Paint syntax-highlighted bash into the buffer, one source line per row with a single-space leading gutter. Used to render multi-line shell commands inside tool output.",
        &["buf_id", "command"],
        lua,
        |_, (buf_id, command): (u64, String)|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        let mut bh = BashHighlighter::new();
                        for line in command.lines() {
                            sink.print(" ");
                            bh.print_line(sink, line);
                            sink.newline();
                        }
                    });
                }
            });
            Ok(())
        },
    )?;
    register_ui_fn(
        &bash,
        "smelt.bash",
        "render_line",
        "Paint one bash line into row 0 of the buffer with no leading gutter. Used by tool `render_summary` callbacks where the host supplies a scratch buffer per wrapped summary line.",
        &["buf_id", "line"],
        lua,
        |_, (buf_id, line): (u64, String)|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        let mut bh = BashHighlighter::new();
                        bh.print_line(sink, &line);
                        sink.newline();
                    });
                }
            });
            Ok(())
        },
    )?;
    smelt.set("bash", bash)?;
    Ok(())
}
