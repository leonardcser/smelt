//! `smelt.markdown.render(buf_id, source)` — paint markdown into a
//! Buffer using the same renderer the transcript uses for assistant
//! text blocks.

use crate::content::to_buffer::render_into_buffer;
use crate::ui::BufId;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let md = lua.create_table()?;
    md.set(
        "render",
        lua.create_function(|_, (buf_id, source): (u64, String)| {
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        smelt_core::transcript_present::render_markdown_inner(
                            sink,
                            &source,
                            width as usize,
                            "",
                            false,
                            None,
                        );
                    });
                }
            });
            Ok(())
        })?,
    )?;
    smelt.set("markdown", md)?;
    Ok(())
}
