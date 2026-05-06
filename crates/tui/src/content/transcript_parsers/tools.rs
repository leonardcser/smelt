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
            let inner_width = (width as u16).saturating_sub(2);
            rows += replay_layout(out, &layout, inner_width);
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
/// prefix. Caps at [`MAX_TOOL_BLOCK_ROWS`]. `inner_width` is the
/// width available inside the gutter; used by `Hbox` column allocation
/// and 1×1 leaf auto-repeat.
fn replay_layout(out: &mut LineBuilder, layout: &BlockLayout, inner_width: u16) -> u16 {
    crate::lua::app_ref::try_with_app(|app| {
        let cap = MAX_TOOL_BLOCK_ROWS as u16;
        replay_node(out, layout, app, cap, inner_width, true)
    })
    .unwrap_or(0)
}

fn replay_node(
    out: &mut LineBuilder,
    layout: &BlockLayout,
    app: &mut crate::app::TuiApp,
    rows_cap: u16,
    width: u16,
    with_gutter: bool,
) -> u16 {
    if rows_cap == 0 {
        return 0;
    }
    match layout {
        BlockLayout::Leaf(buf_id) => {
            let Some(buf) = app.ui.buf_destroy(*buf_id) else {
                return 0;
            };
            replay_leaf(out, &buf, rows_cap, width, with_gutter)
        }
        BlockLayout::Vbox(items) => {
            let mut written = 0u16;
            for child in items {
                let remaining = rows_cap.saturating_sub(written);
                if remaining == 0 {
                    break;
                }
                written = written.saturating_add(replay_node(
                    out,
                    child,
                    app,
                    remaining,
                    width,
                    with_gutter,
                ));
            }
            written
        }
        BlockLayout::Hbox(items) => replay_hbox(out, items, app, rows_cap, width, with_gutter),
    }
}

fn replay_leaf(
    out: &mut LineBuilder,
    buf: &smelt_core::buffer::Buffer,
    rows_cap: u16,
    width: u16,
    with_gutter: bool,
) -> u16 {
    let n = buf.line_count();
    if n == 0 || rows_cap == 0 {
        return 0;
    }
    if is_unit_leaf(buf) && width > 0 {
        let glyph = buf.get_line(0).unwrap_or("");
        if with_gutter {
            out.print_gutter("  ");
        }
        out.print(&glyph.repeat(width as usize));
        out.newline();
        return 1;
    }
    let limit = (n as u16).min(rows_cap);
    for i in 0..limit {
        if with_gutter {
            out.print_gutter("  ");
        }
        replay_buffer_row_into(buf, i, out);
        out.newline();
    }
    limit
}

fn replay_hbox(
    out: &mut LineBuilder,
    items: &[smelt_core::content::block_layout::HboxItem],
    app: &mut crate::app::TuiApp,
    rows_cap: u16,
    total_width: u16,
    with_gutter: bool,
) -> u16 {
    let widths = smelt_core::content::block_layout::solve_hbox_widths(items, total_width);

    // Take ownership of each child leaf's buffer when it is a Leaf.
    // Non-Leaf Hbox children render as their flattened leaves stacked
    // (a v1 limitation; nested Hbox/Vbox columns aren't laid out
    // side-by-side). Using leaves() preserves the buf-destroy semantics
    // and order.
    let mut columns: Vec<Vec<smelt_core::buffer::Buffer>> = Vec::with_capacity(items.len());
    let mut col_height: u16 = 0;
    let mut any_unit_only = true;
    for item in items {
        let mut bufs: Vec<smelt_core::buffer::Buffer> = Vec::new();
        for buf_id in item.layout.leaves() {
            if let Some(buf) = app.ui.buf_destroy(buf_id) {
                bufs.push(buf);
            }
        }
        let height: u16 = bufs
            .iter()
            .map(|b| {
                if is_unit_leaf(b) {
                    0
                } else {
                    b.line_count() as u16
                }
            })
            .sum();
        if height > 0 {
            any_unit_only = false;
        }
        if height > col_height {
            col_height = height;
        }
        columns.push(bufs);
    }
    if any_unit_only {
        // Pure separator row — keep it a single line.
        col_height = 1;
    }
    let row_total = col_height.min(rows_cap);
    if row_total == 0 {
        return 0;
    }

    for r in 0..row_total {
        if with_gutter {
            out.print_gutter("  ");
        }
        for (col_idx, bufs) in columns.iter().enumerate() {
            let col_w = widths.get(col_idx).copied().unwrap_or(0);
            if col_w == 0 {
                continue;
            }
            let emitted = emit_column_row(out, bufs, r, col_w);
            // Pad the rest of the column with spaces so subsequent
            // columns start at the right offset.
            if emitted < col_w {
                out.print(&" ".repeat((col_w - emitted) as usize));
            }
        }
        out.newline();
    }
    row_total
}

/// Pick which leaf inside a column owns row `r`, then emit a clipped
/// styled row into `out`. Returns the display width emitted.
fn emit_column_row(
    out: &mut LineBuilder,
    bufs: &[smelt_core::buffer::Buffer],
    r: u16,
    col_w: u16,
) -> u16 {
    // 1×1 unit leaves repeat horizontally to fill the column; if the
    // column contains a unit leaf at any vertical position it paints on
    // every row.
    for buf in bufs {
        if is_unit_leaf(buf) {
            let glyph = buf.get_line(0).unwrap_or("");
            let repeat = col_w as usize;
            let s = glyph.repeat(repeat);
            out.print(&s);
            return col_w;
        }
    }
    // Walk the leaves to find the (leaf, local_row) for absolute row r.
    let mut consumed: u16 = 0;
    for buf in bufs {
        let h = buf.line_count() as u16;
        if r < consumed + h {
            return emit_buffer_row_clipped(buf, r - consumed, col_w, out);
        }
        consumed = consumed.saturating_add(h);
    }
    0
}

fn is_unit_leaf(buf: &smelt_core::buffer::Buffer) -> bool {
    if buf.line_count() != 1 {
        return false;
    }
    let line = buf.get_line(0).unwrap_or("");
    smelt_core::content::builder::display_width(line) == 1
}

/// Emit a buffer row's styled spans into `out`, clipped to `max_cols`
/// display columns. Returns the display width actually emitted.
fn emit_buffer_row_clipped(
    buf: &smelt_core::buffer::Buffer,
    row: u16,
    max_cols: u16,
    out: &mut LineBuilder,
) -> u16 {
    use unicode_width::UnicodeWidthChar;

    let text = buf.get_line(row as usize).unwrap_or("");
    let mut highlights = buf.highlights_at(row as usize);
    highlights.sort_by_key(|h| h.col_start);

    let chars: Vec<char> = text.chars().collect();
    let mut emitted_cols: u16 = 0;
    let mut col_idx: u16 = 0;

    let theme_clone = out.theme().clone();

    for h in &highlights {
        if h.col_end <= col_idx {
            continue;
        }
        if h.col_start > col_idx {
            let plain: String = chars[col_idx as usize..h.col_start as usize]
                .iter()
                .collect();
            let used = emit_clipped(
                out,
                &plain,
                None,
                SpanMeta::default(),
                max_cols,
                emitted_cols,
            );
            emitted_cols = emitted_cols.saturating_add(used);
            col_idx = h.col_start;
            if emitted_cols >= max_cols {
                return emitted_cols;
            }
        }
        let end = h.col_end.min(chars.len() as u16);
        if end <= col_idx {
            continue;
        }
        let segment: String = chars[col_idx as usize..end as usize].iter().collect();
        let style = theme_clone.resolve(h.hl);
        let used = emit_clipped(
            out,
            &segment,
            Some(style),
            h.meta.clone(),
            max_cols,
            emitted_cols,
        );
        emitted_cols = emitted_cols.saturating_add(used);
        col_idx = end;
        if emitted_cols >= max_cols {
            return emitted_cols;
        }
    }
    if (col_idx as usize) < chars.len() && emitted_cols < max_cols {
        let tail: String = chars[col_idx as usize..].iter().collect();
        let used = emit_clipped(
            out,
            &tail,
            None,
            SpanMeta::default(),
            max_cols,
            emitted_cols,
        );
        emitted_cols = emitted_cols.saturating_add(used);
    }
    let _ = UnicodeWidthChar::width(' '); // satisfy import even if loop empty
    emitted_cols
}

fn emit_clipped(
    out: &mut LineBuilder,
    segment: &str,
    style: Option<smelt_core::style::Style>,
    meta: SpanMeta,
    max_cols: u16,
    already: u16,
) -> u16 {
    use unicode_width::UnicodeWidthChar;
    let budget = max_cols.saturating_sub(already);
    if budget == 0 {
        return 0;
    }
    let mut acc = String::new();
    let mut acc_w: u16 = 0;
    for ch in segment.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if acc_w.saturating_add(cw) > budget {
            break;
        }
        acc.push(ch);
        acc_w = acc_w.saturating_add(cw);
    }
    if acc.is_empty() {
        return 0;
    }
    if let Some(s) = style {
        out.append_resolved_span(&acc, s, meta);
    } else {
        out.print(&acc);
    }
    acc_w
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
