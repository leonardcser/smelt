//! `smelt.diff` — paint diffs into buffers.
//!
//! - `smelt.diff.render(buf, { old, new, path })` paints an inline diff
//!   (one buffer with delete/insert lines interleaved). Same pipeline the
//!   confirm dialog uses.
//! - `smelt.diff.render_split(left_buf, right_buf, { old, new, lang? | path? })`
//!   paints aligned side-by-side views into two buffers — synthetic padding
//!   rows on whichever side is shorter so both buffers share a row count.

use crate::content::highlight::{print_inline_diff, print_split_diff};
use crate::content::to_buffer::render_into_buffer;
use crate::smelt_term::BufId;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::content::highlight::lang_to_ext;
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.diff",
    doc = "Paint diffs into Buffers — inline (one buffer) or split side-by-side (two buffers). UiHost-only."
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
        |_, (buf_id, opts): (u64, mlua::Table)| -> LuaResult<()> {
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
    register_ui_fn(
        &diff,
        "smelt.diff",
        "render_split",
        "Paint a side-by-side diff between `opts.old` and `opts.new` into two buffers — `left_buf_id` gets the pre-edit view, `right_buf_id` gets the post-edit view. Both buffers end up with the same row count: synthetic padding rows fill in wherever one side has fewer changes than the other, so vertical alignment between sides is exact. Pick the syntax via `opts.lang` (`\"rust\"`, `\"py\"`, …) or `opts.path` (extension-sniffed); `lang` wins when both are set.",
        &["left_buf_id", "right_buf_id", "opts"],
        lua,
        |_, (left_id, right_id, opts): (u64, u64, mlua::Table)| -> LuaResult<()> {
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
                render_split_into_pair(
                    app,
                    BufId(left_id),
                    BufId(right_id),
                    &old,
                    &new,
                    ext.as_deref(),
                );
            });
            Ok(())
        },
    )?;
    smelt.set("diff", diff)?;
    Ok(())
}

/// Render the split diff into both buffers in a single pass. Two buffers are
/// borrowed sequentially (not simultaneously), so we cache `LineBuilder`
/// output in scratch buffers and replay onto the targets.
fn render_split_into_pair(
    app: &mut crate::app::TuiApp,
    left_id: BufId,
    right_id: BufId,
    old: &str,
    new: &str,
    ext: Option<&str>,
) {
    use smelt_buffer::buffer::{BufCreateOpts, BufId as InnerBufId, Buffer};
    let theme = app.ui.theme().clone();
    let width = crate::content::term_width() as u16;
    let mut scratch_left = Buffer::new(InnerBufId(u64::MAX - 1), BufCreateOpts::default());
    let mut scratch_right = Buffer::new(InnerBufId(u64::MAX - 2), BufCreateOpts::default());
    // Two paired `render_into_buffer` calls share a `LineBuilder` per buffer;
    // `print_split_diff` writes to both within one walk so the two builders
    // need to live concurrently. `render_into_buffer` takes a closure that
    // owns its sink, so to get TWO sinks we nest the calls.
    render_into_buffer(&mut scratch_left, width, &theme, |left_sink| {
        render_into_buffer(&mut scratch_right, width, &theme, |right_sink| {
            print_split_diff(left_sink, right_sink, old, new, ext);
        });
    });
    // Now copy each scratch into the corresponding real buffer by replacing
    // its contents wholesale. We use `take_contents_from` to move lines +
    // decorations; falls back to a manual replay if that doesn't exist.
    replace_buffer_with(app, left_id, scratch_left);
    replace_buffer_with(app, right_id, scratch_right);
}

fn replace_buffer_with(
    app: &mut crate::app::TuiApp,
    target: BufId,
    source: smelt_buffer::buffer::Buffer,
) {
    if let Some(dst) = app.ui.buf_mut(target) {
        let lines: Vec<String> = source.lines().to_vec();
        dst.set_all_lines(lines);
        for row in 0..source.line_count() {
            let dec = source.decoration_at(row).clone();
            dst.set_decoration(row, dec);
        }
    }
}
