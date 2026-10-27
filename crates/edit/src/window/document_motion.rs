//! Cursor motions over row-backed documents: the `Motion` vocabulary resolved
//! against display rows, as `motions::resolve` does against plain text.

use super::document_view::{
    document_byte_col_at_cell, document_position_cell, document_row_string, BlockMotion,
};
use super::*;
use crate::motions::{find_counted, find_reach, scan_motion, LineTo, Motion, Reach, Use};
use crate::row::{BlockBoundary, DisplayDocument};
use crate::scan::Scan;

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

/// Rows `scan_brackets` looks through before giving up on a match.
const BRACKET_SCAN_ROWS: RowIndex = 10_000;

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
            scan_rows(doc, from, motion, n, env)?
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
            None => (
                matching_bracket(doc, from, env.total_rows)?,
                Reach::Inclusive,
            ),
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
            let offset = match motion {
                Motion::ScreenTop => (n as RowIndex - 1).min(visible - 1),
                Motion::ScreenMiddle => (visible - 1) / 2,
                _ => (visible - 1).saturating_sub(n as RowIndex - 1),
            };
            let row = env.scroll_top.saturating_add(offset).min(last);
            let line = row_text(doc, row)?;
            (at(row, first_non_blank(&line)), Reach::Linewise)
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

/// Rows a scan starts with on each side of the cursor; it widens while the
/// scan reaches the edge of what was loaded.
const SCAN_ROWS: RowIndex = 128;

/// Run a word, sentence or paragraph motion over the rows around `from`, each
/// row a line, as `motions::resolve` does over plain text.
fn scan_rows<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    from: DocPosition,
    motion: Motion,
    n: usize,
    env: RowEnv,
) -> Option<(DocPosition, Reach)> {
    let last = env.total_rows.saturating_sub(1);
    let mut radius = SCAN_ROWS;
    loop {
        let first_row = from.row.saturating_sub(radius);
        let last_row = from.row.saturating_add(radius).min(last);
        let mut window = String::new();
        let mut starts = Vec::new();
        for row in first_row..=last_row {
            if row > first_row {
                window.push('\n');
            }
            starts.push(window.len());
            // A row the document cannot supply reads as an empty line.
            window.push_str(&document_row_string(doc, row).unwrap_or_default());
        }
        let line = usize::try_from(from.row - first_row).ok()?;
        let col = from.byte_col.min(window.len() - starts[line]);
        let mut scan = Scan::new(&window, starts[line] + col);
        let result = scan_motion(&mut scan, motion, n, env.usage);
        let cut_before = first_row > 0 && scan.cur.line == 0;
        let cut_after = last_row < last && scan.cur.line == starts.len() - 1;
        let whole_document = first_row == 0 && last_row == last;
        if (result.is_none() || cut_before || cut_after) && !whole_document {
            radius = radius.saturating_mul(2);
            continue;
        }
        let reach = result?;
        return Some((
            DocPosition {
                row: first_row + scan.cur.line as RowIndex,
                byte_col: scan.cur.col,
            },
            reach,
        ));
    }
}

fn document_row_is_blank<D: DisplayDocument + ?Sized>(doc: &mut D, row: RowIndex) -> Option<bool> {
    Some(document_row_string(doc, row)?.trim().is_empty())
}

/// The bracket matching the first one at or after `from` on its row (vim `%`),
/// looking through rows on either side.
fn matching_bracket<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    from: DocPosition,
    total_rows: RowIndex,
) -> Option<DocPosition> {
    let line = document_row_string(doc, from.row)?;
    let col = text::snap_grapheme(&line, from.byte_col.min(line.len()));
    let (start, bracket) = line[col..]
        .char_indices()
        .map(|(i, c)| (col + i, c))
        .find(|&(_, c)| "()[]{}".contains(c))?;
    let (open, close, forward) = match bracket {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let (this, other) = if forward {
        (open, close)
    } else {
        (close, open)
    };
    let mut depth = 0i32;
    let rows: Box<dyn Iterator<Item = RowIndex>> = if forward {
        Box::new(from.row..from.row.saturating_add(BRACKET_SCAN_ROWS).min(total_rows))
    } else {
        Box::new((from.row.saturating_sub(BRACKET_SCAN_ROWS)..=from.row).rev())
    };
    for row in rows {
        let line = document_row_string(doc, row)?;
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut visit = |byte_col: usize, c: char| {
            if c == this {
                depth += 1;
            } else if c == other {
                depth -= 1;
                if depth == 0 {
                    return Some(DocPosition { row, byte_col });
                }
            }
            None
        };
        let found = if forward {
            chars
                .iter()
                .filter(|&&(i, _)| row != from.row || i >= start)
                .find_map(|&(i, c)| visit(i, c))
        } else {
            chars
                .iter()
                .rev()
                .filter(|&&(i, _)| row != from.row || i <= start)
                .find_map(|&(i, c)| visit(i, c))
        };
        if found.is_some() {
            return found;
        }
    }
    None
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
