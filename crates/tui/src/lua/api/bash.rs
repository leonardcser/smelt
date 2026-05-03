//! `smelt.bash.render(buf_id, command)` — paint bash highlighting into
//! a Buffer the caller owns. One-line-per-line, leading-space gutter.

use crate::term::content::highlight::BashHighlighter;
use crate::term::content::to_buffer::render_into_buffer;
use mlua::prelude::*;
use ui::BufId;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let bash = lua.create_table()?;
    bash.set(
        "render",
        lua.create_function(|_, (buf_id, command): (u64, String)| {
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::term::content::term_width() as u16;
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
    smelt.set("bash", bash)?;
    Ok(())
}
