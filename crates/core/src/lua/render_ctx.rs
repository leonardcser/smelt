//! RenderCtx userdata — passed to Lua tool `render` hooks so they can
//! call back into Rust rendering primitives (diff, syntax, markdown,
//! etc.) directly.

use crate::content::display::{ColorRole, ColorValue};
use crate::content::highlight::{print_inline_diff, print_syntax_file, render_code_block};
use crate::content::layout_out::SpanCollector;
use crate::content::wrap::wrap_line;
use mlua::prelude::*;

/// Opaque handle given to Lua `render` hooks. Methods write directly
/// into the `SpanCollector` owned by the Rust caller.
pub struct RenderCtx {
    col: *mut SpanCollector,
    width: usize,
}

impl RenderCtx {
    /// Create a new `RenderCtx`. The pointer is valid only for the
    /// duration of the enclosing `render_tool_body` call.
    pub(crate) fn new(col: &mut SpanCollector, width: usize) -> Self {
        Self { col, width }
    }
}

impl LuaUserData for RenderCtx {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut(
            "text",
            |_, this, (content, is_error): (String, Option<bool>)| {
                let col = unsafe { &mut *this.col };
                let is_error = is_error.unwrap_or(false);
                let max_cols = this.width.saturating_sub(3);
                for line in content.lines() {
                    let expanded = line.replace('\t', "    ");
                    let segs = wrap_line(&expanded, max_cols);
                    if segs.len() > 1 {
                        col.mark_wrapped();
                    }
                    for seg in &segs {
                        if is_error {
                            col.push_fg(ColorValue::Role(ColorRole::ErrorMsg));
                            col.print_string(format!("  {}", seg));
                            col.pop_style();
                        } else {
                            col.push_dim();
                            col.print(&format!("  {}", seg));
                            col.pop_style();
                        }
                        col.newline();
                    }
                }
                Ok(())
            },
        );

        methods.add_method_mut(
            "diff",
            |_, this, (old, new, path): (String, String, String)| {
                let col = unsafe { &mut *this.col };
                print_inline_diff(col, &old, &new, &path, &old, 0, u16::MAX);
                Ok(())
            },
        );

        methods.add_method_mut("file", |_, this, (content, path): (String, String)| {
            let col = unsafe { &mut *this.col };
            print_syntax_file(col, &content, &path, 0, u16::MAX);
            Ok(())
        });

        methods.add_method_mut("markdown", |_, this, source: String| {
            let col = unsafe { &mut *this.col };
            crate::transcript_present::render_markdown_inner(
                col, &source, this.width, "", false, None,
            );
            Ok(())
        });

        methods.add_method_mut("code", |_, this, (source, lang): (String, String)| {
            let col = unsafe { &mut *this.col };
            let lines: Vec<&str> = source.lines().collect();
            render_code_block(col, &lines, &lang, this.width, true, None, false);
            Ok(())
        });

        methods.add_method_mut("newline", |_, this, ()| {
            let col = unsafe { &mut *this.col };
            col.newline();
            Ok(())
        });
    }
}
