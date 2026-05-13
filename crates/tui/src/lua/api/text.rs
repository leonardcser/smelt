//! `smelt.text` bindings — visual-width measurement + dim/error
//! body rendering into a Buffer using the same wrapping that the
//! built-in tool render path uses for tool output.

use crate::content::to_buffer::render_into_buffer;
use crate::smelt_term::BufId;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::content::wrap::wrap_line;
use smelt_core::lua::doc::register_ui_fn;
use smelt_core::theme::role_hl;
use unicode_width::UnicodeWidthStr;

#[lua_module(
    name = "smelt.text",
    doc = "Visual-width measurement and dim/error body rendering into a Buffer. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let text = lua.create_table()?;
    register_ui_fn(
        &text,
        "smelt.text",
        "width",
        "Return the visual column count of `s`. Lua's `#s` counts bytes; use this for sizing extmark ranges or computing column offsets so multi-byte and wide characters land correctly.",
        &["s"],
        lua,
        |_, s: String| Ok(UnicodeWidthStr::width(s.as_str()) as u64),
    )?;

    register_ui_fn(
        &text,
        "smelt.text",
        "render",
        "Paint plain text into a buffer with the dim/error body styling that the built-in tool render path uses. `opts.is_error = true` switches to the error-message highlight group.",
        &["buf", "content", "opts"],
        lua,
        |_, (buf_id, content, opts): (u64, String, Option<mlua::Table>)|  -> LuaResult<()>{
            let is_error = opts
                .as_ref()
                .and_then(|t| t.get::<Option<bool>>("is_error").ok().flatten())
                .unwrap_or(false);
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        let max_cols = (width as usize).saturating_sub(3);
                        for line in content.lines() {
                            let expanded = line.replace('\t', "    ");
                            let segs = wrap_line(&expanded, max_cols);
                            if segs.len() > 1 {
                                sink.mark_wrapped();
                            }
                            for seg in &segs {
                                if is_error {
                                    sink.push_hl(role_hl("ErrorMsg"));
                                    sink.print(&format!("  {}", seg));
                                    sink.pop_style();
                                } else {
                                    sink.push_dim();
                                    sink.print(&format!("  {}", seg));
                                    sink.pop_style();
                                }
                                sink.newline();
                            }
                        }
                    });
                }
            });
            Ok(())
        },
    )?;

    smelt.set("text", text)?;
    Ok(())
}
