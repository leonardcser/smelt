//! `smelt.syntax.render(buf, { content, lang?, path? })` — multi-line block render
//! with no gutter and no line numbers. Indentation is the caller's responsibility
//! (panel `pad_left`, composed leading spaces, etc.).
//!
//! `smelt.syntax.render_file(buf, { content, lang?, path? })` — multi-line render
//! with the file-view layout (numbered gutter, indent).
//!
//! Inline single-span highlighting is handled by `smelt.buf.set_styled_lines`
//! spans with `syntax = "<lang>"`.

use crate::content::highlight::{print_code_lines, print_syntax_file, print_syntax_file_ext};
use crate::content::to_buffer::render_into_buffer;
use crate::smelt_term::BufId;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.syntax",
    doc = "Paint syntect-highlighted code into a Buffer. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let syntax = lua.create_table()?;
    register_ui_fn(
        &syntax,
        "smelt.syntax",
        "render",
        "Paint syntect-highlighted code from `opts.content` into the buffer as a plain block — one source line per row, no gutter, no line numbers. Indentation is the caller's responsibility. Pick the syntax via `opts.lang` (`\"bash\"`, `\"rust\"`, `\"py\"`, …) or `opts.path` (extension-sniffed); `lang` wins when both are set. Unknown languages fall back to plain text.",
        &["buf_id", "opts"],
        lua,
        |_, (buf_id, opts): (u64, mlua::Table)|  -> LuaResult<()>{
            let content: String = opts.get::<Option<String>>("content")?.unwrap_or_default();
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            let path: Option<String> = opts.get::<Option<String>>("path")?;
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        let lang_token = lang.as_deref().unwrap_or_else(|| {
                            path.as_deref()
                                .and_then(|p| std::path::Path::new(p).extension())
                                .and_then(|e| e.to_str())
                                .unwrap_or("")
                        });
                        print_code_lines(sink, &content, lang_token);
                    });
                }
            });
            Ok(())
        },
    )?;

    register_ui_fn(
        &syntax,
        "smelt.syntax",
        "render_file",
        "Paint `opts.content` into the buffer with file-view metadata: each row gets `SourceLine::Linear` stamped so a `gutter = \"line_numbers\"` window draws the gutter. Pick the syntax via `opts.lang` or `opts.path`. Use this for write_file / notebook diffs; prefer `smelt.syntax.render` for plain snippets.",
        &["buf_id", "opts"],
        lua,
        |_, (buf_id, opts): (u64, mlua::Table)|  -> LuaResult<()>{
            let content: String = opts.get::<Option<String>>("content")?.unwrap_or_default();
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            let path: Option<String> = opts.get::<Option<String>>("path")?;
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(BufId(buf_id)) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        match (lang.as_deref(), path.as_deref()) {
                            (Some(l), _) => {
                                print_syntax_file_ext(
                                    sink,
                                    &content,
                                    path.as_deref().unwrap_or(""),
                                    Some(smelt_core::content::highlight::lang_to_ext(l)),
                                    0,
                                    u16::MAX,
                                );
                            }
                            (None, Some(p)) => {
                                print_syntax_file(sink, &content, p, 0, u16::MAX);
                            }
                            (None, None) => {
                                print_syntax_file(sink, &content, "", 0, u16::MAX);
                            }
                        }
                    });
                }
            });
            Ok(())
        },
    )?;
    smelt.set("syntax", syntax)?;
    Ok(())
}
