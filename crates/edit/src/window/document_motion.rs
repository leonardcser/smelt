//! Cursor motions over row-backed documents: the `Motion` vocabulary resolved
//! against display rows, as `motions::resolve` does against plain text.

use super::document_view::{
    document_byte_col_at_cell, document_position_cell, document_row_string, BlockMotion,
};
use super::*;
use crate::motions::{
    bracket_search_forward, find_counted, find_reach, resolve, LineTo, Motion, Reach, Use, WordTo,
};
use crate::row::{BlockBoundary, DisplayDocument, DisplayRow};
use std::ops::Range;

/// What a row motion needs to know besides the motion itself.
#[derive(Clone, Copy)]
pub(super) struct RowEnv {
    pub total_rows: RowIndex,
    pub vim_mode: VimMode,
    pub usage: Use,
    /// First visible row and number of visible rows, for `H` `M` `L`.
    pub scroll_top: RowIndex,
    pub viewport_rows: u16,
}

/// Rows a scan held to a block (`%`, a text object) loads on each side of the
/// cursor at most, which bounds how long one that finds nothing in a huge
/// block takes.
pub(super) const SCAN_LIMIT_ROWS: RowIndex = 1024;

/// How many rows a block may open with before its text, such as the padding
/// around a user message.
const BLOCK_LEADING_BLANK_ROWS: RowIndex = 4;

/// Where `motion` takes the cursor from `from`, and how far it reaches; `None`
/// when it cannot move.
pub(super) fn resolve_rows<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    from: DocPosition,
    motion: Motion,
    count: Option<usize>,
    env: RowEnv,
) -> Option<(DocPosition, Reach)> {
    let last = env.total_rows.checked_sub(1)?;
    let n = count.unwrap_or(1).max(1);
    let from = DocPosition {
        row: from.row.min(last),
        ..from
    };
    let at = |row: RowIndex, byte_col: usize| DocPosition { row, byte_col };
    let row_text = |doc: &mut D, row: RowIndex| document_row_string(doc, row);
    // The row's column that sits under the cursor's cell.
    let same_cell = |doc: &mut D, row: RowIndex| {
        let cell = document_position_cell(doc, from).unwrap_or(0);
        document_byte_col_at_cell(doc, row, cell).unwrap_or(0)
    };
    Some(match motion {
        Motion::Left | Motion::Right => {
            let line = row_text(doc, from.row)?;
            let mut col = text::snap_grapheme(&line, from.byte_col.min(line.len()));
            for _ in 0..n {
                col = if motion == Motion::Left {
                    text::prev_grapheme_boundary(&line, col)
                } else {
                    text::next_grapheme_boundary(&line, col)
                };
            }
            let selecting = matches!(env.vim_mode, VimMode::Visual | VimMode::VisualLine);
            if motion == Motion::Right
                && env.usage == Use::Move
                && !selecting
                && col > 0
                && col >= line.len()
            {
                col = text::prev_grapheme_boundary(&line, line.len());
            }
            (at(from.row, col), Reach::Exclusive)
        }
        Motion::Down | Motion::Up => {
            let delta = if motion == Motion::Down {
                n as isize
            } else {
                -(n as isize)
            };
            let row = add_signed_row(from.row, delta).min(last);
            (at(row, same_cell(doc, row)), Reach::Linewise)
        }
        Motion::Word(..) | Motion::Sentence { .. } | Motion::Paragraph { .. } => {
            scan_rows(doc, from, motion, count, env)?
        }
        Motion::LineStart => (at(from.row, 0), Reach::Exclusive),
        Motion::FirstNonBlank => {
            let line = row_text(doc, from.row)?;
            (at(from.row, first_non_blank(&line)), Reach::Exclusive)
        }
        Motion::Line(to) => {
            let delta = match to {
                LineTo::Next => n as isize,
                LineTo::Prev => -(n as isize),
                LineTo::Current => n as isize - 1,
            };
            let row = add_signed_row(from.row, delta).min(last);
            let line = row_text(doc, row)?;
            (at(row, first_non_blank(&line)), Reach::Linewise)
        }
        Motion::LastNonBlank => {
            let row = from.row.saturating_add(n as RowIndex - 1).min(last);
            let line = row_text(doc, row)?;
            let end = line.trim_end_matches([' ', '\t']).len();
            (
                at(row, text::prev_grapheme_boundary(&line, end)),
                Reach::Inclusive,
            )
        }
        Motion::Column => {
            let line = row_text(doc, from.row)?;
            (
                at(from.row, text::cell_to_byte(&line, n - 1)),
                Reach::Exclusive,
            )
        }
        Motion::LineEnd => {
            let row = from.row.saturating_add(n as RowIndex - 1).min(last);
            let len = row_text(doc, row)?.len();
            (at(row, len), Reach::Exclusive)
        }
        Motion::BufferStart => match count {
            Some(c) => {
                let row = (c as RowIndex).saturating_sub(1).min(last);
                (at(row, same_cell(doc, row)), Reach::Linewise)
            }
            None => (at(0, same_cell(doc, 0)), Reach::Linewise),
        },
        Motion::BufferEnd => {
            let row = count.map_or(last, |c| (c as RowIndex).saturating_sub(1).min(last));
            (at(row, same_cell(doc, row)), Reach::Linewise)
        }
        Motion::MatchBracket => match count {
            Some(percent) if percent <= 100 => {
                let row = (percent as RowIndex * env.total_rows).div_ceil(100).max(1) - 1;
                (
                    at(row.min(last), same_cell(doc, row.min(last))),
                    Reach::Linewise,
                )
            }
            Some(_) => return None,
            None => scan_rows(doc, from, motion, None, env)?,
        },
        Motion::Find { kind, ch, repeat } => {
            let line = row_text(doc, from.row)?;
            let col = text::snap_grapheme(&line, from.byte_col.min(line.len()));
            let col = find_counted(&line, col, kind, ch, repeat, n)?;
            (at(from.row, col), find_reach(kind))
        }
        Motion::Block(block) => (
            block_position(doc, env.total_rows, from, block, n)?,
            Reach::Exclusive,
        ),
        Motion::ByteOffset => return None,
        Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => {
            let visible = env
                .total_rows
                .saturating_sub(env.scroll_top)
                .min(env.viewport_rows as RowIndex)
                .max(1);
            let offset = ScreenMotion::of(motion, n)?.visible_row(row_to_usize(visible))?;
            let row = env.scroll_top.saturating_add(offset as RowIndex).min(last);
            // The column stays, as nvim's 'nostartofline' keeps it.
            (at(row, same_cell(doc, row)), Reach::Linewise)
        }
    })
}

/// Pull a cursor that rests past a row's last character back onto it, as normal
/// mode requires.
pub(super) fn clamp_to_row_text<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    mut pos: DocPosition,
    vim_mode: VimMode,
) -> DocPosition {
    if vim_mode != VimMode::Normal {
        return pos;
    }
    if let Some(line) = document_row_string(doc, pos.row) {
        if pos.byte_col > 0 && pos.byte_col >= line.len() {
            pos.byte_col = text::prev_grapheme_boundary(&line, line.len());
        }
    }
    pos
}

fn first_non_blank(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// Rows a scan starts with on each side of the cursor; it widens while it
/// runs into an edge of what was loaded.
const SCAN_ROWS: RowIndex = 16;

/// Consecutive rows joined into one text, a row per line, so the scans that walk
/// the prompt walk them too.
pub(super) struct RowWindow {
    pub text: String,
    starts: Vec<usize>,
    first: RowIndex,
}

impl RowWindow {
    /// Load `rows`, or `None` when the document returns another number of
    /// them. With `block` set, keep only the rows of the document blocks that
    /// hold those rows, and say which sides end at a block's edge.
    fn load<D: DisplayDocument + ?Sized>(
        doc: &mut D,
        rows: Range<RowIndex>,
        block: Option<Range<RowIndex>>,
    ) -> Option<(Self, Edges)> {
        let loaded = doc.materialize(rows.clone()).rows;
        if loaded.len() as RowIndex != rows.end - rows.start {
            return None;
        }
        let (mut keep, mut ends) = (0..loaded.len(), Edges::default());
        if let Some(block) = block.filter(|_| !loaded.is_empty()) {
            let first = row_to_usize(block.start.saturating_sub(rows.start)).min(loaded.len() - 1);
            let last = row_to_usize(block.end.saturating_sub(rows.start)).max(first + 1);
            if let Some(start) = (0..=first).rev().find(|&i| loaded[i].block_start) {
                keep.start = start;
                ends.before = true;
            }
            if let Some(end) = (last..loaded.len()).find(|&i| loaded[i].block_start) {
                keep.end = end;
                ends.after = true;
            }
        }
        let mut text = String::new();
        let mut starts = Vec::new();
        for row in &loaded[keep.clone()] {
            if !starts.is_empty() {
                text.push('\n');
            }
            starts.push(text.len());
            text.push_str(&row.text);
        }
        let first = rows.start + keep.start as RowIndex;
        Some((
            Self {
                text,
                starts,
                first,
            },
            ends,
        ))
    }

    /// The byte of `pos`, held inside its row; `None` outside the window.
    pub fn offset(&self, pos: DocPosition) -> Option<usize> {
        let line = row_to_usize(pos.row.checked_sub(self.first)?);
        let start = *self.starts.get(line)?;
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.text.len(), |next| next - 1);
        Some(start + pos.byte_col.min(end - start))
    }

    pub fn position(&self, offset: usize) -> DocPosition {
        let line = self.line(offset);
        DocPosition {
            row: self.first + line as RowIndex,
            byte_col: offset - self.starts[line],
        }
    }

    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset).max(1) - 1
    }

    /// Which edges of the window the bytes `range` touch.
    pub fn edges(&self, range: Range<usize>) -> Edges {
        Edges {
            before: self.line(range.start) == 0,
            after: self.line(range.end) + 1 == self.starts.len(),
        }
    }
}

/// The edges of a window a scan ran into, where more rows may change its result.
#[derive(Clone, Copy, Default)]
pub(super) struct Edges {
    pub before: bool,
    pub after: bool,
}

impl Edges {
    pub const BOTH: Self = Self {
        before: true,
        after: true,
    };
}

/// How far a scan over rows may reach.
#[derive(Clone, Copy)]
pub(super) enum ScanBound {
    /// Across blocks, to the document's ends.
    Document,
    /// Inside the blocks that hold the starting rows, and at most
    /// `SCAN_LIMIT_ROWS` on either side.
    Block,
}

/// Run `attempt` over a window of the rows `around` and some on either side,
/// loading more rows on each side whose edge it ran into, until the document
/// or, for `ScanBound::Block`, the block ends.
pub(super) fn over_rows<D: DisplayDocument + ?Sized, T>(
    doc: &mut D,
    around: Range<RowIndex>,
    total_rows: RowIndex,
    bound: ScanBound,
    mut attempt: impl FnMut(&RowWindow) -> (Option<T>, Edges),
) -> Option<T> {
    let (block, limit) = match bound {
        ScanBound::Document => (None, RowIndex::MAX),
        ScanBound::Block => (Some(around.clone()), SCAN_LIMIT_ROWS),
    };
    let (mut before, mut after) = (SCAN_ROWS, SCAN_ROWS);
    loop {
        let first = around.start.saturating_sub(before);
        let end = around.end.saturating_add(after).min(total_rows);
        let Some((window, ends)) = RowWindow::load(doc, first..end, block.clone()) else {
            // Rows the document cannot load around the start, such as records
            // that fail to hydrate, leave the scan to the starting rows.
            let (window, _) = RowWindow::load(doc, around.clone(), block)?;
            return attempt(&window).0;
        };
        let (result, edges) = attempt(&window);
        let more_before = edges.before && !ends.before && first > 0 && before < limit;
        let more_after = edges.after && !ends.after && end < total_rows && after < limit;
        if !more_before && !more_after {
            return result;
        }
        if more_before {
            before = before.saturating_mul(2);
        }
        if more_after {
            after = after.saturating_mul(2);
        }
    }
}

/// The first row from `from` on, towards the document's end or, unless
/// `forward`, its start, that `found` holds for, looking at `limit` rows at
/// most. When none does, the last row it looked at, or `from`.
pub(super) fn find_row<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    from: RowIndex,
    forward: bool,
    limit: RowIndex,
    mut found: impl FnMut(&DisplayRow) -> bool,
) -> Result<RowIndex, RowIndex> {
    const CHUNK: RowIndex = 64;
    let total_rows = doc.snapshot().total_rows;
    let (mut next, mut seen, mut left) = (Some(from).filter(|&row| row < total_rows), from, limit);
    while let Some(at) = next.filter(|_| left > 0) {
        let len = CHUNK.min(left);
        let chunk = if forward {
            at..at.saturating_add(len).min(total_rows)
        } else {
            at.saturating_sub(len - 1)..at + 1
        };
        let rows = doc.materialize(chunk.clone()).rows;
        if rows.len() as RowIndex != chunk.end - chunk.start {
            break;
        }
        let mut rows = rows.iter().enumerate();
        let hit = if forward {
            rows.find(|(_, row)| found(row))
        } else {
            rows.rev().find(|(_, row)| found(row))
        };
        if let Some((offset, _)) = hit {
            return Ok(chunk.start + offset as RowIndex);
        }
        left -= chunk.end - chunk.start;
        (seen, next) = if forward {
            (
                chunk.end - 1,
                Some(chunk.end).filter(|&row| row < total_rows),
            )
        } else {
            (chunk.start, chunk.start.checked_sub(1))
        };
    }
    Err(seen)
}

/// Run a word, sentence, paragraph or bracket motion over the rows around
/// `from`, as `motions::resolve` does over plain text.
fn scan_rows<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    from: DocPosition,
    motion: Motion,
    count: Option<usize>,
    env: RowEnv,
) -> Option<(DocPosition, Reach)> {
    // Where a motion that found nothing would have gone on looking.
    let failed = |text: &str, start: usize| {
        let forward = match motion {
            Motion::Word(WordTo::Next | WordTo::End, _)
            | Motion::Paragraph { forward: true }
            | Motion::Sentence { forward: true } => true,
            Motion::MatchBracket => match bracket_search_forward(text, start) {
                Some(forward) => forward,
                None => return Edges::default(),
            },
            _ => false,
        };
        Edges {
            before: !forward,
            after: forward,
        }
    };
    // Brackets pair inside a block; words, sentences and paragraphs go on
    // into the next one.
    let bound = if motion == Motion::MatchBracket {
        ScanBound::Block
    } else {
        ScanBound::Document
    };
    over_rows(
        doc,
        from.row..from.row.saturating_add(1),
        env.total_rows,
        bound,
        |window| {
            let Some(start) = window.offset(from) else {
                return (None, Edges::default());
            };
            match resolve(&window.text, start, motion, count, env.usage) {
                Some((at, reach)) => (Some((window.position(at), reach)), window.edges(at..at)),
                None => (None, failed(&window.text, start)),
            }
        },
    )
}

fn document_row_is_blank<D: DisplayDocument + ?Sized>(doc: &mut D, row: RowIndex) -> Option<bool> {
    Some(document_row_string(doc, row)?.trim().is_empty())
}

/// The first row with text in the block starting at `start`.
fn block_home_row<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    total_rows: RowIndex,
    start: RowIndex,
) -> RowIndex {
    let last = start
        .saturating_add(BLOCK_LEADING_BLANK_ROWS)
        .min(total_rows.saturating_sub(1));
    (start..=last)
        .find(|&row| document_row_is_blank(doc, row) == Some(false))
        .unwrap_or(start)
}

/// Where `motion` lands from `from`, `count` blocks away, on the first row with
/// text of the block. Past the last block in its direction the cursor goes to
/// the document edge, like vim's section motions; a document that does not
/// know the kind has nowhere to go.
fn block_position<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    total_rows: RowIndex,
    from: DocPosition,
    motion: BlockMotion,
    count: usize,
) -> Option<DocPosition> {
    let mut row = from.row;
    for _ in 0..count {
        let mut boundary = doc.block_boundary(row, motion.kind, motion.forward);
        if let BlockBoundary::At(start) = boundary {
            // `row` already rests in the block starting at `start`: step to the one before.
            if !motion.forward && start < row && block_home_row(doc, total_rows, start) >= row {
                boundary = doc.block_boundary(start, motion.kind, false);
            }
        }
        match boundary {
            BlockBoundary::Unsupported => return None,
            BlockBoundary::At(start) => row = block_home_row(doc, total_rows, start),
            BlockBoundary::End => {
                row = if motion.forward {
                    total_rows.saturating_sub(1)
                } else {
                    0
                };
                break;
            }
        }
    }
    Some(DocPosition { row, byte_col: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motions::WordTo;
    use crate::row::{DisplayRow, StaticRowsDocument};

    fn env(total_rows: RowIndex) -> RowEnv {
        RowEnv {
            total_rows,
            vim_mode: VimMode::Normal,
            usage: Use::Move,
            scroll_top: 0,
            viewport_rows: 10,
        }
    }

    fn document(rows: usize, blank_at: &[usize]) -> StaticRowsDocument {
        StaticRowsDocument::new(
            (0..rows)
                .map(|row| {
                    let text = if blank_at.contains(&row) {
                        String::new()
                    } else {
                        format!("line {row}. next")
                    };
                    DisplayRow::new(text, Vec::new())
                })
                .collect(),
        )
    }

    #[test]
    fn paragraph_motion_widens_the_scan_past_the_first_window() {
        let mut doc = document(1000, &[700]);
        let found = resolve_rows(
            &mut doc,
            DocPosition::default(),
            Motion::Paragraph { forward: true },
            None,
            env(1000),
        );
        assert_eq!(
            found,
            Some((
                DocPosition {
                    row: 700,
                    byte_col: 0
                },
                Reach::Exclusive
            ))
        );
        let back = resolve_rows(
            &mut doc,
            DocPosition {
                row: 900,
                byte_col: 0,
            },
            Motion::Paragraph { forward: false },
            None,
            env(1000),
        );
        assert_eq!(back.map(|(pos, _)| pos.row), Some(700));
    }

    #[test]
    fn sentence_motion_walks_rows_as_lines() {
        let mut doc = document(3, &[]);
        let found = resolve_rows(
            &mut doc,
            DocPosition::default(),
            Motion::Sentence { forward: true },
            None,
            env(3),
        );
        assert_eq!(
            found.map(|(pos, _)| pos),
            Some(DocPosition {
                row: 0,
                byte_col: "line 0. ".len()
            })
        );
    }

    #[test]
    fn a_word_motion_past_the_last_row_stays_in_the_document() {
        let mut doc = document(2, &[]);
        let found = resolve_rows(
            &mut doc,
            DocPosition {
                row: 1,
                byte_col: 0,
            },
            Motion::Word(WordTo::Next, text::CharClass::Word),
            Some(50),
            env(2),
        );
        assert_eq!(found.map(|(pos, _)| pos.row), Some(1));
    }
}
