//! The old and new sources inside a diff, highlighted separately so each side
//! parses like the file it came from.

use std::borrow::Cow;
use std::marker::PhantomData;
use std::ops::Range;

use super::source::{LineSource, ScopedLines, SourceHighlighter, Syntax};

/// Which source a diff row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowSide {
    Old,
    New,
    /// Context: in both sources, rendered from the new one.
    Both,
    /// An elided stretch: both sources restart parsing after it.
    Gap,
}

#[derive(Debug)]
pub(crate) struct DiffSides {
    /// Per side, ascending: the row of each of its source lines.
    rows: [Vec<usize>; 2],
    highlighters: [SourceHighlighter; 2],
}

impl DiffSides {
    /// `rows` lists `(row, side)` in ascending row order; rows that are not
    /// source text are left out.
    pub(crate) fn new(
        [old, new]: [Syntax; 2],
        rows: impl IntoIterator<Item = (usize, RowSide)>,
    ) -> Self {
        let mut sides: [Vec<usize>; 2] = Default::default();
        for (row, side) in rows {
            if side != RowSide::New {
                sides[0].push(row);
            }
            if side != RowSide::Old {
                sides[1].push(row);
            }
        }
        Self {
            rows: sides,
            highlighters: [SourceHighlighter::new(old), SourceHighlighter::new(new)],
        }
    }

    /// Scopes of each row in `rows` from the side it renders on, or `None` if
    /// `cancel` stopped the parse. `text` gives a row's source, or `None` for
    /// a gap.
    pub(crate) fn scopes<'t>(
        &self,
        text: impl Fn(usize) -> Option<Cow<'t, str>>,
        rows: Range<usize>,
        cancel: &mut dyn FnMut() -> bool,
    ) -> Option<ScopedLines> {
        let mut out = ScopedLines {
            lines: vec![Vec::new(); rows.len()],
            stacks: Vec::new(),
        };
        for side in 0..2 {
            let lines = &self.rows[side];
            let first = lines.partition_point(|&row| row < rows.start);
            let last = lines.partition_point(|&row| row < rows.end);
            // Context renders from the new side, so the old side parses only
            // when the range has deleted rows.
            if side == 0
                && lines[first..last]
                    .iter()
                    .all(|row| self.rows[1].binary_search(row).is_ok())
            {
                continue;
            }
            let source = SideSource {
                rows: lines,
                text: &text,
                lines: PhantomData,
            };
            let scoped = self.highlighters[side].scopes(&source, first..last, cancel)?;
            let offset = out.stacks.len();
            out.stacks.extend(scoped.stacks);
            // The new side runs second, so context rows end up with its scopes.
            for (&row, runs) in lines[first..last].iter().zip(scoped.lines) {
                out.lines[row - rows.start] = runs
                    .into_iter()
                    .map(|(end, id)| (end, id + offset))
                    .collect();
            }
        }
        Some(out)
    }
}

struct SideSource<'a, 't, F> {
    rows: &'a [usize],
    text: &'a F,
    lines: PhantomData<Cow<'t, str>>,
}

impl<'t, F: Fn(usize) -> Option<Cow<'t, str>>> LineSource for SideSource<'_, 't, F> {
    fn line_count(&self) -> usize {
        self.rows.len()
    }

    fn line(&self, index: usize) -> Option<Cow<'_, str>> {
        (self.text)(*self.rows.get(index)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sides(rows: &[RowSide]) -> DiffSides {
        DiffSides::new(
            [Syntax::for_ext("rs"), Syntax::for_ext("rs")],
            rows.iter().copied().enumerate(),
        )
    }

    #[test]
    fn each_side_parses_only_its_own_lines() {
        use RowSide::*;
        // The old side opens a comment that the new side never sees.
        let text = ["/* old", "let a = 1;", "*/", "let b = 2;", "let c = 3;"];
        let sides = sides(&[Old, Both, Old, New, Both]);
        let scoped = sides
            .scopes(|row| Some(Cow::Borrowed(text[row])), 0..5, &mut || false)
            .unwrap();
        let plain = SourceHighlighter::new(Syntax::for_ext("rs"))
            .scopes(&["let a = 1;"][..], 0..1, &mut || false)
            .unwrap();
        let stacks = |line: usize| -> Vec<_> {
            scoped.lines[line]
                .iter()
                .map(|&(end, id)| (end, scoped.stacks[id].clone()))
                .collect()
        };
        let expected: Vec<_> = plain.lines[0]
            .iter()
            .map(|&(end, id)| (end, plain.stacks[id].clone()))
            .collect();
        assert_eq!(stacks(1), expected, "context renders from the new side");
        assert!(stacks(2).iter().all(|(_, stack)| stack
            .iter()
            .any(|s| s.build_string().starts_with("comment"))));
    }

    #[test]
    fn gaps_restart_parsing_and_context_never_parses_the_old_side() {
        use RowSide::*;
        let text = ["/* open", "", "let x = 1;"];
        let sides = sides(&[Both, Gap, Both]);
        let scoped = sides
            .scopes(
                |row| (row != 1).then(|| Cow::Borrowed(text[row])),
                0..3,
                &mut || false,
            )
            .unwrap();
        assert!(scoped.lines[1].is_empty());
        assert!(scoped.lines[2].iter().all(|&(_, id)| !scoped.stacks[id]
            .iter()
            .any(|s| s.build_string().starts_with("comment"))));
        assert_eq!(sides.highlighters[0].retained_bytes(), 0);
    }
}
