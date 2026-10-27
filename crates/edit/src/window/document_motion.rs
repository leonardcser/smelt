//! Cursor motions over row-backed documents: the `Motion` vocabulary resolved
//! against display rows, as `motions::resolve` does against plain text.

use super::document_view::{
    document_byte_col_at_cell, document_position_cell, document_row_string, BlockMotion,
};
use super::*;
use crate::motions::{find_counted, find_reach, LineTo, Motion, Reach, Use, WordTo};
use crate::row::{BlockBoundary, DisplayDocument};

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
        Motion::Word(to, class) => (word_position(doc, from, to, class, n, env)?, word_reach(to)),
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
        Motion::Paragraph { forward } => {
            let mut pos = from;
            for _ in 0..n {
                pos = paragraph_position(doc, env.total_rows, pos, forward)?;
            }
            (pos, Reach::Exclusive)
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
        Motion::Sentence { .. } | Motion::ByteOffset => return None,
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

fn word_reach(to: WordTo) -> Reach {
    match to {
        WordTo::Next | WordTo::Prev => Reach::Exclusive,
        WordTo::End | WordTo::PrevEnd => Reach::Inclusive,
    }
}

fn first_non_blank(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn word_position<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    from: DocPosition,
    to: WordTo,
    class: text::CharClass,
    n: usize,
    env: RowEnv,
) -> Option<DocPosition> {
    let last = env.total_rows.saturating_sub(1);
    let mut pos = from;
    for step in 0..n {
        let line = document_row_string(doc, pos.row)?;
        pos.byte_col = text::snap_grapheme(&line, pos.byte_col.min(line.len()));
        match to {
            WordTo::Next => {
                let col = text::word_forward_pos(&line, pos.byte_col, class);
                if col < line.len() || pos.row >= last {
                    pos.byte_col = col;
                } else if env.usage != Use::Move && step + 1 == n {
                    // An operator over a row's last word stops at the row's end.
                    pos.byte_col = line.len();
                } else {
                    pos = DocPosition {
                        row: pos.row + 1,
                        byte_col: 0,
                    };
                }
            }
            WordTo::Prev => {
                let col = text::word_backward_pos(&line, pos.byte_col, class);
                if col > 0 || pos.row == 0 {
                    pos.byte_col = col;
                } else {
                    pos.row -= 1;
                    pos.byte_col = document_row_string(doc, pos.row)?.len();
                }
            }
            WordTo::End => {
                let col = text::word_end_pos(&line, pos.byte_col, class);
                if col > pos.byte_col || pos.row >= last {
                    pos.byte_col = col;
                } else {
                    pos = DocPosition {
                        row: pos.row + 1,
                        byte_col: 0,
                    };
                }
            }
            WordTo::PrevEnd => {
                if line[..pos.byte_col].chars().any(|c| !c.is_whitespace()) {
                    pos.byte_col = text::word_end_backward_pos(&line, pos.byte_col, class);
                } else {
                    pos = previous_word_end(doc, pos.row, class)?;
                }
            }
        }
    }
    Some(pos)
}

/// The last character of the closest word on a row before `row`, or the start
/// of the document when there is none.
fn previous_word_end<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    row: RowIndex,
    class: text::CharClass,
) -> Option<DocPosition> {
    for row in (0..row).rev() {
        let line = document_row_string(doc, row)?;
        if line.chars().any(|c| !c.is_whitespace()) {
            return Some(DocPosition {
                row,
                byte_col: text::word_end_backward_pos(&line, line.len(), class),
            });
        }
    }
    Some(DocPosition::default())
}

fn document_row_is_blank<D: DisplayDocument + ?Sized>(doc: &mut D, row: RowIndex) -> Option<bool> {
    Some(document_row_string(doc, row)?.trim().is_empty())
}

/// The start of the next (`}`) or previous (`{`) blank row past the paragraph at
/// `pos`, or the end or start of the document when there is none.
fn paragraph_position<D: DisplayDocument + ?Sized>(
    doc: &mut D,
    total_rows: RowIndex,
    pos: DocPosition,
    forward: bool,
) -> Option<DocPosition> {
    let last = total_rows.saturating_sub(1);
    let mut row = pos.row.min(last);
    for want_blank in [true, false] {
        while document_row_is_blank(doc, row)? == want_blank {
            if forward && row == last {
                let byte_col = document_row_string(doc, row)?.len();
                return Some(DocPosition { row, byte_col });
            }
            if !forward && row == 0 {
                return Some(DocPosition::default());
            }
            row = if forward { row + 1 } else { row - 1 };
        }
    }
    Some(DocPosition { row, byte_col: 0 })
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
