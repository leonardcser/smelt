use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool(
    out: &mut SpanCollector,
    _call_id: &str,
    name: &str,
    summary: &str,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    output: Option<&ToolOutput>,
    user_message: Option<&str>,
    width: usize,
    renderer: Option<&dyn ToolBodyRenderer>,
) -> u16 {
    let color: ColorValue = match status {
        ToolStatus::Ok => ColorValue::Role(ColorRole::Success),
        ToolStatus::Err | ToolStatus::Denied => ColorValue::Role(ColorRole::ErrorMsg),
        ToolStatus::Confirm => ColorValue::Role(ColorRole::Accent),
        ToolStatus::Pending => ColorValue::Role(ColorRole::ToolPending),
    };
    let time = if status != ToolStatus::Confirm && renderer.is_some_and(|r| r.elapsed_visible(name))
    {
        elapsed
    } else {
        None
    };
    let tl = if name == "bash" && status == ToolStatus::Pending {
        let ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(120_000);
        let secs = ms / 1000;
        Some(format!("timeout: {}", format_duration(secs)))
    } else {
        None
    };
    let mut rows = print_tool_line(out, name, summary, color, time, tl.as_deref(), width);
    if name == "web_fetch" {
        if let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) {
            let segs = wrap_line(prompt, width.saturating_sub(3));
            if segs.len() > 1 {
                out.mark_wrapped();
            }
            for seg in &segs {
                print_dim(out, &format!("  {}", seg));
                out.newline();
                rows += 1;
            }
        }
    }
    if let Some(msg) = user_message {
        print_dim(out, &format!("  {msg}"));
        out.newline();
        rows += 1;
    }
    if status != ToolStatus::Denied {
        if let Some(out_data) = output {
            if !out_data.content.is_empty() {
                if let Some(r) = renderer {
                    rows += r.render(name, args, Some(out_data), width, out);
                } else {
                    rows += print_tool_output(out, name, out_data, args, width);
                }
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
    out: &mut SpanCollector,
    name: &str,
    summary: &str,
    pill_color: ColorValue,
    elapsed: Option<Duration>,
    timeout_label: Option<&str>,
    width: usize,
) -> u16 {
    out.push_fg(pill_color);
    out.print("\u{23fa}");
    out.pop_style();
    let time_str = elapsed
        .filter(|d| d.as_secs_f64() >= 0.1)
        .map(|d| format!("  {}", format_duration(d.as_secs())))
        .unwrap_or_default();
    let timeout_str = timeout_label
        .map(|l| format!(" ({})", l))
        .unwrap_or_default();
    let suffix_len = time_str.len() + timeout_str.len();
    let ly = tool_line_layout(name, suffix_len, width);

    print_dim(out, &format!(" {} ", name));

    if name == "bash" {
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
        let total = wrapped.len();
        let show = total.min(MAX_TOOL_BLOCK_ROWS);
        let mut line_num = 0;
        let mut bh = BashHighlighter::new();

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
            bh.print_line(out, seg);
            if idx == 0 {
                print_dim_non_selectable(out, &time_str, &timeout_str);
            }
            out.newline();
            line_num += 1;
        }

        if total > MAX_TOOL_BLOCK_ROWS {
            let skipped = total - MAX_TOOL_BLOCK_ROWS;
            out.print_gutter(&" ".repeat(ly.prefix_len));
            print_dim(
                out,
                &format!("... {} below", pluralize(skipped, "line", "lines")),
            );
            out.newline();
            line_num += 1;
        }

        return line_num as u16;
    }

    let truncated = truncate_str(summary, ly.max_summary);
    out.print(&truncated);
    print_dim_non_selectable(out, &time_str, &timeout_str);
    out.newline();
    1
}

pub(super) fn print_tool_output(
    out: &mut SpanCollector,
    _name: &str,
    output: &ToolOutput,
    _args: &HashMap<String, serde_json::Value>,
    width: usize,
) -> u16 {
    render_wrapped_output(out, &output.content, output.is_error, width)
}

pub(super) fn print_dim(out: &mut SpanCollector, text: &str) {
    out.push_dim();
    out.print(text);
    out.pop_style();
}

fn print_dim_non_selectable(out: &mut SpanCollector, time_str: &str, timeout_str: &str) {
    let meta = SpanMeta {
        selectable: false,
        copy_as: None,
    };
    if !time_str.is_empty() {
        out.push_dim();
        out.print_with_meta(time_str, meta.clone());
        out.pop_style();
    }
    if !timeout_str.is_empty() {
        out.push_dim();
        out.print_with_meta(timeout_str, meta);
        out.pop_style();
    }
}

pub fn render_wrapped_output(
    out: &mut SpanCollector,
    content: &str,
    is_error: bool,
    width: usize,
) -> u16 {
    let _perf = crate::perf::begin("render:wrapped_output");
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
            out.push_fg(ColorValue::Role(ColorRole::ErrorMsg));
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

pub fn render_default_output(
    out: &mut SpanCollector,
    content: &str,
    is_error: bool,
    width: usize,
) -> u16 {
    let preview = result_preview(content, DEFAULT_PREVIEW_LINES);
    let max_cols = width.saturating_sub(3);
    let segs = wrap_line(&preview, max_cols);
    if segs.len() > 1 {
        out.mark_wrapped();
    }
    let mut rows = 0u16;
    for seg in &segs {
        if is_error {
            out.push_fg(ColorValue::Role(ColorRole::ErrorMsg));
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

fn result_preview(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.trim_end_matches('\n').lines().collect();
    if lines.len() <= max_lines {
        lines.join(" | ")
    } else {
        format!(
            "{} ... ({})",
            lines[..max_lines].join(" | "),
            pluralize(lines.len(), "line", "lines")
        )
    }
}
