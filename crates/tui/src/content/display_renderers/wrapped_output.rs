use super::metrics::{block_inner_width, pluralize, BLOCK_GUTTER_SPACE};
use super::MAX_TOOL_BLOCK_ROWS;
use smelt_core::content::ansi::{emit_ansi_row, wrap_ansi};
use smelt_core::content::builder::LineBuilder;
use smelt_core::theme::intern;

fn print_dim(out: &mut LineBuilder, text: &str) {
    out.push_dim();
    out.print(text);
    out.pop_style();
}

pub(super) fn measure_wrapped_output(content: &str, width: usize) -> u16 {
    let max_cols = block_inner_width(width);
    let total_rows: usize = content
        .lines()
        .map(|line| {
            let expanded = line.replace('\t', "    ");
            let (_, ranges, _) = wrap_ansi(&expanded, max_cols);
            ranges.len()
        })
        .sum();
    (total_rows.min(MAX_TOOL_BLOCK_ROWS) + usize::from(total_rows > MAX_TOOL_BLOCK_ROWS)) as u16
}

pub(super) fn render_wrapped_output(
    out: &mut LineBuilder,
    content: &str,
    is_error: bool,
    width: usize,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:wrapped_output");
    let max_cols = block_inner_width(width);

    let mut all_lines = Vec::new();
    let mut total_rows = 0usize;

    for line in content.lines() {
        let expanded = line.replace('\t', "    ");
        let (spans, ranges, boundaries) = wrap_ansi(&expanded, max_cols);
        if ranges.len() > 1 {
            out.mark_wrapped();
        }
        total_rows += ranges.len();
        all_lines.push((spans, ranges, boundaries));
    }

    let mut rows = 0u16;
    if total_rows > MAX_TOOL_BLOCK_ROWS {
        let skipped = total_rows - MAX_TOOL_BLOCK_ROWS;
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

    let mut skip = total_rows.saturating_sub(MAX_TOOL_BLOCK_ROWS);
    for (spans, ranges, boundaries) in &all_lines {
        let count = ranges.len();
        if skip >= count {
            skip -= count;
            continue;
        }
        let start = skip;
        skip = 0;
        for &(ws, we) in &ranges[start..] {
            if is_error {
                out.push_hl(intern("ErrorMsg"));
            } else {
                out.push_dim();
            }
            out.print(BLOCK_GUTTER_SPACE);
            emit_ansi_row(out, spans, boundaries, ws, we);
            out.pop_style();
            out.newline();
            rows += 1;
        }
    }
    rows
}
