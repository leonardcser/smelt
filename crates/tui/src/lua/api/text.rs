//! `smelt.text.render(buf_id, content, opts?)` — paint plain text
//! into a Buffer with the same wrapping + dim/error styling that the
//! built-in tool render path uses for body output. `opts` accepts
//! `{ is_error = bool }`.

use crate::content::to_buffer::render_into_buffer;
use crate::ui::BufId;
use mlua::prelude::*;
use smelt_core::content::display::{ColorRole, ColorValue};
use smelt_core::content::wrap::wrap_line;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let text = lua.create_table()?;
    text.set(
        "render",
        lua.create_function(
            |_, (buf_id, content, opts): (u64, String, Option<mlua::Table>)| {
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
                                        sink.push_fg(ColorValue::Role(ColorRole::ErrorMsg));
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
        )?,
    )?;
    smelt.set("text", text)?;
    Ok(())
}
