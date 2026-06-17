//! `smelt.render` - paint plain text / markdown / syntax-highlighted
//! code / split diffs into a `Buf`. UiHost-only.
//!
//! Every fn writes into one or more buffers using the same renderer
//! the transcript uses for its own content blocks. Inline single-span
//! syntax highlighting is handled by `buf:styled` with `syntax = "..."`.
//!
//! For inline diffs, file views, and notebook previews inside tool
//! `render` / `preview` callbacks, return a declarative
//! `smelt.layout.{diff,file_view,vbox}{...}` instead - the host renders
//! it directly into the block buffer with no scratch-buffer seam.

use crate::content::highlight::{
    compute_split_diff, lang_to_ext, print_code_lines, print_split_diff_side,
};
use crate::content::to_buffer::render_into_buffer;
use crate::smelt_edit::BufId;
use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use smelt_core::content::highlight::SplitSide;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::theme::intern;

use super::buf::LuaBuf;

/// Options for `smelt.render.text`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.render.TextOpts")]
pub struct LuaRenderTextOpts {
    /// Highlight group applied to the whole block. When omitted, text renders dim.
    pub hl_group: Option<String>,
    /// Wrapping width in terminal cells. Defaults to the current terminal width.
    pub width: Option<u16>,
}

/// Options for `smelt.render.syntax`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.render.SyntaxOpts")]
pub struct LuaRenderSyntaxOpts {
    /// Source code to render.
    #[lua(default)]
    pub content: String,
    /// Syntax language token such as `"rust"`, `"lua"`, or `"py"`.
    pub lang: Option<String>,
    /// Path whose extension is used when `lang` is omitted.
    pub path: Option<String>,
}

/// Options for `smelt.render.diff_split`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.render.DiffSplitOpts")]
pub struct LuaRenderDiffSplitOpts {
    /// Left/pre-edit text.
    #[lua(default)]
    pub old: String,
    /// Right/post-edit text.
    #[lua(default)]
    pub new: String,
    /// Syntax language token such as `"rust"`, `"lua"`, or `"py"`.
    pub lang: Option<String>,
    /// Path whose extension is used when `lang` is omitted.
    pub path: Option<String>,
}

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
        "Paint plain text into a buffer. With no `opts.hl_group`, text renders as dim body. Pass `opts.hl_group = \"ErrorMsg\"` for errors, `\"SmeltAccent\"` for accent, or any registered theme group; the mapping is the caller's choice, not the renderer's. `opts.width` overrides the wrapping width for tool layouts rendered into narrower panes.",
        &["buf", "content", "opts"],
        |_, (buf, content, opts): (LuaBuf, String, Option<LuaRenderTextOpts>)| -> LuaResult<()> {
            let hl_group = opts.as_ref().and_then(|opts| opts.hl_group.as_deref());
            let width_opt = opts.as_ref().and_then(|opts| opts.width).filter(|w| *w > 0);
            crate::lua::with_app(|app| {
                let theme_snap = app.ui.theme().clone();
                let width = width_opt.unwrap_or_else(|| crate::content::term_width() as u16);
                if let Some(buf) = app.ui.buf_mut(buf.id) {
                    render_into_buffer(buf, width, &theme_snap, |sink| {
                        let max_cols = (width as usize).saturating_sub(3);
                        let hl = hl_group.map(intern);
                        match hl {
                            Some(g) => sink.push_hl(g),
                            None => sink.push_dim(),
                        }
                        for line in content.lines() {
                            let expanded = line.replace('\t', "    ");
                            let (spans, ranges, boundaries) =
                                smelt_core::content::ansi::wrap_ansi(&expanded, max_cols);
                            if ranges.len() > 1 {
                                sink.mark_wrapped();
                            }
                            for &(ws, we) in &ranges {
                                smelt_core::content::ansi::emit_ansi_row(
                                    sink, &spans, &boundaries, ws, we,
                                );
                                sink.newline();
                            }
                        }
                        sink.pop_style();
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
                        crate::content::display_renderers::render_markdown_inner(
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
        |_, (buf, opts): (LuaBuf, LuaRenderSyntaxOpts)| -> LuaResult<()> {
            let content = opts.content;
            let lang = opts.lang;
            let path = opts.path;
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
        |_, (left, right, opts): (LuaBuf, LuaBuf, LuaRenderDiffSplitOpts)| -> LuaResult<()> {
            let old = opts.old;
            let new = opts.new;
            let lang = opts.lang;
            let path = opts.path;
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
