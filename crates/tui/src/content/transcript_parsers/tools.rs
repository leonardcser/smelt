use super::metrics::{block_inner_width, BLOCK_GUTTER_SPACE, BLOCK_GUTTER_W};
use super::MAX_TOOL_BLOCK_ROWS;
use protocol::{StyledLines, StyledSpan};
use smelt_core::buffer::SpanMeta;
use smelt_core::content::block_layout::{DiffSpec, FileViewSpec, RenderedLayout, RenderedLeaf};
use smelt_core::content::builder::{replay_buffer_row_into, LineBuilder};
use smelt_core::content::highlight::{
    build_file_view_cache, print_cached_inline_diff, print_inline_diff_ext, GutterStyle,
    InlineSyntax,
};
use smelt_core::content::wrap::{wrap_line, wrap_line_ranges};
use smelt_core::theme::{intern, HlGroup};
use smelt_core::transcript_model::{ToolOutput, ToolStatus};
use smelt_core::utils::format_duration;
use std::collections::HashMap;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool(
    out: &mut LineBuilder,
    name: &str,
    summary: &StyledLines,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    output: Option<&ToolOutput>,
    user_message: Option<&str>,
    rendered: Option<&RenderedLayout>,
    width: usize,
) -> u16 {
    let color: HlGroup = match status {
        ToolStatus::Ok => intern("SmeltSuccess"),
        ToolStatus::Err | ToolStatus::Denied => intern("ErrorMsg"),
        ToolStatus::Confirm => intern("SmeltAccent"),
        ToolStatus::Pending => intern("SmeltToolPending"),
    };
    let time = if status != ToolStatus::Confirm {
        elapsed
    } else {
        None
    };
    let mut rows = print_tool_line(out, name, summary, color, time, width);
    if let Some(msg) = user_message {
        print_dim(out, &format!("{BLOCK_GUTTER_SPACE}{msg}"));
        out.newline();
        rows += 1;
    }
    if status != ToolStatus::Denied {
        if let Some(layout) = rendered {
            let inner_width = block_inner_width(width) as u16;
            rows += replay_rendered(out, layout, inner_width);
        } else if let Some(out_data) = output {
            if !out_data.content.trim().is_empty() {
                rows += print_tool_output(out, name, out_data, args, width);
            }
        }
    }
    rows
}

struct ToolLineLayout {
    prefix_len: usize,
    max_summary: usize,
}

fn tool_line_layout(name: &str, suffix_len: usize, width: usize) -> ToolLineLayout {
    let prefix_len = 2 + name.len() + 1; // "⏺ name "
    let max_summary = width.saturating_sub(prefix_len + suffix_len + 1);
    ToolLineLayout {
        prefix_len,
        max_summary,
    }
}

fn print_tool_line(
    out: &mut LineBuilder,
    name: &str,
    summary: &StyledLines,
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

    let max_seg = ly.max_summary.max(1);

    struct Wrapped {
        // Cumulative byte offsets: span i covers the concatenated plain at offs[i]..offs[i+1].
        offs: Vec<usize>,
        // Wrap segments as byte ranges into the concatenated plain text.
        ranges: Vec<(usize, usize)>,
    }

    let mut wlines: Vec<Wrapped> = summary
        .0
        .iter()
        .map(|spans| {
            let mut plain = String::new();
            let mut offs = Vec::with_capacity(spans.len() + 1);
            offs.push(0);
            for s in spans {
                plain.push_str(&s.text);
                offs.push(plain.len());
            }
            let ranges = wrap_line_ranges(&plain, max_seg);
            Wrapped { offs, ranges }
        })
        .collect();
    if wlines.is_empty() {
        wlines.push(Wrapped {
            offs: vec![0],
            ranges: vec![(0, 0)],
        });
    }

    if wlines.iter().any(|w| w.ranges.len() > 1) {
        out.mark_wrapped();
    }

    let total: usize = wlines.iter().map(|w| w.ranges.len()).sum();
    let show = total.min(MAX_TOOL_BLOCK_ROWS);
    let plain_source = summary.as_plain_text();
    let mut rows = 0u16;
    let mut emitted = 0usize;

    'outer: for (li, w) in wlines.iter().enumerate() {
        let spans: &[StyledSpan] = summary
            .0
            .get(li)
            .map(Vec::as_slice)
            .unwrap_or(&[] as &[StyledSpan]);
        // One InlineSyntax per span — state carries across wrap segments of the
        // same span so syntax context survives soft-wraps.
        let mut syntaxes: Vec<Option<InlineSyntax<'static>>> = spans
            .iter()
            .map(|s| s.syntax.as_deref().map(InlineSyntax::new))
            .collect();

        for (seg_idx, &(rs, re)) in w.ranges.iter().enumerate() {
            if emitted >= show {
                break 'outer;
            }
            let is_first = emitted == 0;
            if !is_first {
                out.print_gutter(&" ".repeat(ly.prefix_len));
                if seg_idx > 0 {
                    out.mark_soft_wrap_continuation();
                }
            }
            if is_first {
                out.set_source_text(&plain_source);
            }

            for (sp_idx, span) in spans.iter().enumerate() {
                let sp_start = w.offs[sp_idx];
                let sp_end = w.offs[sp_idx + 1];
                let lo = sp_start.max(rs);
                let hi = sp_end.min(re);
                if lo >= hi {
                    continue;
                }
                let piece = &span.text[lo - sp_start..hi - sp_start];

                let fg_color = span.fg.as_deref().and_then(|name| out.theme().get(name).fg);
                let bg_color = span.bg.as_deref().and_then(|name| out.theme().get(name).bg);
                out.save_style();
                if let Some(group) = span.hl.as_deref() {
                    out.set_hl(intern(group));
                }
                if let Some(c) = fg_color {
                    out.set_fg(c);
                }
                if let Some(c) = bg_color {
                    out.set_bg(c);
                }
                if span.dim {
                    out.set_dim();
                }
                if span.bold {
                    out.set_bold();
                }
                if span.italic {
                    out.set_italic();
                }
                match syntaxes[sp_idx].as_mut() {
                    Some(h) => h.print_line(out, piece),
                    None => out.print(piece),
                }
                out.pop_style();
            }

            if is_first {
                print_dim_non_selectable(out, &time_str);
            }
            out.newline();
            rows += 1;
            emitted += 1;
        }
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

fn replay_rendered(out: &mut LineBuilder, layout: &RenderedLayout, inner_width: u16) -> u16 {
    let cap = MAX_TOOL_BLOCK_ROWS as u16;
    replay_node(out, layout, cap, inner_width, true)
}

/// Render a `RenderedLayout` directly into `out` (no tool-block gutter, no row cap)
/// — used by the confirm dialog's preview pipeline, which renders into a fresh
/// dialog-owned buffer instead of stamping into a transcript row.
pub fn render_layout_into(out: &mut LineBuilder, layout: &RenderedLayout, width: u16) -> u16 {
    replay_node(out, layout, u16::MAX, width, false)
}

fn replay_node(
    out: &mut LineBuilder,
    layout: &RenderedLayout,
    rows_cap: u16,
    width: u16,
    with_gutter: bool,
) -> u16 {
    if rows_cap == 0 {
        return 0;
    }
    match layout {
        RenderedLayout::Leaf(leaf) => render_leaf(out, leaf, rows_cap, width, with_gutter),
        RenderedLayout::Vbox(items) => {
            let mut written = 0u16;
            for child in items {
                let remaining = rows_cap.saturating_sub(written);
                if remaining == 0 {
                    break;
                }
                written =
                    written.saturating_add(replay_node(out, child, remaining, width, with_gutter));
            }
            written
        }
        RenderedLayout::Hbox(items) => replay_hbox(out, items, rows_cap, width, with_gutter),
    }
}

fn render_leaf(
    out: &mut LineBuilder,
    leaf: &RenderedLeaf,
    rows_cap: u16,
    width: u16,
    with_gutter: bool,
) -> u16 {
    match leaf {
        RenderedLeaf::Buf(buf) => replay_leaf(out, buf, rows_cap, width, with_gutter),
        RenderedLeaf::Diff(spec) => render_diff_spec(out, spec, with_gutter),
        RenderedLeaf::DiffCache(cache) => {
            let indent = if with_gutter {
                BLOCK_GUTTER_W as u16
            } else {
                0
            };
            print_cached_inline_diff(
                out,
                cache,
                GutterStyle::InlineLineNumbers,
                indent,
                0,
                u16::MAX,
            )
        }
        RenderedLeaf::FileView(spec) => render_file_view_spec(out, spec, with_gutter),
    }
}

/// Render a `Diff` spec leaf directly into the worker's block buffer. The
/// 2-cell indent gets baked into the diff renderer (every row gets it), so
/// bg, indent, line numbers, content, and trailing pad all share one render
/// pass and survive the projection seam intact.
fn render_diff_spec(out: &mut LineBuilder, spec: &DiffSpec, with_gutter: bool) -> u16 {
    let ext = spec
        .lang
        .as_deref()
        .map(smelt_core::content::highlight::lang_to_ext);
    let indent = if with_gutter {
        BLOCK_GUTTER_W as u16
    } else {
        0
    };
    print_inline_diff_ext(
        out,
        &spec.old,
        &spec.new,
        &spec.path,
        &spec.anchor,
        ext,
        GutterStyle::InlineLineNumbers,
        indent,
        0,
        u16::MAX,
    )
}

/// Render a `FileView` spec leaf — single-line-number column, no diff bg.
fn render_file_view_spec(out: &mut LineBuilder, spec: &FileViewSpec, with_gutter: bool) -> u16 {
    let ext = spec
        .lang
        .as_deref()
        .map(smelt_core::content::highlight::lang_to_ext)
        .or_else(|| {
            std::path::Path::new(&spec.path)
                .extension()
                .and_then(|e| e.to_str())
        });
    let cache = build_file_view_cache(&spec.content, ext);
    let indent = if with_gutter {
        BLOCK_GUTTER_W as u16
    } else {
        0
    };
    print_cached_inline_diff(
        out,
        &cache,
        GutterStyle::InlineLineNumbers,
        indent,
        0,
        u16::MAX,
    )
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
            out.print_gutter(BLOCK_GUTTER_SPACE);
        }
        out.print(&glyph.repeat(width as usize));
        out.newline();
        return 1;
    }
    let limit = (n as u16).min(rows_cap);
    for i in 0..limit {
        if with_gutter {
            out.print_gutter(BLOCK_GUTTER_SPACE);
        }
        replay_buffer_row_into(buf, i, out);
        out.newline();
    }
    limit
}

fn replay_hbox(
    out: &mut LineBuilder,
    items: &[smelt_core::content::block_layout::RenderedHboxItem],
    rows_cap: u16,
    total_width: u16,
    with_gutter: bool,
) -> u16 {
    use smelt_core::content::block_layout::solve_hbox_widths;
    let widths = solve_hbox_widths(items, total_width);

    let mut columns: Vec<Vec<&smelt_core::buffer::Buffer>> = Vec::with_capacity(items.len());
    let mut col_height: u16 = 0;
    let mut any_unit_only = true;
    for item in items {
        // Hbox columns can only contain buffer leaves; spec leaves (diff/file_view)
        // would need width-dependent height computation that hbox layout can't do
        // ahead of time. The built-in tools that use specs (edit_file, write_file)
        // wrap them in a top-level `Leaf`, not an `Hbox`, so this restriction is
        // invisible to them.
        let bufs: Vec<&smelt_core::buffer::Buffer> = item
            .layout
            .leaves()
            .into_iter()
            .filter_map(|l| match l {
                RenderedLeaf::Buf(b) => Some(&**b),
                _ => None,
            })
            .collect();
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
        col_height = 1; // pure separator row
    }
    let row_total = col_height.min(rows_cap);
    if row_total == 0 {
        return 0;
    }

    for r in 0..row_total {
        if with_gutter {
            out.print_gutter(BLOCK_GUTTER_SPACE);
        }
        for (col_idx, bufs) in columns.iter().enumerate() {
            let col_w = widths.get(col_idx).copied().unwrap_or(0);
            if col_w == 0 {
                continue;
            }
            let emitted = emit_column_row(out, bufs, r, col_w);
            if emitted < col_w {
                out.print(&" ".repeat((col_w - emitted) as usize));
            }
        }
        out.newline();
    }
    row_total
}

fn emit_column_row(
    out: &mut LineBuilder,
    bufs: &[&smelt_core::buffer::Buffer],
    r: u16,
    col_w: u16,
) -> u16 {
    // Unit leaves (1×1) repeat horizontally to fill the column.
    for buf in bufs {
        if is_unit_leaf(buf) {
            let glyph = buf.get_line(0).unwrap_or("");
            let repeat = col_w as usize;
            let s = glyph.repeat(repeat);
            out.print(&s);
            return col_w;
        }
    }
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
    let _perf = smelt_perf::perf::begin("render:wrapped_output");
    let max_cols = super::metrics::block_inner_width(width);

    let wrapped: Vec<String> = content
        .lines()
        .flat_map(|line| {
            let expanded = normalize_terminal_line(line).replace('\t', "    ");
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
            &format!(
                "{BLOCK_GUTTER_SPACE}... {} above",
                pluralize(skipped, "line", "lines")
            ),
        );
        out.newline();
        rows += 1;
    }
    let start = total.saturating_sub(MAX_TOOL_BLOCK_ROWS);
    for seg in &wrapped[start..] {
        if is_error {
            out.push_hl(intern("ErrorMsg"));
            out.print_string(format!("{BLOCK_GUTTER_SPACE}{seg}"));
            out.pop_style();
        } else {
            print_dim(out, &format!("{BLOCK_GUTTER_SPACE}{seg}"));
        }
        out.newline();
        rows += 1;
    }
    rows
}

fn normalize_terminal_line(line: &str) -> String {
    let latest = line.rsplit('\r').next().unwrap_or("");
    strip_ansi_and_controls(latest)
}

fn strip_ansi_and_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        } else if !ch.is_control() {
            out.push(ch);
        }
    }
    out
}

pub(super) fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}
