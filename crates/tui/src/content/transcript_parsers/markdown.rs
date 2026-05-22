use smelt_core::content::builder::LineBuilder;
use smelt_core::content::highlight::{render_code_block, render_markdown_table};
use smelt_core::content::{markdown_closes_fence, markdown_opening_fence};
use smelt_core::theme::intern;

pub fn render_markdown_inner(
    out: &mut LineBuilder,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:markdown");
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(indent.len() + 1)
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    let mut rows = 0u16;
    let mut last_content_line: Option<&str> = None;
    let mut pending_blank = false;
    while i < lines.len() {
        if lines[i].trim().is_empty() {
            let after_heading = last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
            let mut next_i = i + 1;
            while next_i < lines.len() && lines[next_i].trim().is_empty() {
                next_i += 1;
            }
            if rows > 0 && !after_heading && next_i < lines.len() && !is_list_item(lines[next_i]) {
                pending_blank = true;
            }
            i = next_i;
            continue;
        }
        if pending_blank {
            out.newline();
            rows += 1;
            pending_blank = false;
        }
        if let Some((fence_len, lang)) = markdown_opening_fence(lines[i]) {
            // Don't add an extra blank line before the fence — paragraph spacing
            // is already handled by pending_blank, and inter-block gaps handle
            // spacing between separate blocks.
            i += 1;
            let code_start = i;
            while i < lines.len() {
                if markdown_closes_fence(fence_len, lines[i]) {
                    break;
                }
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
            rows += render_markdown_table_from_lines(
                out,
                &lines[table_start..i],
                width,
                dim,
                bctx,
                indent,
            );
            last_content_line = None;
        } else if is_horizontal_rule(lines[i]) {
            rows += render_horizontal_rule(out, bctx, indent);
            last_content_line = None;
            i += 1;
        } else {
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
                            group: Some(intern("SmeltHeading")),
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
            last_content_line = Some(lines[i]);
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
    width: usize,
    dim: bool,
    bctx: Option<&smelt_core::content::BoxContext>,
    indent: &str,
) -> u16 {
    let mut alignments = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        if smelt_core::content::is_table_separator(line) {
            if alignments.is_empty() {
                alignments = smelt_core::content::parse_table_alignments(line);
            }
            continue;
        }
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        let cells: Vec<String> = trimmed.split('|').map(|c| c.trim().to_string()).collect();
        table_rows.push(cells);
    }
    // Source text on row 0 lets copy_range reconstruct the raw markdown; subsequent
    // rows are soft-wrap continuations so they're skipped once row 0's source is emitted.
    out.arm_source_text(lines.join("\n"));
    let n = render_markdown_table(out, &table_rows, &alignments, width, dim, bctx, indent);
    out.disarm_source_text();
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::builder::test_util::render_test;

    #[test]
    fn markdown_collapses_leading_blank_run_before_code_block() {
        let md = "\n\nValidation run in the new worktree:\n\n```bash\ncargo test\n```\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert_eq!(rows[0], "Validation run in the new worktree:");
        assert_eq!(rows.iter().filter(|row| row.is_empty()).count(), 1);
        assert!(rows.iter().any(|row| row.contains("cargo test")));
        assert!(!rows.iter().any(|row| row.contains("``")), "rows: {rows:?}");
    }

    #[test]
    fn markdown_code_block_can_contain_shorter_fenced_block() {
        let md = "````markdown\n```rust\nfn main() {}\n```\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert!(
            rows.iter().any(|row| row.contains("```rust")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("fn main()")),
            "rows: {rows:?}"
        );
        assert_eq!(rows.iter().filter(|row| row.contains("````")).count(), 0);
    }

    #[test]
    fn markdown_code_block_ignores_longer_opening_fence_line() {
        let md = "````markdown\n`````text\ninside\n`````\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<&str> = block.lines.iter().map(|l| l.text.as_str()).collect();

        assert!(
            rows.iter().any(|row| row.contains("`````text")),
            "rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("inside")),
            "rows: {rows:?}"
        );
    }

    #[test]
    fn markdown_code_block_keeps_fence_with_trailing_text_as_content() {
        let md = "````\n```` text\ninside\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(rows, vec!["```` text", "inside"]);
    }

    #[test]
    fn markdown_code_block_closes_on_longer_plain_fence() {
        let md = "````\ninside\n`````\nafter";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(rows, vec!["inside", "after"]);
    }

    #[test]
    fn markdown_adjacent_nested_code_blocks_preserve_inner_fences() {
        let md = "````\n```\n```\n````\n````\n```\nnested code block\n```\n````";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let rows: Vec<String> = block
            .lines
            .iter()
            .map(|l| l.text.trim_end().to_string())
            .collect();

        assert_eq!(rows, vec!["```", "```", "```", "nested code block", "```"]);
    }

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

    #[test]
    fn rendered_table_honors_separator_alignment_markers() {
        // Generous header widths so per-column padding is visible.
        let md = "| LLLL | CCCC | RRRR |\n|:-----|:----:|-----:|\n| x | y | z |\n";
        let block = render_test(80, |sink| {
            render_markdown_inner(sink, md, 80, "", false, None);
        });
        let data_row = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .find(|s| s.contains('x') && s.contains('y') && s.contains('z'))
            .expect("data row");
        assert!(data_row.contains("┃ x    ┃"), "left: {data_row:?}");
        assert!(data_row.contains("┃  y   ┃"), "center: {data_row:?}");
        assert!(data_row.contains("┃    z ┃"), "right: {data_row:?}");
    }
}
