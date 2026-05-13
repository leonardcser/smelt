//! `smelt.markdown.render(buf_id, source)` — paint markdown into a
//! Buffer using the same renderer the transcript uses for assistant
//! text blocks.

use crate::content::to_buffer::render_into_buffer;
use crate::smelt_term::BufId;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let md = lua.create_table()?;
    record_module_doc(
        "smelt.markdown",
        "Paint markdown into a Buffer. UiHost-only.",
    );

    register_ui_fn(
        &md,
        "smelt.markdown",
        "render",
        "Render markdown `source` into the buffer using the same renderer the transcript uses for assistant text blocks (headings, lists, code fences, inline emphasis).",
        &["buf_id", "source"],
        lua,
        |_, (buf_id, source): (u64, String)|  -> LuaResult<()>{
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        crate::content::transcript_parsers::render_markdown_inner(
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
        },
    )?;
    smelt.set("markdown", md)?;
    Ok(())
}
