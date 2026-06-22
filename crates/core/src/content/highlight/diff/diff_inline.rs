use similar::{ChangeTag, InlineChangeMode, InlineChangeOptions, TextDiff};

use super::{DiffByteRange, DiffLine};

fn changed_line_text(line: &DiffLine) -> Option<&str> {
    match line {
        DiffLine::Delete { text, .. } | DiffLine::Insert { text, .. } => Some(text),
        _ => None,
    }
}

fn changed_line_highlights_mut(line: &mut DiffLine) -> Option<&mut Vec<DiffByteRange>> {
    match line {
        DiffLine::Delete { highlights, .. } | DiffLine::Insert { highlights, .. } => {
            Some(highlights)
        }
        _ => None,
    }
}

pub(super) fn full_line_highlight(text: &str) -> Vec<DiffByteRange> {
    (!text.is_empty())
        .then_some(DiffByteRange {
            start: 0,
            end: text.len(),
        })
        .into_iter()
        .collect()
}

fn inline_options() -> InlineChangeOptions {
    let mut options = InlineChangeOptions::new();
    options
        .mode(InlineChangeMode::Chars)
        .min_ratio(0.0)
        .semantic_cleanup(true);
    options
}

pub(super) fn inline_highlights_for_pair(
    old: &str,
    new: &str,
) -> (Vec<DiffByteRange>, Vec<DiffByteRange>) {
    let diff = TextDiff::from_lines(old, new);
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();

    for change in diff.iter_all_inline_changes_with_options(inline_options()) {
        for (emphasized, value) in change.iter_strings_lossy() {
            let len = value.len();
            match change.tag() {
                ChangeTag::Equal => {
                    old_pos += len;
                    new_pos += len;
                }
                ChangeTag::Delete => {
                    let end = old_pos + len;
                    if emphasized {
                        old_ranges.push(DiffByteRange {
                            start: old_pos,
                            end,
                        });
                    }
                    old_pos = end;
                }
                ChangeTag::Insert => {
                    let end = new_pos + len;
                    if emphasized {
                        new_ranges.push(DiffByteRange {
                            start: new_pos,
                            end,
                        });
                    }
                    new_pos = end;
                }
            }
        }
    }

    if old_ranges.is_empty() && new_ranges.is_empty() && old != new {
        old_ranges = full_line_highlight(old);
        new_ranges = full_line_highlight(new);
    }

    (old_ranges, new_ranges)
}

pub(super) fn annotate_inline_highlights(lines: &mut [DiffLine]) {
    let mut i = 0usize;
    while i < lines.len() {
        if !matches!(lines[i], DiffLine::Delete { .. }) {
            i += 1;
            continue;
        }

        let del_start = i;
        while i < lines.len() && matches!(lines[i], DiffLine::Delete { .. }) {
            i += 1;
        }
        let ins_start = i;
        while i < lines.len() && matches!(lines[i], DiffLine::Insert { .. }) {
            i += 1;
        }
        let del_end = ins_start;
        let ins_end = i;
        let pairs = (del_end - del_start).min(ins_end - ins_start);

        for offset in 0..pairs {
            let old = changed_line_text(&lines[del_start + offset])
                .unwrap_or_default()
                .to_string();
            let new = changed_line_text(&lines[ins_start + offset])
                .unwrap_or_default()
                .to_string();
            let (old_ranges, new_ranges) = inline_highlights_for_pair(&old, &new);
            if let Some(highlights) = changed_line_highlights_mut(&mut lines[del_start + offset]) {
                *highlights = old_ranges;
            }
            if let Some(highlights) = changed_line_highlights_mut(&mut lines[ins_start + offset]) {
                *highlights = new_ranges;
            }
        }

        for line in lines.iter_mut().take(del_end).skip(del_start + pairs) {
            if let Some(text) = changed_line_text(line).map(str::to_string) {
                if let Some(highlights) = changed_line_highlights_mut(line) {
                    *highlights = full_line_highlight(&text);
                }
            }
        }
        for line in lines.iter_mut().take(ins_end).skip(ins_start + pairs) {
            if let Some(text) = changed_line_text(line).map(str::to_string) {
                if let Some(highlights) = changed_line_highlights_mut(line) {
                    *highlights = full_line_highlight(&text);
                }
            }
        }
    }
}
