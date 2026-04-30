//! `smelt.syntax.render(buf_id, { content, path })` — paint
//! syntect-highlit code into a Buffer the caller owns.

use crate::content::highlight::print_syntax_file;
use crate::content::to_buffer::render_into_buffer;
use mlua::prelude::*;
use ui::BufId;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let syntax = lua.create_table()?;
    syntax.set(
        "render",
        lua.create_function(|_, (buf_id, opts): (u64, mlua::Table)| {
            let content: String = opts.get::<Option<String>>("content")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        print_syntax_file(sink, &content, &path, 0, u16::MAX);
                    });
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("syntax", syntax)?;
    Ok(())
}
