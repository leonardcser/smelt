use super::MAX_TOOL_BLOCK_ROWS;
use smelt_core::buffer::SpanMeta;
use smelt_core::content::block_layout::BlockLayout;
use smelt_core::content::builder::{replay_buffer_row_into, LineBuilder};
use smelt_core::content::wrap::wrap_line;
use smelt_core::theme::{role_hl, HlGroup};
use smelt_core::transcript_model::{ToolOutput, ToolStatus};
use smelt_core::utils::format_duration;
use std::collections::HashMap;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool(
    out: &mut LineBuilder,
    call_id: &str,
    name: &str,
    summary: &str,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    output: Option<&ToolOutput>,
    user_message: Option<&str>,
    width: usize,
) -> u16 {
    let color: HlGroup = match status {
        ToolStatus::Ok => role_hl("Success"),
        ToolStatus::Err | ToolStatus::Denied => role_hl("ErrorMsg"),
        ToolStatus::Confirm => role_hl("Accent"),
        ToolStatus::Pending => role_hl("ToolPending"),
    };
    let time = if status != ToolStatus::Confirm {
        elapsed
    } else {
        None
    };
    let mut rows = print_tool_line(out, name, summary, color, time, width);
    if let Some(msg) = user_message {
        print_dim(out, &format!("  {msg}"));
        out.newline();
        rows += 1;
    }
    if status != ToolStatus::Denied {
        let layout =
            call_render_layout(name, args, output, summary, status, elapsed, call_id, width);
        if let Some(layout) = layout {
            rows += replay_layout(out, &layout);
        } else if let Some(out_data) = output {
            if !out_data.content.is_empty() {
                rows += print_tool_output(out, name, out_data, args, width);
            }
        }
    }
    rows
}

/// Layout metrics for a tool header line.
struct ToolLineLayout {
    prefix_len: usize,
    max_summary: usize,
}

fn tool_line_layout(name: &str, suffix_len: usize, width: usize) -> ToolLineLayout {
    let prefix_len = 2 + name.len() + 1; // "⏺ " + name + " "
    let max_summary = width.saturating_sub(prefix_len + suffix_len + 1);
    ToolLineLayout {
        prefix_len,
        max_summary,
    }
}

fn print_tool_line(
    out: &mut LineBuilder,
    name: &str,
    summary: &str,
    pill_color: HlGroup,
    elapsed: Option<Duration>,
    width: usize,
) -> u16 {
    out.push_hl(pill_color);
    out.print("\u{23fa}");
    out.pop_style();
    let time_str = elapsed
        .filter(|d| d.as_secs_f64() >= 1.0)
        .map(|d| format!("  {}", format_duration(d.as_secs())))
        .unwrap_or_default();
    let suffix_len = time_str.len();
    let ly = tool_line_layout(name, suffix_len, width);

    print_dim(out, &format!(" {} ", name));

    // Wrap the summary, then paint each line as plain text. Tools that
    // want custom row-0 styling do so by skipping the default summary
    // (returning a layout that includes their own header buffer).
    let raw_lines: Vec<&str> = summary.lines().collect();
    let mut wrapped: Vec<String> = Vec::new();
    let mut is_soft_wrap = Vec::new();
    for line in &raw_lines {
        let segs = wrap_line(line, ly.max_summary.max(1));
        if segs.len() > 1 {
            out.mark_wrapped();
        }
        for (si, seg) in segs.into_iter().enumerate() {
            is_soft_wrap.push(si > 0);
            wrapped.push(seg);
        }
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
        is_soft_wrap.push(false);
    }
    let total = wrapped.len();
    let show = total.min(MAX_TOOL_BLOCK_ROWS);
    let mut rows = 0u16;

    for (idx, seg) in wrapped[..show].iter().enumerate() {
        if idx > 0 {
            out.print_gutter(&" ".repeat(ly.prefix_len));
            if is_soft_wrap[idx] {
                out.mark_soft_wrap_continuation();
            }
        }
        if idx == 0 {
            out.set_source_text(summary);
        }
        out.print(seg);
        if idx == 0 {
            print_dim_non_selectable(out, &time_str);
        }
        out.newline();
        rows += 1;
    }

    if total > MAX_TOOL_BLOCK_ROWS {
        let skipped = total - MAX_TOOL_BLOCK_ROWS;
        out.print_gutter(&" ".repeat(ly.prefix_len));
        print_dim(
            out,
            &format!("... {} below", pluralize(skipped, "line", "lines")),
        );
        out.newline();
        rows += 1;
    }

    rows
}

#[allow(clippy::too_many_arguments)]
fn call_render_layout(
    name: &str,
    args: &HashMap<String, serde_json::Value>,
    output: Option<&ToolOutput>,
    summary: &str,
    status: ToolStatus,
    elapsed: Option<Duration>,
    call_id: &str,
    width: usize,
) -> Option<BlockLayout> {
    let status_label = match status {
        ToolStatus::Pending => "pending",
        ToolStatus::Ok => "ok",
        ToolStatus::Err => "err",
        ToolStatus::Denied => "denied",
        ToolStatus::Confirm => "confirm",
    };
    let elapsed_secs = elapsed.map(|d| d.as_secs());
    let cid = if call_id.is_empty() {
        None
    } else {
        Some(call_id)
    };
    crate::lua::app_ref::try_with_app(|app| {
        app.lua.render_tool_layout(
            name,
            args,
            output,
            smelt_core::lua::runtime::ToolRenderCtx {
                width,
                summary,
                status: status_label,
                elapsed_secs,
                call_id: cid,
            },
        )
    })
    .flatten()
}

/// Walk a [`BlockLayout`] returned by a tool's `render` hook and
/// replay each leaf buffer's rows into `out`. Continuation rows are
/// gutter-padded to two columns so they line up under the `⏺ name `
/// prefix. Caps at [`MAX_TOOL_BLOCK_ROWS`].
fn replay_layout(out: &mut LineBuilder, layout: &BlockLayout) -> u16 {
    crate::lua::app_ref::try_with_app(|app| {
        let leaves = layout.leaves();
        let mut rows = 0u16;
        for buf_id in leaves {
            if rows as usize >= MAX_TOOL_BLOCK_ROWS {
                break;
            }
            let Some(buf) = app.ui.buf_destroy(buf_id) else {
                continue;
            };
            let n = buf.line_count();
            for i in 0..n {
                if rows as usize >= MAX_TOOL_BLOCK_ROWS {
                    break;
                }
                out.print_gutter("  ");
                replay_buffer_row_into(&buf, i as u16, out);
                out.newline();
                rows += 1;
            }
        }
        rows
    })
    .unwrap_or(0)
}

pub(super) fn print_tool_output(
    out: &mut LineBuilder,
    _name: &str,
    output: &ToolOutput,
    _args: &HashMap<String, serde_json::Value>,
    width: usize,
) -> u16 {
    render_wrapped_output(out, &output.content, output.is_error, width)
}

pub(super) fn print_dim(out: &mut LineBuilder, text: &str) {
    out.push_dim();
    out.print(text);
    out.pop_style();
}

fn print_dim_non_selectable(out: &mut LineBuilder, time_str: &str) {
    let meta = SpanMeta {
        selectable: false,
        copy_as: None,
    };
    if !time_str.is_empty() {
        out.push_dim();
        out.print_with_meta(time_str, meta);
        out.pop_style();
    }
}

pub fn render_wrapped_output(
    out: &mut LineBuilder,
    content: &str,
    is_error: bool,
    width: usize,
) -> u16 {
    let _perf = smelt_core::perf::begin("render:wrapped_output");
    let max_cols = width.saturating_sub(3); // "  " prefix + 1 margin

    // Pre-wrap all lines so we can count visual rows.
    let wrapped: Vec<String> = content
        .lines()
        .flat_map(|line| {
            let expanded = line.replace('\t', "    ");
            let segs = wrap_line(&expanded, max_cols);
            if segs.len() > 1 {
                out.mark_wrapped();
            }
            segs
        })
        .collect();

    let total = wrapped.len();
    let mut rows = 0u16;
    if total > MAX_TOOL_BLOCK_ROWS {
        let skipped = total - MAX_TOOL_BLOCK_ROWS;
        print_dim(
            out,
            &format!("  ... {} above", pluralize(skipped, "line", "lines")),
        );
        out.newline();
        rows += 1;
    }
    let start = total.saturating_sub(MAX_TOOL_BLOCK_ROWS);
    for seg in &wrapped[start..] {
        if is_error {
            out.push_hl(role_hl("ErrorMsg"));
            out.print_string(format!("  {}", seg));
            out.pop_style();
        } else {
            print_dim(out, &format!("  {}", seg));
        }
        out.newline();
        rows += 1;
    }
    rows
}

pub(super) fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}
