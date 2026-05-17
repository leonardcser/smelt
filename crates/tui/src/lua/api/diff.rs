//! `smelt.diff` — paint diffs into buffers.
//!
//! - `smelt.diff.render_split(left_buf, right_buf, { old, new, lang? | path? })`
//!   paints aligned side-by-side views into two buffers — synthetic padding
//!   rows on whichever side is shorter so both buffers share a row count.
//!
//! Inline (single-buffer) diffs are returned declaratively via
//! `smelt.layout.diff{...}` from a tool's `render` / `preview` callback; the
//! host renders them directly into the block buffer with no scratch-buffer seam.

use crate::content::highlight::{compute_split_diff, print_split_diff_side};
use crate::content::to_buffer::render_into_buffer;
use crate::smelt_term::BufId;
use lua_doc_derive::lua_module;
use mlua::prelude::*;
use smelt_core::content::highlight::{lang_to_ext, SplitSide};
use smelt_core::lua::doc::register_ui_fn;

#[lua_module(
    name = "smelt.diff",
    doc = "Paint side-by-side diffs into a pair of Buffers. Inline diffs are returned declaratively via `smelt.layout.diff`. UiHost-only."
)]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let diff = lua.create_table()?;
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

/// Render the split diff into both buffers. The diff plan is computed
/// once and replayed per side, so each side renders directly into its
/// target buffer — no scratch copy, no lost highlight extmarks.
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
