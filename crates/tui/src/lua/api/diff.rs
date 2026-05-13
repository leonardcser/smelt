//! `smelt.diff.render(buf_id, { old, new, path })` — paint an inline
//! diff into a Buffer the caller owns. Same pipeline the built-in
//! confirm dialog uses.

use crate::content::highlight::print_inline_diff;
use crate::content::to_buffer::render_into_buffer;
use crate::smelt_term::BufId;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.diff",
    doc = "Paint an inline diff into a Buffer. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let diff = lua.create_table()?;
    register_ui_fn(
        &diff,
        "smelt.diff",
        "render",
        "Paint an inline diff between `opts.old` and `opts.new` into the buffer, syntax-highlighted by `opts.path`'s extension. Mirrors the pipeline used by the built-in confirm dialog.",
        &["buf_id", "opts"],
        lua,
        |_, (buf_id, opts): (u64, mlua::Table)|  -> LuaResult<()>{
            let old: String = opts.get::<Option<String>>("old")?.unwrap_or_default();
            let new: String = opts.get::<Option<String>>("new")?.unwrap_or_default();
            let path: String = opts.get::<Option<String>>("path")?.unwrap_or_default();
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        print_inline_diff(sink, &old, &new, &path, &old, 0, u16::MAX);
                    });
                }
            });
            Ok(())
        },
    )?;
    smelt.set("diff", diff)?;
    Ok(())
}
