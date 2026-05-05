//! `smelt.bash.render(buf_id, command)` — paint bash highlighting into
//! a Buffer the caller owns. One-line-per-line, leading-space gutter.
//!
//! `smelt.bash.render_line(buf_id, line)` — paint one line of bash into
//! row 0 of the buffer with no leading gutter. Used by tool
//! `render_summary` callbacks: the host hands the tool a scratch Buffer
//! per wrapped summary line and replays row 0 inline.

use crate::content::highlight::BashHighlighter;
use crate::content::to_buffer::render_into_buffer;
use crate::ui::BufId;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let bash = lua.create_table()?;
    bash.set(
        "render",
        lua.create_function(|_, (buf_id, command): (u64, String)| {
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
        })?,
    )?;
    bash.set(
        "render_line",
        lua.create_function(|_, (buf_id, line): (u64, String)| {
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
        })?,
    )?;
    smelt.set("bash", bash)?;
    Ok(())
}
