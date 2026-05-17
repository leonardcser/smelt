//! Inline diff rendering: live `print_inline_diff` for tools that
//! produce a fresh diff per render, plus the persisted `CachedInlineDiff`
//! IR that `edit_file` / `edit_notebook` produce once and replay.

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::path::Path;
use syntect::easy::HighlightLines;

use super::{syntax_theme, GutterStyle, SYNTAX_SET};
use crate::content::builder::LineBuilder;
use crate::content::default_width;
use crate::style::Color;
use smelt_buffer::buffer::SpanMeta;

struct DiffChange {
    tag: ChangeTag,
    value: String,
}

struct DiffViewData {
    file_content: String,
    start_line: usize,
    first_mod: usize,
    view_start: usize,
    view_end: usize,
    max_display_lineno: usize,
    changes: Vec<DiffChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedInlineDiff {
    pub(crate) max_display_lineno: usize,
    pub(crate) lines: Vec<CachedDiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedSpan {
    pub(crate) text: String,
    pub(crate) fg: (u8, u8, u8),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum CachedDiffLine {
    Context {
        lineno: usize,
        text: String,
        spans: Vec<CachedSpan>,
    },
    Delete {
        lineno: usize,
        text: String,
        spans: Vec<CachedSpan>,
    },
    Insert {
        lineno: usize,
        text: String,
        spans: Vec<CachedSpan>,
    },
    Ellipsis,
}

fn cached_spans_for_line(h: &mut HighlightLines, line: &str) -> Vec<CachedSpan> {
    let line_with_nl = format!("{}\n", line);
    h.highlight_line(&line_with_nl, &SYNTAX_SET)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(style, text)| {
            let text = text.trim_end_matches('\n').trim_end_matches('\r');
            if text.is_empty() {
                None
            } else {
                Some(CachedSpan {
                    text: text.to_string(),
                    fg: (style.foreground.r, style.foreground.g, style.foreground.b),
                })
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn build_inline_diff_cache(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
) -> CachedInlineDiff {
    build_inline_diff_cache_ext(old, new, path, anchor, None)
}

/// All-Context cache for a single-file view (write_file, notebook insert,
/// `smelt.syntax.render_file`). Same IR as the diff renderer so a single
/// `print_cached_inline_diff` pipeline serves both — line numbers, wrap math,
/// and bg-spanning all live in one place.
pub fn build_file_view_cache(content: &str, ext: Option<&str>) -> CachedInlineDiff {
    let _perf = smelt_perf::perf::begin("render:build_file_view_cache");
    let ext = ext.unwrap_or("txt");
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = syntax_theme();
    let mut h = HighlightLines::new(syntax, theme);
    let lines: Vec<CachedDiffLine> = content
        .lines()
        .enumerate()
        .map(|(i, line)| CachedDiffLine::Context {
            lineno: i + 1,
            text: line.to_string(),
            spans: cached_spans_for_line(&mut h, line),
        })
        .collect();
    let max_display_lineno = lines.len().max(1);
    CachedInlineDiff {
        max_display_lineno,
        lines,
    }
}

pub fn build_inline_diff_cache_ext(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    syntax_ext: Option<&str>,
) -> CachedInlineDiff {
    let _perf = smelt_perf::perf::begin("render:build_diff_cache");
    let dv = compute_diff_view(old, new, path, anchor);
    let expanded_lines: Vec<String> = dv
        .file_content
        .lines()
        .map(|l| l.replace('\t', "    "))
        .collect();
    let file_lines: Vec<&str> = expanded_lines.iter().map(|s| s.as_str()).collect();
    let lookup = if !anchor.is_empty() { anchor } else { old };
    let lookup_indent = lookup
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let file_indent = file_lines
        .get(dv.start_line)
        .unwrap_or(&"")
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let extra_indent = " ".repeat(file_indent.saturating_sub(lookup_indent));

    let ext = syntax_ext.unwrap_or_else(|| {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("txt")
    });
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = syntax_theme();
    let mut h = HighlightLines::new(syntax, theme);

    let ctx = 3usize;
    let visible = compute_change_visibility(&dv.changes, ctx);
    let mut lines = Vec::new();

    let ctx_before_end = dv.start_line.min(dv.first_mod);
    let ctx_before_start = dv.view_start.min(ctx_before_end);
    for (idx, line) in file_lines[ctx_before_start..ctx_before_end]
        .iter()
        .enumerate()
    {
        lines.push(CachedDiffLine::Context {
            lineno: ctx_before_start + idx + 1,
            text: (*line).to_string(),
            spans: cached_spans_for_line(&mut h, line),
        });
    }

    let mut old_lineno = dv.start_line;
    let mut new_lineno = dv.start_line;
    let mut pending_ellipsis = false;
    let mut emitted_any = !lines.is_empty();
    for (ci, change) in dv.changes.iter().enumerate() {
        let raw = format!(
            "{}{}",
            extra_indent,
            change.value.trim_end_matches('\n').replace('\t', "    ")
        );
        let spans = cached_spans_for_line(&mut h, &raw);
        match change.tag {
            ChangeTag::Equal => {
                if visible[ci] {
                    if pending_ellipsis {
                        pending_ellipsis = false;
                        lines.push(CachedDiffLine::Ellipsis);
                    }
                    if new_lineno >= dv.view_start && new_lineno < dv.view_end {
                        lines.push(CachedDiffLine::Context {
                            lineno: new_lineno + 1,
                            text: raw,
                            spans,
                        });
                        emitted_any = true;
                    }
                } else if emitted_any {
                    pending_ellipsis = true;
                }
                old_lineno += 1;
                new_lineno += 1;
            }
            ChangeTag::Delete => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(CachedDiffLine::Ellipsis);
                }
                lines.push(CachedDiffLine::Delete {
                    lineno: old_lineno + 1,
                    text: raw,
                    spans,
                });
                old_lineno += 1;
            }
            ChangeTag::Insert => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(CachedDiffLine::Ellipsis);
                }
                lines.push(CachedDiffLine::Insert {
                    lineno: new_lineno + 1,
                    text: raw,
                    spans,
                });
                new_lineno += 1;
            }
        }
    }

    let anchor_lines = anchor.lines().count();
    let after_start = dv.start_line + anchor_lines;
    let after_end = dv.view_end.min(file_lines.len());
    for (idx, line) in file_lines
        .iter()
        .take(after_end)
        .skip(after_start)
        .enumerate()
    {
        lines.push(CachedDiffLine::Context {
            lineno: after_start + idx + 1,
            text: (*line).to_string(),
            spans: cached_spans_for_line(&mut h, line),
        });
    }

    CachedInlineDiff {
        max_display_lineno: dv.max_display_lineno,
        lines,
    }
}

#[cfg(test)]
fn change(tag: ChangeTag, value: &str) -> DiffChange {
    DiffChange {
        tag,
        value: value.to_string(),
    }
}

fn compute_diff_view(old: &str, new: &str, path: &str, anchor: &str) -> DiffViewData {
    let file_content = std::fs::read_to_string(path).unwrap_or_default();
    let file_lines_count = file_content.lines().count();
    let lookup = if !anchor.is_empty() {
        anchor
    } else if !old.is_empty() {
        old
    } else {
        new
    };
    let start_line = if lookup.is_empty() {
        0
    } else {
        file_content
            .find(lookup)
            .map(|pos| file_content[..pos].bytes().filter(|&b| b == b'\n').count())
            .unwrap_or(0)
    };

    let diff = TextDiff::from_lines(old, new);
    let changes: Vec<DiffChange> = diff
        .iter_all_changes()
        .map(|c| DiffChange {
            tag: c.tag(),
            value: c.value().to_string(),
        })
        .collect();
    let ctx = 3usize;
    let mut first_mod: Option<usize> = None;
    let mut last_mod: Option<usize> = None;
    let mut new_line = start_line;
    let mut old_line = start_line;
    for c in &changes {
        match c.tag {
            ChangeTag::Equal => {
                new_line += 1;
                old_line += 1;
            }
            ChangeTag::Delete => {
                if first_mod.is_none() {
                    first_mod = Some(new_line);
                }
                last_mod = Some(new_line);
                old_line += 1;
            }
            ChangeTag::Insert => {
                if first_mod.is_none() {
                    first_mod = Some(new_line);
                }
                last_mod = Some(new_line);
                new_line += 1;
            }
        }
    }
    let first_mod = first_mod.unwrap_or(start_line);
    let last_mod = last_mod.unwrap_or(start_line);
    let view_start = first_mod.saturating_sub(ctx);
    let view_end = (last_mod + 1 + ctx).min(file_lines_count);
    let max_display_lineno = view_end.max(old_line).max(new_line);

    DiffViewData {
        file_content,
        start_line,
        first_mod,
        view_start,
        view_end,
        max_display_lineno,
        changes,
    }
}

/// Mark equal lines within `ctx` of any non-Equal change as visible; collapse the rest.
fn compute_change_visibility(changes: &[DiffChange], ctx: usize) -> Vec<bool> {
    let n = changes.len();
    let mut visible = vec![false; n];
    let mut d = usize::MAX;
    // Forward pass: visible based on distance from preceding non-Equal.
    for i in 0..n {
        if changes[i].tag != ChangeTag::Equal {
            d = 0;
            visible[i] = true;
        } else {
            visible[i] = d <= ctx;
        }
        d = d.saturating_add(1);
    }
    // Backward pass: also catch equal lines near a following non-Equal.
    d = usize::MAX;
    for i in (0..n).rev() {
        if changes[i].tag != ChangeTag::Equal {
            d = 0;
        } else if d <= ctx {
            visible[i] = true;
        }
        d = d.saturating_add(1);
    }
    visible
}

/// Render a syntax-highlighted inline diff; `skip` rows are skipped, at most `max_rows` emitted.
/// Syntax is inferred from `path`'s extension.
#[allow(clippy::too_many_arguments)]
pub fn print_inline_diff(
    out: &mut LineBuilder,
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    gutter: GutterStyle,
    skip: u16,
    max_rows: u16,
) -> u16 {
    print_inline_diff_ext(out, old, new, path, anchor, None, gutter, 0, skip, max_rows)
}

/// Like [`print_inline_diff`] but with an explicit syntect language/extension token
/// (bypasses the `path`-based extension sniff) and `indent_cells` of non-selectable
/// leading indent per row — used by the tool-block worker to align diff content with
/// the tool name's column without a separate replay-time wrapper.
#[allow(clippy::too_many_arguments)]
pub fn print_inline_diff_ext(
    out: &mut LineBuilder,
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    syntax_ext: Option<&str>,
    gutter: GutterStyle,
    indent_cells: u16,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:inline_diff_cold");
    let cache = build_inline_diff_cache_ext(old, new, path, anchor, syntax_ext);
    print_cached_inline_diff(out, &cache, gutter, indent_cells, skip, max_rows)
}

fn print_cached_spans(out: &mut LineBuilder, spans: &[CachedSpan], bg: Option<Color>) -> usize {
    let mut col = 0;
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        if let Some(bg_color) = bg {
            out.set_bg(bg_color);
        }
        out.set_fg(Color::Rgb {
            r: span.fg.0,
            g: span.fg.1,
            b: span.fg.2,
        });
        out.print(&span.text);
        col += span.text.chars().count();
    }
    out.reset_style();
    col
}

fn split_cached_spans_into_rows(
    out: &mut LineBuilder,
    spans: &[CachedSpan],
    max_width: usize,
) -> Vec<Vec<CachedSpan>> {
    let _ = out;
    let max_width = max_width.max(1);
    let mut rows: Vec<Vec<CachedSpan>> = Vec::new();
    let mut current_row: Vec<CachedSpan> = Vec::new();
    let mut col = 0;

    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let mut chars = span.text.chars().peekable();
        while chars.peek().is_some() {
            let remaining = max_width.saturating_sub(col);
            if remaining == 0 {
                rows.push(std::mem::take(&mut current_row));
                col = 0;
                continue;
            }
            let chunk: String = chars.by_ref().take(remaining).collect();
            col += chunk.chars().count();
            current_row.push(CachedSpan {
                text: chunk,
                fg: span.fg,
            });
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

pub fn print_cached_inline_diff(
    out: &mut LineBuilder,
    cache: &CachedInlineDiff,
    gutter: GutterStyle,
    indent_cells: u16,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:inline_diff_cached");

    // Single line-number column (` N `). The body sign ("+ "/"- "/"  ") is always
    // written inline regardless of mode. `Stamped` hands off the line number via
    // `SourceLine`; the host window's `LineNumberGutter` paints the column outside
    // the content width — so the in-content prefix is just the sign.
    let lineno_digits = format!("{}", cache.max_display_lineno).len();
    let prefix_cells = match gutter {
        GutterStyle::Stamped => 0,
        GutterStyle::InlineLineNumbers => lineno_digits + 2,
        GutterStyle::None => 0,
    };
    let sign_prefix = 2;
    let indent = indent_cells as usize;
    let indent_str = " ".repeat(indent);
    let layout_width = if out.layout_width() == 0 {
        default_width()
    } else {
        out.layout_width() as usize
    };
    let max_content = layout_width
        .saturating_sub(indent + prefix_cells + sign_prefix)
        .max(1);
    let blank_prefix = " ".repeat(prefix_cells);
    // Content re-wraps per row at `layout_width`, so the layout is width-pinned.
    out.mark_wrapped();
    let emit_limit = if max_rows == 0 { u16::MAX } else { max_rows };
    let bg_del = Color::Rgb {
        r: 60,
        g: 20,
        b: 20,
    };
    let bg_add = Color::Rgb {
        r: 20,
        g: 50,
        b: 20,
    };

    let mut emitted = 0u16;
    for (row_idx, line) in cache.lines.iter().enumerate() {
        if row_idx < skip as usize {
            continue;
        }
        if emitted >= emit_limit {
            break;
        }
        match line {
            CachedDiffLine::Ellipsis => {
                if indent > 0 {
                    out.print_gutter(&indent_str);
                }
                if matches!(gutter, GutterStyle::Stamped) {
                    out.set_source_line(smelt_buffer::buffer::SourceLine::Synthetic);
                } else if prefix_cells > 0 {
                    out.print(&blank_prefix);
                }
                out.set_fg(Color::DarkGrey);
                out.print("...");
                out.reset_style();
                out.newline();
            }
            _ => {
                let (source_line, sign, bg, spans) = match line {
                    CachedDiffLine::Context { lineno, spans, .. } => (
                        smelt_buffer::buffer::SourceLine::Linear {
                            lineno: *lineno as u32,
                        },
                        None,
                        None,
                        spans,
                    ),
                    CachedDiffLine::Delete { lineno, spans, .. } => (
                        smelt_buffer::buffer::SourceLine::Linear {
                            lineno: *lineno as u32,
                        },
                        Some(('-', Color::Red)),
                        Some(bg_del),
                        spans,
                    ),
                    CachedDiffLine::Insert { lineno, spans, .. } => (
                        smelt_buffer::buffer::SourceLine::Linear {
                            lineno: *lineno as u32,
                        },
                        Some(('+', Color::Green)),
                        Some(bg_add),
                        spans,
                    ),
                    CachedDiffLine::Ellipsis => unreachable!(),
                };
                let line_fg = sign.map(|(_, color)| color);
                let visual_rows = split_cached_spans_into_rows(out, spans, max_content);
                let pad_meta = SpanMeta {
                    selectable: false,
                    copy_as: None,
                };
                for (vi, vrow) in visual_rows.iter().enumerate() {
                    // For delete/insert rows the bg extends under the indent
                    // (the leftmost cells of the row) and across the trailing
                    // pad, so the strip reads as a single change-band. Indent
                    // and pad cells stay non-selectable; `pad_row_to_layout_width`
                    // emits the trailing spaces with the active bg.
                    if let Some(bgv) = bg {
                        out.set_bg(bgv);
                    }
                    if indent > 0 {
                        out.print_gutter(&indent_str);
                    }
                    emit_diff_prefix(
                        out,
                        gutter,
                        source_line,
                        vi,
                        lineno_digits,
                        &blank_prefix,
                        line_fg,
                    );
                    if let Some((ch, color)) = sign {
                        let bgv = bg.unwrap();
                        out.set_bg(bgv);
                        if vi == 0 {
                            out.set_fg(color);
                            out.print(&format!("{} ", ch));
                        } else {
                            out.print("  ");
                        }
                        print_cached_spans(out, vrow, bg);
                        out.set_bg(bgv);
                        out.pad_row_to_layout_width(pad_meta.clone());
                        out.reset_style();
                    } else {
                        out.print("  ");
                        print_cached_spans(out, vrow, None);
                    }
                    out.newline();
                }
            }
        }
        emitted += 1;
    }
    emitted
}

/// Emit the diff row's left-of-content prefix per the chosen gutter style. `Stamped`
/// hands off via `SourceLine` (host window's `LineNumberGutter` paints the column);
/// `InlineLineNumbers` writes ` N ` as text — coloured `line_fg` when set (red/green
/// for delete/insert rows so the gutter shares the row's diff tint), otherwise the
/// theme's `Comment` group; `None` writes nothing.
fn emit_diff_prefix(
    out: &mut LineBuilder,
    gutter: GutterStyle,
    source_line: smelt_buffer::buffer::SourceLine,
    vi: usize,
    lineno_digits: usize,
    blank_prefix: &str,
    line_fg: Option<Color>,
) {
    match gutter {
        GutterStyle::Stamped => {
            out.stamp_chunk(vi, source_line);
        }
        GutterStyle::InlineLineNumbers => {
            match line_fg {
                Some(fg) => out.push_fg(fg),
                None => out.push_hl(crate::theme::intern("Comment")),
            }
            if vi == 0 {
                let lineno = match source_line {
                    smelt_buffer::buffer::SourceLine::Linear { lineno } => Some(lineno),
                    smelt_buffer::buffer::SourceLine::Diff { new, .. } => new,
                    _ => None,
                };
                out.print_gutter(&format!(
                    " {:>w$} ",
                    lineno.map(|n| n.to_string()).unwrap_or_default(),
                    w = lineno_digits
                ));
            } else {
                out.print_gutter(blank_prefix);
            }
            out.pop_style();
        }
        GutterStyle::None => {}
    }
}

// ─── Side-by-side diff renderer ──────────────────────────────────────
//
// Decoupled from output: `compute_split_diff` returns a `SplitDiffPlan`
// describing every row on both sides; `print_split_diff_side` paints one
// side from the plan. Two passes share the same plan, so callers can
// render to two buffers without holding both `LineBuilder`s live at once.
// The convenience `print_split_diff` wraps both passes when the caller
// can hand over both sinks simultaneously (used in tests).

/// One visual row of a side-by-side diff. Synthesised lines (padding
/// where one side has fewer rows in a delete/insert block) are encoded as
/// `None`; concrete rows carry the source line number for the gutter and
/// a `removed` flag that controls per-line bg + syntax-highlight scope.
#[derive(Clone, Debug)]
pub struct SplitDiffRow {
    pub left: Option<SplitDiffCell>,
    pub right: Option<SplitDiffCell>,
}

#[derive(Clone, Debug)]
pub struct SplitDiffCell {
    pub text: String,
    pub lineno: u32,
    /// True for delete/insert rows (paints the row-fill bg); false for
    /// `Equal` context rows.
    pub changed: bool,
}

/// Self-contained diff IR. Computed once, then replayed per side.
#[derive(Clone, Debug)]
pub struct SplitDiffPlan {
    pub rows: Vec<SplitDiffRow>,
}

/// Walk `old` vs `new` at line granularity and produce the aligned
/// row plan. Consecutive delete/insert blocks are paired one-to-one
/// (zip); whichever side has fewer rows in the block gets `None`
/// padding to keep both sides on the same visual row.
pub fn compute_split_diff(old: &str, new: &str) -> SplitDiffPlan {
    let _perf = smelt_perf::perf::begin("render:compute_split_diff");
    let diff = TextDiff::from_lines(old, new);
    let mut rows: Vec<SplitDiffRow> = Vec::new();
    let mut old_lineno: u32 = 0;
    let mut new_lineno: u32 = 0;
    let mut pending_dels: Vec<SplitDiffCell> = Vec::new();
    let mut pending_ins: Vec<SplitDiffCell> = Vec::new();

    let flush = |rows: &mut Vec<SplitDiffRow>,
                 dels: &mut Vec<SplitDiffCell>,
                 ins: &mut Vec<SplitDiffCell>| {
        let n = dels.len().max(ins.len());
        for i in 0..n {
            rows.push(SplitDiffRow {
                left: dels.get(i).cloned(),
                right: ins.get(i).cloned(),
            });
        }
        dels.clear();
        ins.clear();
    };

    for change in diff.iter_all_changes() {
        let text = change.value().trim_end_matches('\n').replace('\t', "    ");
        match change.tag() {
            ChangeTag::Equal => {
                flush(&mut rows, &mut pending_dels, &mut pending_ins);
                old_lineno += 1;
                new_lineno += 1;
                let left = SplitDiffCell {
                    text: text.clone(),
                    lineno: old_lineno,
                    changed: false,
                };
                let right = SplitDiffCell {
                    text,
                    lineno: new_lineno,
                    changed: false,
                };
                rows.push(SplitDiffRow {
                    left: Some(left),
                    right: Some(right),
                });
            }
            ChangeTag::Delete => {
                old_lineno += 1;
                pending_dels.push(SplitDiffCell {
                    text,
                    lineno: old_lineno,
                    changed: true,
                });
            }
            ChangeTag::Insert => {
                new_lineno += 1;
                pending_ins.push(SplitDiffCell {
                    text,
                    lineno: new_lineno,
                    changed: true,
                });
            }
        }
    }
    flush(&mut rows, &mut pending_dels, &mut pending_ins);
    SplitDiffPlan { rows }
}

/// Which side of the plan to render.
#[derive(Clone, Copy)]
pub enum SplitSide {
    Left,
    Right,
}

fn paint_diff_line(
    out: &mut LineBuilder,
    text: &str,
    h: &mut HighlightLines,
    bg: Option<Color>,
    source_line: smelt_buffer::buffer::SourceLine,
) {
    if let Some(bg) = bg {
        out.fill_line_bg(bg);
    }
    out.set_source_line(source_line);
    let line_with_nl = format!("{}\n", text);
    if let Ok(regions) = h.highlight_line(&line_with_nl, &SYNTAX_SET) {
        for (style, span) in regions {
            let span = span.trim_end_matches('\n').trim_end_matches('\r');
            if span.is_empty() {
                continue;
            }
            out.set_fg(Color::Rgb {
                r: style.foreground.r,
                g: style.foreground.g,
                b: style.foreground.b,
            });
            out.print(span);
        }
    }
    out.reset_style();
    out.newline();
}

fn paint_synthetic_row(out: &mut LineBuilder) {
    out.set_source_line(smelt_buffer::buffer::SourceLine::Synthetic);
    out.newline();
}

/// Render one side of `plan` into `out`. Delete rows get the dark-red
/// row fill on the left side, insert rows the dark-green fill on the
/// right; equal rows are highlighted but not filled; `None` cells emit
/// a synthetic padding row.
pub fn print_split_diff_side(
    out: &mut LineBuilder,
    plan: &SplitDiffPlan,
    syntax_ext: Option<&str>,
    side: SplitSide,
) {
    let _perf = smelt_perf::perf::begin("render:split_diff_side");
    let ext = syntax_ext.unwrap_or("txt");
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = syntax_theme();
    let mut h = HighlightLines::new(syntax, theme);
    let bg_changed = match side {
        SplitSide::Left => Color::Rgb {
            r: 60,
            g: 20,
            b: 20,
        },
        SplitSide::Right => Color::Rgb {
            r: 20,
            g: 50,
            b: 20,
        },
    };
    for row in &plan.rows {
        let cell = match side {
            SplitSide::Left => row.left.as_ref(),
            SplitSide::Right => row.right.as_ref(),
        };
        match cell {
            Some(c) => paint_diff_line(
                out,
                &c.text,
                &mut h,
                if c.changed { Some(bg_changed) } else { None },
                smelt_buffer::buffer::SourceLine::Linear { lineno: c.lineno },
            ),
            None => paint_synthetic_row(out),
        }
    }
}

/// Convenience: render both sides in one call when the caller can hold
/// both `LineBuilder`s simultaneously. Equivalent to computing the plan
/// once and calling [`print_split_diff_side`] for each side.
pub fn print_split_diff(
    left: &mut LineBuilder,
    right: &mut LineBuilder,
    old: &str,
    new: &str,
    syntax_ext: Option<&str>,
) {
    let plan = compute_split_diff(old, new);
    print_split_diff_side(left, &plan, syntax_ext, SplitSide::Left);
    print_split_diff_side(right, &plan, syntax_ext, SplitSide::Right);
}

#[cfg(test)]
mod split_tests {
    use super::*;
    use crate::content::builder::render_into;
    use smelt_buffer::buffer::{BufCreateOpts, BufId, Buffer, SourceLine};
    use smelt_buffer::theme::Theme;

    fn split(old: &str, new: &str) -> (Buffer, Buffer) {
        let theme = Theme::default();
        let mut left = Buffer::new(BufId(0), BufCreateOpts::default());
        let mut right = Buffer::new(BufId(1), BufCreateOpts::default());
        render_into(&mut left, 80, &theme, |lsink| {
            render_into(&mut right, 80, &theme, |rsink| {
                print_split_diff(lsink, rsink, old, new, Some("txt"));
            });
        });
        (left, right)
    }

    fn texts(buf: &Buffer) -> Vec<String> {
        (0..buf.line_count())
            .map(|r| buf.get_line(r).unwrap_or("").to_string())
            .collect()
    }

    fn source_lines(buf: &Buffer) -> Vec<Option<SourceLine>> {
        (0..buf.line_count())
            .map(|r| buf.decoration_at(r).source_line)
            .collect()
    }

    #[test]
    fn split_diff_pairs_one_to_one_change() {
        let (l, r) = split("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(l.line_count(), r.line_count());
        assert_eq!(texts(&l), vec!["alpha", "beta", "gamma"]);
        assert_eq!(texts(&r), vec!["alpha", "BETA", "gamma"]);
        for sl in source_lines(&l).iter().chain(source_lines(&r).iter()) {
            assert!(matches!(sl, Some(SourceLine::Linear { .. })));
        }
    }

    #[test]
    fn split_diff_pads_with_synthetic_when_one_side_only_inserts() {
        let (l, r) = split("alpha\ngamma\n", "alpha\nNEW\ngamma\n");
        assert_eq!(l.line_count(), r.line_count());
        assert_eq!(l.line_count(), 3);
        assert!(matches!(source_lines(&l)[1], Some(SourceLine::Synthetic)));
    }

    #[test]
    fn split_diff_pads_with_synthetic_when_one_side_only_deletes() {
        let (l, r) = split("alpha\nOLD\ngamma\n", "alpha\ngamma\n");
        assert_eq!(l.line_count(), r.line_count());
        assert_eq!(l.line_count(), 3);
        assert!(matches!(source_lines(&r)[1], Some(SourceLine::Synthetic)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::builder::test_util::render_test;

    fn count_lines(cache: &CachedInlineDiff) -> (usize, usize, usize, usize) {
        let (mut ctx, mut del, mut ins, mut ell) = (0, 0, 0, 0);
        for l in &cache.lines {
            match l {
                CachedDiffLine::Context { .. } => ctx += 1,
                CachedDiffLine::Delete { .. } => del += 1,
                CachedDiffLine::Insert { .. } => ins += 1,
                CachedDiffLine::Ellipsis => ell += 1,
            }
        }
        (ctx, del, ins, ell)
    }

    #[test]
    fn compute_change_visibility_marks_all_equal_lines_when_no_changes() {
        let changes = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Equal, "b"),
            change(ChangeTag::Equal, "c"),
        ];
        let vis = compute_change_visibility(&changes, 3);
        assert_eq!(vis, vec![false, false, false]);
    }

    #[test]
    fn compute_change_visibility_expands_context_around_modifications() {
        // Indices:        0       1       2       3       4       5       6       7
        let changes = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Equal, "b"),
            change(ChangeTag::Equal, "c"),
            change(ChangeTag::Equal, "d"),
            change(ChangeTag::Delete, "x"),
            change(ChangeTag::Equal, "e"),
            change(ChangeTag::Equal, "f"),
            change(ChangeTag::Equal, "g"),
        ];
        let vis = compute_change_visibility(&changes, 2);
        // ctx=2: lines within 2 of the Delete (idx 4) are visible
        // before: 2,3 visible; 0,1 hidden
        // delete: 4 visible
        // after: 5,6 visible; 7 hidden
        assert_eq!(vis, vec![false, false, true, true, true, true, true, false]);
    }

    #[test]
    fn compute_change_visibility_with_zero_context_marks_only_changes() {
        let changes = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Insert, "x"),
            change(ChangeTag::Equal, "b"),
        ];
        let vis = compute_change_visibility(&changes, 0);
        assert_eq!(vis, vec![false, true, false]);
    }

    #[test]
    fn compute_change_visibility_handles_empty_changes() {
        let vis = compute_change_visibility(&[], 3);
        assert!(vis.is_empty());
    }

    #[test]
    fn compute_change_visibility_treats_insert_and_delete_symmetrically() {
        let with_insert = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Equal, "b"),
            change(ChangeTag::Insert, "+"),
            change(ChangeTag::Equal, "c"),
        ];
        let with_delete = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Equal, "b"),
            change(ChangeTag::Delete, "-"),
            change(ChangeTag::Equal, "c"),
        ];
        assert_eq!(
            compute_change_visibility(&with_insert, 1),
            compute_change_visibility(&with_delete, 1)
        );
    }

    #[test]
    fn split_cached_spans_into_rows_wraps_at_max_width() {
        let block = render_test(80, |out| {
            let spans = vec![CachedSpan {
                text: "abcdefghij".to_string(),
                fg: (10, 20, 30),
            }];
            let rows = split_cached_spans_into_rows(out, &spans, 4);
            assert_eq!(rows.len(), 3);
            let concat: String = rows
                .iter()
                .flat_map(|r| r.iter().map(|s| s.text.as_str()))
                .collect();
            assert_eq!(concat, "abcdefghij");
            assert_eq!(rows[0][0].text, "abcd");
            assert_eq!(rows[1][0].text, "efgh");
            assert_eq!(rows[2][0].text, "ij");
            // Color preserved on every chunk.
            for row in &rows {
                for span in row {
                    assert_eq!(span.fg, (10, 20, 30));
                }
            }
        });
        assert_eq!(block.outcome.line_count, 0);
    }

    #[test]
    fn split_cached_spans_into_rows_ignores_empty_spans() {
        render_test(80, |out| {
            let spans = vec![
                CachedSpan {
                    text: "".to_string(),
                    fg: (0, 0, 0),
                },
                CachedSpan {
                    text: "ab".to_string(),
                    fg: (1, 2, 3),
                },
            ];
            let rows = split_cached_spans_into_rows(out, &spans, 10);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].len(), 1);
            assert_eq!(rows[0][0].text, "ab");
        });
    }

    #[test]
    fn split_cached_spans_into_rows_clamps_max_width_to_one() {
        render_test(80, |out| {
            let spans = vec![CachedSpan {
                text: "ab".to_string(),
                fg: (0, 0, 0),
            }];
            let rows = split_cached_spans_into_rows(out, &spans, 0);
            // max_width clamped to 1
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0].text, "a");
            assert_eq!(rows[1][0].text, "b");
        });
    }

    #[test]
    fn split_cached_spans_into_rows_emits_empty_row_for_empty_input() {
        render_test(80, |out| {
            let rows = split_cached_spans_into_rows(out, &[], 4);
            assert_eq!(rows.len(), 1);
            assert!(rows[0].is_empty());
        });
    }

    #[test]
    fn cached_spans_for_line_returns_non_empty_for_plain_text() {
        let theme = syntax_theme();
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let spans = cached_spans_for_line(&mut h, "hello world");
        assert!(!spans.is_empty());
        let concat: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(concat, "hello world");
    }

    #[test]
    fn cached_spans_for_line_strips_trailing_newline_and_cr() {
        let theme = syntax_theme();
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let spans = cached_spans_for_line(&mut h, "ab");
        let concat: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!concat.ends_with('\n'));
        assert!(!concat.ends_with('\r'));
    }

    #[test]
    fn build_inline_diff_cache_empty_inputs_produce_no_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();
        let cache = build_inline_diff_cache("", "", path.to_str().unwrap(), "");
        let (ctx, del, ins, _) = count_lines(&cache);
        assert_eq!((ctx, del, ins), (0, 0, 0));
    }

    #[test]
    fn build_inline_diff_cache_pure_insert_emits_insert_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "new line\n").unwrap();
        let cache = build_inline_diff_cache("", "new line\n", path.to_str().unwrap(), "");
        let (_, del, ins, _) = count_lines(&cache);
        assert!(
            ins >= 1,
            "expected at least one insert, got cache={cache:?}"
        );
        assert_eq!(del, 0);
    }

    #[test]
    fn build_inline_diff_cache_pure_delete_emits_delete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "").unwrap();
        let cache = build_inline_diff_cache("removed\n", "", path.to_str().unwrap(), "removed\n");
        let (_, del, ins, _) = count_lines(&cache);
        assert!(del >= 1);
        assert_eq!(ins, 0);
    }

    #[test]
    fn build_inline_diff_cache_replacement_produces_delete_and_insert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "line one\nline two\nline three\n").unwrap();
        let cache = build_inline_diff_cache(
            "line two\n",
            "LINE TWO\n",
            path.to_str().unwrap(),
            "line two\n",
        );
        let (_, del, ins, _) = count_lines(&cache);
        assert!(del >= 1);
        assert!(ins >= 1);
        // Surrounding context lines should be present.
        let saw_context = cache
            .lines
            .iter()
            .any(|l| matches!(l, CachedDiffLine::Context { .. }));
        assert!(saw_context);
    }

    #[test]
    fn build_inline_diff_cache_anchor_locates_position_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(
            &path,
            "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n",
        )
        .unwrap();
        let cache =
            build_inline_diff_cache("gamma\n", "GAMMA\n", path.to_str().unwrap(), "gamma\n");
        // The first emitted line number for the delete should be near gamma (line 3).
        let first_lineno = cache.lines.iter().find_map(|l| match l {
            CachedDiffLine::Delete { lineno, .. } => Some(*lineno),
            _ => None,
        });
        assert_eq!(first_lineno, Some(3));
    }

    #[test]
    fn build_inline_diff_cache_falls_back_when_path_missing() {
        let cache = build_inline_diff_cache(
            "foo\n",
            "bar\n",
            "/nonexistent/path/that/does/not/exist",
            "",
        );
        let (_, del, ins, _) = count_lines(&cache);
        assert!(del >= 1);
        assert!(ins >= 1);
    }

    #[test]
    fn build_inline_diff_cache_ext_uses_overridden_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.unknownext");
        std::fs::write(&path, "let x = 1;\nlet y = 2;\n").unwrap();
        let cache_plain = build_inline_diff_cache_ext(
            "let x = 1;\n",
            "let x = 99;\n",
            path.to_str().unwrap(),
            "let x = 1;\n",
            None,
        );
        let cache_rs = build_inline_diff_cache_ext(
            "let x = 1;\n",
            "let x = 99;\n",
            path.to_str().unwrap(),
            "let x = 1;\n",
            Some("rs"),
        );
        // Both produce structurally similar diffs; rs-highlighted cache should have
        // at least as many span chunks because keywords/punctuation get split out.
        let span_count = |c: &CachedInlineDiff| -> usize {
            c.lines
                .iter()
                .map(|l| match l {
                    CachedDiffLine::Context { spans, .. }
                    | CachedDiffLine::Delete { spans, .. }
                    | CachedDiffLine::Insert { spans, .. } => spans.len(),
                    CachedDiffLine::Ellipsis => 0,
                })
                .sum()
        };
        assert!(span_count(&cache_rs) >= span_count(&cache_plain));
    }

    #[test]
    fn build_inline_diff_cache_collapses_distant_unchanged_lines_with_ellipsis() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        // Long file so view_end caps off; we still expect the cache to be non-trivial.
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let mut old = String::new();
        for i in 0..40 {
            old.push_str(&format!("line {i}\n"));
        }
        let mut new = old.clone();
        new = new
            .replace("line 5", "LINE 5")
            .replace("line 30", "LINE 30");
        let cache = build_inline_diff_cache(&old, &new, path.to_str().unwrap(), "");
        let (_, _, _, ell) = count_lines(&cache);
        assert!(ell >= 1, "expected at least one ellipsis collapse");
    }

    #[test]
    fn print_cached_inline_diff_zero_max_rows_emits_no_limit() {
        let cache = CachedInlineDiff {
            max_display_lineno: 1,
            lines: vec![CachedDiffLine::Context {
                lineno: 1,
                text: "x".to_string(),
                spans: vec![CachedSpan {
                    text: "x".to_string(),
                    fg: (200, 200, 200),
                }],
            }],
        };
        let block = render_test(80, |out| {
            let emitted = print_cached_inline_diff(out, &cache, GutterStyle::Stamped, 0, 0, 0);
            // 0 means "no limit" — should emit the single line.
            assert_eq!(emitted, 1);
        });
        assert!(block.outcome.line_count >= 1);
    }

    #[test]
    fn print_cached_inline_diff_respects_max_rows() {
        let cache = CachedInlineDiff {
            max_display_lineno: 3,
            lines: (1..=3)
                .map(|i| CachedDiffLine::Context {
                    lineno: i,
                    text: format!("line{i}"),
                    spans: vec![CachedSpan {
                        text: format!("line{i}"),
                        fg: (0, 0, 0),
                    }],
                })
                .collect(),
        };
        render_test(80, |out| {
            let emitted = print_cached_inline_diff(out, &cache, GutterStyle::Stamped, 0, 0, 2);
            assert_eq!(emitted, 2);
        });
    }

    #[test]
    fn print_cached_inline_diff_skips_leading_rows() {
        let cache = CachedInlineDiff {
            max_display_lineno: 3,
            lines: (1..=3)
                .map(|i| CachedDiffLine::Context {
                    lineno: i,
                    text: format!("line{i}"),
                    spans: vec![CachedSpan {
                        text: format!("line{i}"),
                        fg: (0, 0, 0),
                    }],
                })
                .collect(),
        };
        let block = render_test(80, |out| {
            let emitted = print_cached_inline_diff(out, &cache, GutterStyle::Stamped, 0, 2, 0);
            assert_eq!(emitted, 1);
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("line3"));
        assert!(!joined.contains("line1"));
        assert!(!joined.contains("line2"));
    }

    #[test]
    fn print_cached_inline_diff_renders_delete_insert_and_ellipsis_markers() {
        let span = |s: &str| CachedSpan {
            text: s.to_string(),
            fg: (200, 200, 200),
        };
        let cache = CachedInlineDiff {
            max_display_lineno: 10,
            lines: vec![
                CachedDiffLine::Context {
                    lineno: 1,
                    text: "ctx".to_string(),
                    spans: vec![span("ctx")],
                },
                CachedDiffLine::Ellipsis,
                CachedDiffLine::Delete {
                    lineno: 5,
                    text: "old".to_string(),
                    spans: vec![span("old")],
                },
                CachedDiffLine::Insert {
                    lineno: 5,
                    text: "new".to_string(),
                    spans: vec![span("new")],
                },
            ],
        };
        let block = render_test(80, |out| {
            print_cached_inline_diff(out, &cache, GutterStyle::InlineLineNumbers, 0, 0, 0);
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("ctx"));
        assert!(joined.contains("old"));
        assert!(joined.contains("new"));
        assert!(joined.contains("..."));
        assert!(joined.contains('-'));
        assert!(joined.contains('+'));
    }

    #[test]
    fn print_inline_diff_end_to_end_emits_visible_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {\n    let x = 1;\n}\n").unwrap();
        let block = render_test(80, |out| {
            let emitted = print_inline_diff(
                out,
                "    let x = 1;\n",
                "    let x = 42;\n",
                path.to_str().unwrap(),
                "    let x = 1;\n",
                GutterStyle::InlineLineNumbers,
                0,
                0,
            );
            assert!(emitted > 0);
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Delete and insert markers should both appear.
        assert!(joined.contains('-'));
        assert!(joined.contains('+'));
    }
}
