//! `smelt.render` — paint plain text / markdown / syntax-highlighted
//! code / split diffs into a `Buf`. UiHost-only.
//!
//! Every fn writes into one or more buffers using the same renderer
//! the transcript uses for its own content blocks. Inline single-span
//! syntax highlighting is handled by `buf:styled` with `syntax = "..."`.
//!
//! For inline diffs, file views, and notebook previews inside tool
//! `render` / `preview` callbacks, return a declarative
//! `smelt.layout.{diff,file_view,vbox}{...}` instead — the host renders
//! it directly into the block buffer with no scratch-buffer seam.

use crate::content::highlight::{
    compute_split_diff, lang_to_ext, print_code_lines, print_split_diff_side,
};
use crate::content::to_buffer::render_into_buffer;
use crate::smelt_term::BufId;
use mlua::prelude::*;
use smelt_core::content::highlight::SplitSide;
use smelt_core::content::wrap::wrap_line;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::theme::intern;

use super::buf::LuaBuf;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "render",
        "Paint text / markdown / syntax-highlighted code / split diffs into a `Buf`. UiHost-only.",
        Tier::UiHost,
    )?;

    m.fn_(
        "text",
        "Paint plain text into a buffer. With no `opts.hl_group`, text renders as dim body. Pass `opts.hl_group = \"ErrorMsg\"` for errors, `\"SmeltAccent\"` for accent, or any registered theme group — the mapping is the caller's choice, not the renderer's.",
        &["buf", "content", "opts"],
        |_, (buf, content, opts): (LuaBuf, String, Option<mlua::Table>)| -> LuaResult<()> {
            let hl_group = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("hl_group").ok().flatten());
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(buf.id) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        let max_cols = (width as usize).saturating_sub(3);
                        let hl = hl_group.as_deref().map(intern);
                        for line in content.lines() {
                            let expanded = line.replace('\t', "    ");
                            let segs = wrap_line(&expanded, max_cols);
                            if segs.len() > 1 {
                                sink.mark_wrapped();
                            }
                            for seg in &segs {
                                match hl {
                                    Some(g) => sink.push_hl(g),
                                    None => sink.push_dim(),
                                }
                                sink.print(seg);
                                sink.pop_style();
                                sink.newline();
                            }
                        }
                    });
                }
            });
            Ok(())
        },
    )?;

    m.fn_(
        "markdown",
        "Render markdown `source` into the buffer using the same renderer the transcript uses for assistant text blocks.",
        &["buf", "source"],
        |_, (buf, source): (LuaBuf, String)| -> LuaResult<()> {
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(buf.id) {
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

    m.fn_(
        "syntax",
        "Paint syntect-highlighted code from `opts.content` into the buffer as a plain block. Pick syntax via `opts.lang` or `opts.path`. Unknown languages fall back to plain text.",
        &["buf", "opts"],
        |_, (buf, opts): (LuaBuf, mlua::Table)| -> LuaResult<()> {
            let content: String = opts.get::<Option<String>>("content")?.unwrap_or_default();
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            let path: Option<String> = opts.get::<Option<String>>("path")?;
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = crate::content::term_width() as u16;
                if let Some(buf) = app.ui.buf_mut(buf.id) {
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

    m.fn_(
        "diff_split",
        "Paint a side-by-side diff between `opts.old` and `opts.new` into two buffers. Both buffers end up with the same row count; synthetic padding rows align them. Pick syntax via `opts.lang` or `opts.path`.",
        &["left", "right", "opts"],
        |_, (left, right, opts): (LuaBuf, LuaBuf, mlua::Table)| -> LuaResult<()> {
            let old: String = opts.get::<Option<String>>("old")?.unwrap_or_default();
            let new: String = opts.get::<Option<String>>("new")?.unwrap_or_default();
            let lang: Option<String> = opts.get::<Option<String>>("lang")?;
            let path: Option<String> = opts.get::<Option<String>>("path")?;
            let ext: Option<String> = lang.as_deref().map(|l| lang_to_ext(l).to_string()).or_else(
                || {
                    path.as_deref()
                        .and_then(|p| std::path::Path::new(p).extension())
                        .and_then(|e| e.to_str())
                        .map(str::to_string)
                },
            );
            crate::lua::with_app(|app| {
                render_split_into_pair(app, left.id, right.id, &old, &new, ext.as_deref());
            });
            Ok(())
        },
    )?;

    Ok(())
}

fn render_split_into_pair(
    app: &mut crate::app::TuiApp,
    left_id: BufId,
    right_id: BufId,
    old: &str,
    new: &str,
    ext: Option<&str>,
) {
    let theme = app.ui.theme().clone();
    let width = crate::content::term_width() as u16;
    let plan = compute_split_diff(old, new);
    if let Some(buf) = app.ui.buf_mut(left_id) {
        render_into_buffer(buf, width, &theme, |sink| {
            print_split_diff_side(sink, &plan, ext, SplitSide::Left);
        });
    }
    if let Some(buf) = app.ui.buf_mut(right_id) {
        render_into_buffer(buf, width, &theme, |sink| {
            print_split_diff_side(sink, &plan, ext, SplitSide::Right);
        });
    }
}
