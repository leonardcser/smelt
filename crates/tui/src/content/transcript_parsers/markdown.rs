use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::{render_code_block, render_markdown_table};
use smelt_core::theme::role_hl;

pub fn render_markdown_inner(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
) -> u16 {
    let _perf = smelt_core::perf::begin("render:markdown");
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(indent.len() + 1)
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    let mut rows = 0u16;
    let mut last_content_line: Option<&str> = None;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("```") {
            let prev_blank = i > 0 && lines[i - 1].trim().is_empty();
            let after_heading = last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
            if rows > 0 && !prev_blank && !after_heading {
                out.newline();
                rows += 1;
            }
            let lang = lines[i].trim_start().trim_start_matches('`').trim();
            i += 1;
            let code_start = i;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                i += 1;
            }
            let code_lines = &lines[code_start..i];
            if i < lines.len() {
                i += 1;
            }
            rows += render_code_block(out, code_lines, lang, width, dim, bctx, true);
            last_content_line = None;
        } else if lines[i].trim_start().starts_with('|') {
            let table_start = i;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                i += 1;
            }
            rows +=
                render_markdown_table_from_lines(out, &lines[table_start..i], dim, bctx, indent);
            last_content_line = None;
        } else if is_horizontal_rule(lines[i]) {
            let prev_blank = i > 0 && lines[i - 1].trim().is_empty();
            let after_heading = last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
            if rows > 0 && !prev_blank && !after_heading {
                out.newline();
                rows += 1;
            }
            rows += render_horizontal_rule(out, bctx, indent);
            let mut next_i = i + 1;
            while next_i < lines.len() && lines[next_i].trim().is_empty() {
                next_i += 1;
            }
            let next_is_heading =
                next_i < lines.len() && lines[next_i].trim_start().starts_with('#');
            if next_i < lines.len() && !next_is_heading && !lines[next_i].trim().is_empty() {
                out.newline();
                rows += 1;
            }
            last_content_line = None;
            i += 1;
        } else {
            if lines[i].trim().is_empty() {
                // Skip blank lines after headings (no trailing gap) and before list items.
                let after_heading =
                    last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
                if after_heading {
                    i += 1;
                    continue;
                }
                let mut next_i = i + 1;
                while next_i < lines.len() && lines[next_i].trim().is_empty() {
                    next_i += 1;
                }
                if next_i < lines.len() && is_list_item(lines[next_i]) {
                    i += 1;
                    continue;
                }
            } else {
                last_content_line = Some(lines[i]);
            }
            let trimmed = lines[i].trim_start();
            {
                use smelt_core::content::highlight::{
                    emit_inline_spans, inline_spans_width, parse_inline_spans, wrap_inline_spans,
                    InlineSpan, InlineStyle,
                };
                let leading_ws = &lines[i][..lines[i].len() - trimmed.len()];
                let mut line_spans: Vec<InlineSpan> = Vec::new();

                if trimmed.starts_with('#') {
                    line_spans.push(InlineSpan {
                        text: trimmed.to_string(),
                        style: InlineStyle {
                            bold: true,
                            dim,
                            group: Some(role_hl("Heading")),
                            ..Default::default()
                        },
                    });
                } else if trimmed.starts_with('>') {
                    line_spans.push(InlineSpan {
                        text: trimmed.to_string(),
                        style: InlineStyle {
                            dim: true,
                            italic: true,
                            ..Default::default()
                        },
                    });
                } else {
                    let (prefix, body) = split_list_prefix(trimmed);
                    if !leading_ws.is_empty() {
                        line_spans.push(InlineSpan {
                            text: leading_ws.to_string(),
                            style: InlineStyle {
                                dim,
                                ..Default::default()
                            },
                        });
                    }
                    if !prefix.is_empty() {
                        line_spans.push(InlineSpan {
                            text: prefix.to_string(),
                            style: InlineStyle {
                                dim: true,
                                ..Default::default()
                            },
                        });
                    }
                    line_spans.extend(parse_inline_spans(body, dim));
                }

                let wrapped = wrap_inline_spans(&line_spans, max_cols);
                if wrapped.len() > 1 {
                    out.mark_wrapped();
                }
                for (si, row_spans) in wrapped.iter().enumerate() {
                    if si == 0 {
                        out.set_source_text(lines[i]);
                    } else {
                        out.mark_soft_wrap_continuation();
                    }
                    if let Some(b) = bctx {
                        b.print_left(out);
                        emit_inline_spans(out, row_spans);
                        b.print_right(out, inline_spans_width(row_spans));
                    } else {
                        out.print(indent);
                        emit_inline_spans(out, row_spans);
                    }
                    out.newline();
                }
                rows += wrapped.len() as u16;
            }
            i += 1;
        }
    }
    rows
}

fn split_list_prefix(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b'.' {
        let end = i + 1;
        if end < bytes.len() && bytes[end] == b' ' {
            return (&line[..end + 1], &line[end + 1..]);
        }
        return (&line[..end], &line[end..]);
    }
    if line.starts_with("- ") || line.starts_with("* ") {
        return (&line[..2], &line[2..]);
    }
    ("", line)
}

fn is_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return true;
    }
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b'.' {
        return true;
    }
    false
}

pub(super) fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let non_space_count = trimmed.chars().filter(|&c| !c.is_whitespace()).count();
    if non_space_count < 3 {
        return false;
    }
    let mut first_char: Option<char> = None;
    for ch in trimmed.chars() {
        if ch == ' ' || ch == '\t' {
            continue;
        }
        if first_char.is_none() {
            first_char = Some(ch);
        } else if first_char != Some(ch) {
            return false;
        }
        if !matches!(ch, '-' | '*' | '_') {
            return false;
        }
    }
    first_char.is_some()
}

fn render_horizontal_rule(
    out: &mut LineBuilder,
    bctx: Option<&smelt_core::content::BoxContext>,
    indent: &str,
) -> u16 {
    let hr = "─".repeat(3);

    if let Some(b) = bctx {
        b.print_left(out);
    } else if !indent.is_empty() {
        out.print(indent);
    }

    out.push_dim();
    out.print_with_meta(
        &hr,
        smelt_core::buffer::SpanMeta {
            selectable: true,
            copy_as: Some("---".into()),
        },
    );
    out.pop_style();

    if let Some(b) = bctx {
        b.print_right(out, 3);
    }

    out.newline();
    1
}

fn render_markdown_table_from_lines(
    out: &mut LineBuilder,
    lines: &[&str],
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    indent: &str,
) -> u16 {
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        if smelt_core::content::is_table_separator(line) {
            continue;
        }
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        let cells: Vec<String> = trimmed.split('|').map(|c| c.trim().to_string()).collect();
        table_rows.push(cells);
    }
    // Source text on row 0 lets copy_range reconstruct the raw markdown; subsequent
    // rows are soft-wrap continuations so they're skipped once row 0's source is emitted.
    out.arm_source_text(lines.join("\n"));
    let n = render_markdown_table(out, &table_rows, dim, bctx, indent);
    out.disarm_source_text();
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::builder::test_util::render_test;

    #[test]
    fn rendered_table_attaches_raw_source_to_first_row() {
        let md = "| col | val |\n| --- | --- |\n| a   | 1   |\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        assert!(block.lines.len() >= 2);
        assert_eq!(
            block.lines[0].source_text.as_deref(),
            Some("| col | val |\n| --- | --- |\n| a   | 1   |")
        );
        for line in block.lines.iter().skip(1) {
            assert!(line.soft_wrapped);
            assert!(line.source_text.is_none());
        }
    }
}
