//! Run-aware text wrapping shared by transcript renderers.
//!
//! `InlineLine` is width-independent: it stores text runs plus per-run metadata
//! and can measure or wrap them for any column budget. Rendering code decides
//! what the metadata means.

use std::ops::Range;

use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BreakPolicy {
    /// Prefer spaces as break points; spaces remain part of wrapped rows.
    #[default]
    Normal,
    /// Prefer spaces as break points; a space that causes a row break is omitted.
    BreakOnSpaces,
    /// Keep the run together. It may exceed the width when it starts a row.
    Unbreakable,
    /// Preserve every character and break strictly by display width.
    PreserveSpaces,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineRun<T> {
    pub text: String,
    pub meta: T,
    pub break_policy: BreakPolicy,
}

impl<T> InlineRun<T> {
    pub fn new(text: impl Into<String>, meta: T, break_policy: BreakPolicy) -> Self {
        Self {
            text: text.into(),
            meta,
            break_policy,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedRun<T> {
    pub run_index: usize,
    pub range: Range<usize>,
    pub meta: T,
    pub break_policy: BreakPolicy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineLine<T> {
    pub runs: Vec<InlineRun<T>>,
}

impl<T> InlineLine<T> {
    pub fn new(runs: Vec<InlineRun<T>>) -> Self {
        Self { runs }
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty() || self.runs.iter().all(|run| run.text.is_empty())
    }

    pub fn measure_unwrapped(&self) -> usize {
        self.runs
            .iter()
            .map(|run| UnicodeWidthStr::width(run.text.as_str()))
            .sum()
    }

    /// Wrap the concatenated plain text into byte ranges using the transcript
    /// output's word-boundary behavior. If a separating space does not fit at
    /// the end of a row, it is treated as the break point rather than emitted
    /// as the first cell of the continuation row.
    pub fn wrap_plain_ranges(&self, max_cells: usize) -> Vec<(usize, usize)> {
        if let [run] = self.runs.as_slice() {
            return crate::wrap::wrap_line_ranges(&run.text, max_cells);
        }

        let mut plain = String::new();
        for run in &self.runs {
            plain.push_str(&run.text);
        }
        crate::wrap::wrap_line_ranges(&plain, max_cells)
    }
}

impl<T: Clone> InlineLine<T> {
    pub fn plain(text: impl Into<String>, meta: T) -> Self {
        Self {
            runs: vec![InlineRun::new(text, meta, BreakPolicy::BreakOnSpaces)],
        }
    }

    pub fn fragment_text<'a>(&'a self, fragment: &WrappedRun<T>) -> &'a str {
        &self.runs[fragment.run_index].text[fragment.range.clone()]
    }

    pub fn wrap_fragments(&self, max_cells: usize) -> Vec<Vec<WrappedRun<T>>> {
        self.wrap_fragments_with_widths(max_cells, max_cells)
    }

    pub fn wrap_fragments_with_widths(
        &self,
        first_cells: usize,
        continuation_cells: usize,
    ) -> Vec<Vec<WrappedRun<T>>> {
        if first_cells == 0 || continuation_cells == 0 {
            let row: Vec<WrappedRun<T>> = self
                .runs
                .iter()
                .enumerate()
                .filter(|(_, run)| !run.text.is_empty())
                .map(|(run_index, run)| WrappedRun {
                    run_index,
                    range: 0..run.text.len(),
                    meta: run.meta.clone(),
                    break_policy: run.break_policy,
                })
                .collect();
            return vec![row];
        }

        let mut state = WrapState::new(first_cells, continuation_cells);
        for (run_index, run) in self.runs.iter().enumerate() {
            match run.break_policy {
                BreakPolicy::Normal => append_normal_fragments(run_index, run, false, &mut state),
                BreakPolicy::BreakOnSpaces => {
                    append_normal_fragments(run_index, run, true, &mut state)
                }
                BreakPolicy::Unbreakable => append_unbreakable_fragment(run_index, run, &mut state),
                BreakPolicy::PreserveSpaces => {
                    append_preserve_space_fragments(run_index, run, &mut state)
                }
            }
        }

        state.finish()
    }
}

impl<T: Clone + PartialEq> InlineLine<T> {
    pub fn wrap_rows(&self, max_cells: usize) -> usize {
        self.wrap_ranges(max_cells).len()
    }

    pub fn wrap_ranges(&self, max_cells: usize) -> Vec<Vec<InlineRun<T>>> {
        if max_cells == 0 {
            return vec![self.runs.clone()];
        }

        self.wrap_fragments(max_cells)
            .into_iter()
            .map(|row| {
                let mut out = Vec::new();
                for fragment in row {
                    append_text(
                        &mut out,
                        self.fragment_text(&fragment),
                        &InlineRun::new("", fragment.meta, fragment.break_policy),
                    );
                }
                out
            })
            .collect()
    }
}

struct WrapState<T> {
    rows: Vec<Vec<WrappedRun<T>>>,
    cur: Vec<WrappedRun<T>>,
    col: usize,
    first_cells: usize,
    continuation_cells: usize,
}

impl<T: Clone> WrapState<T> {
    fn new(first_cells: usize, continuation_cells: usize) -> Self {
        Self {
            rows: Vec::new(),
            cur: Vec::new(),
            col: 0,
            first_cells,
            continuation_cells,
        }
    }

    fn max_cells(&self) -> usize {
        if self.rows.is_empty() {
            self.first_cells
        } else {
            self.continuation_cells
        }
    }

    fn push_row(&mut self) {
        self.rows.push(std::mem::take(&mut self.cur));
        self.col = 0;
    }

    fn append_fragment(&mut self, run_index: usize, range: Range<usize>, run: &InlineRun<T>) {
        if range.is_empty() {
            return;
        }
        if let Some(last) = self.cur.last_mut() {
            if last.run_index == run_index && last.range.end == range.start {
                last.range.end = range.end;
                return;
            }
        }
        self.cur.push(WrappedRun {
            run_index,
            range,
            meta: run.meta.clone(),
            break_policy: run.break_policy,
        });
    }

    fn finish(mut self) -> Vec<Vec<WrappedRun<T>>> {
        if !self.cur.is_empty() || self.rows.is_empty() {
            self.rows.push(self.cur);
        }
        self.rows
    }
}

fn append_normal_fragments<T: Clone>(
    run_index: usize,
    run: &InlineRun<T>,
    break_on_spaces: bool,
    state: &mut WrapState<T>,
) {
    let mut offset = 0usize;
    let text = run.text.as_str();
    while offset < text.len() {
        let relative = &text[offset..];
        let word_rel_end = relative.find(' ').unwrap_or(relative.len());
        let word_start = offset;
        let word_end = offset + word_rel_end;
        let has_space = word_end < text.len();
        let space_end = word_end + usize::from(has_space);
        let segment_width = text_width(&text[word_start..space_end]);
        let word_width = text_width(&text[word_start..word_end]);

        let word_overflows_current_row =
            state.col + segment_width > state.max_cells() && state.col > 0;
        if word_overflows_current_row && word_width <= state.max_cells() {
            state.push_row();
        }

        if word_width > state.max_cells() {
            append_char_fragments(run_index, run, word_start..word_end, state);
        } else {
            state.append_fragment(run_index, word_start..word_end, run);
            state.col += word_width;
        }

        if has_space {
            if state.col + 1 > state.max_cells() && state.col > 0 && break_on_spaces {
                state.push_row();
            } else {
                if state.col + 1 > state.max_cells() && state.col > 0 {
                    state.push_row();
                }
                state.append_fragment(run_index, word_end..space_end, run);
                state.col += 1;
            }
        }
        offset = space_end;
    }
}

fn append_unbreakable_fragment<T: Clone>(
    run_index: usize,
    run: &InlineRun<T>,
    state: &mut WrapState<T>,
) {
    let width = text_width(&run.text);
    if state.col + width > state.max_cells() && state.col > 0 {
        state.push_row();
    }
    state.append_fragment(run_index, 0..run.text.len(), run);
    state.col += width;
}

fn append_preserve_space_fragments<T: Clone>(
    run_index: usize,
    run: &InlineRun<T>,
    state: &mut WrapState<T>,
) {
    append_char_fragments(run_index, run, 0..run.text.len(), state);
}

fn append_char_fragments<T: Clone>(
    run_index: usize,
    run: &InlineRun<T>,
    range: Range<usize>,
    state: &mut WrapState<T>,
) {
    let mut idx = range.start;
    for ch in run.text[range].chars() {
        let next = idx + ch.len_utf8();
        let cw = char_width(ch);
        if state.col + cw > state.max_cells() && state.col > 0 {
            state.push_row();
        }
        state.append_fragment(run_index, idx..next, run);
        state.col += cw;
        idx = next;
    }
}

fn append_text<T: Clone + PartialEq>(row: &mut Vec<InlineRun<T>>, text: &str, run: &InlineRun<T>) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = row.last_mut() {
        if last.meta == run.meta && last.break_policy == run.break_policy {
            last.text.push_str(text);
            return;
        }
    }
    row.push(InlineRun::new(text, run.meta.clone(), run.break_policy));
}

fn text_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn char_width(ch: char) -> usize {
    let mut buf = [0; 4];
    UnicodeWidthStr::width(ch.encode_utf8(&mut buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: Vec<Vec<InlineRun<()>>>) -> Vec<Vec<String>> {
        rows.into_iter()
            .map(|row| row.into_iter().map(|run| run.text).collect())
            .collect()
    }

    #[test]
    fn normal_wraps_on_word_boundaries() {
        let line = InlineLine::plain("hello world", ());
        assert_eq!(
            texts(line.wrap_ranges(7)),
            vec![vec![String::from("hello ")], vec![String::from("world")]]
        );
    }

    #[test]
    fn oversized_control_word_counts_controls_as_cells() {
        let line = InlineLine::plain("\0\0\0\0x", ());
        assert_eq!(
            texts(line.wrap_ranges(3)),
            vec![vec![String::from("\0\0\0")], vec![String::from("\0x")]]
        );
    }

    #[test]
    fn normal_breaks_oversized_words_by_character() {
        let line = InlineLine::plain("abcdef", ());
        assert_eq!(
            texts(line.wrap_ranges(3)),
            vec![vec![String::from("abc")], vec![String::from("def")]]
        );
    }

    #[test]
    fn oversized_word_uses_remaining_row_width() {
        let line = InlineLine::plain("tool {\"files\":[\"long/path.rs\"]}", ());
        assert_eq!(
            texts(line.wrap_ranges(14)),
            vec![
                vec![String::from("tool {\"files\":")],
                vec![String::from("[\"long/path.rs")],
                vec![String::from("\"]}")]
            ]
        );
    }

    #[test]
    fn break_on_spaces_keeps_indent_with_oversized_word() {
        let line = InlineLine::plain("  abcdef", ());
        assert_eq!(
            texts(line.wrap_ranges(4)),
            vec![vec![String::from("  ab")], vec![String::from("cdef")]]
        );
    }

    #[test]
    fn preserve_spaces_breaks_by_cells() {
        let line = InlineLine::new(vec![InlineRun::new(
            "ab cd",
            (),
            BreakPolicy::PreserveSpaces,
        )]);
        assert_eq!(
            texts(line.wrap_ranges(3)),
            vec![vec![String::from("ab ")], vec![String::from("cd")]]
        );
    }

    #[test]
    fn normal_preserves_overflow_space() {
        let line = InlineLine::new(vec![InlineRun::new("abc def", (), BreakPolicy::Normal)]);
        assert_eq!(
            texts(line.wrap_ranges(3)),
            vec![
                vec![String::from("abc")],
                vec![String::from(" ")],
                vec![String::from("def")]
            ]
        );
    }

    #[test]
    fn break_on_spaces_uses_overflow_space_as_break_point() {
        let line = InlineLine::plain("abc def", ());
        assert_eq!(
            texts(line.wrap_ranges(3)),
            vec![vec![String::from("abc")], vec![String::from("def")]]
        );
    }

    #[test]
    fn fragments_preserve_run_indices_and_ranges() {
        let line = InlineLine::new(vec![
            InlineRun::new("abc ", (), BreakPolicy::BreakOnSpaces),
            InlineRun::new("def", (), BreakPolicy::BreakOnSpaces),
        ]);
        let rows = line.wrap_fragments(3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].run_index, 0);
        assert_eq!(rows[0][0].range, 0..3);
        assert_eq!(rows[1][0].run_index, 1);
        assert_eq!(rows[1][0].range, 0..3);
    }

    #[test]
    fn plain_ranges_use_overflow_space_as_break_point() {
        let line = InlineLine::plain("abc def", ());
        let ranges = line.wrap_plain_ranges(3);
        let chunks: Vec<&str> = ranges.iter().map(|(s, e)| &"abc def"[*s..*e]).collect();
        assert_eq!(chunks, vec!["abc", "def"]);
    }

    #[test]
    fn fragments_support_narrower_continuation_rows() {
        let line = InlineLine::new(vec![
            InlineRun::new("abc ", 1, BreakPolicy::BreakOnSpaces),
            InlineRun::new("def ghi", 2, BreakPolicy::BreakOnSpaces),
        ]);
        let rows = line.wrap_fragments_with_widths(8, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(line.fragment_text(&rows[0][0]), "abc ");
        assert_eq!(line.fragment_text(&rows[0][1]), "def ");
        assert_eq!(line.fragment_text(&rows[1][0]), "ghi");
        assert_eq!(rows[0][0].meta, 1);
        assert_eq!(rows[0][1].meta, 2);
        assert_eq!(rows[1][0].meta, 2);
    }

    #[test]
    fn preserves_run_metadata_across_wraps() {
        let line = InlineLine::new(vec![
            InlineRun::new("hello ", 1, BreakPolicy::Normal),
            InlineRun::new("world", 2, BreakPolicy::Normal),
        ]);
        let rows = line.wrap_ranges(7);
        assert_eq!(rows[0][0].meta, 1);
        assert_eq!(rows[1][0].meta, 2);
    }
}
