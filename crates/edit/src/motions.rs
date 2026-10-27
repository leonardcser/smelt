//! Cursor motions: the `Motion` vocabulary every vim surface shares (prompt,
//! viewers, transcript) and the primitives that resolve it over `&str` byte
//! positions.

use super::scan::Scan;
use super::text::{
    byte_to_cell, cell_to_byte, line_end, line_start, next_grapheme_boundary,
    prev_grapheme_boundary, snap_grapheme, CharClass,
};
use crate::window::BlockMotion;

/// Which neighbouring word a word motion lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordTo {
    /// Start of the next word (`w`, `W`).
    Next,
    /// Start of the current or previous word (`b`, `B`).
    Prev,
    /// End of the current or next word (`e`, `E`).
    End,
    /// End of the previous word (`ge`, `gE`).
    PrevEnd,
}

/// Which line `+`, `-` and `_` move to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineTo {
    /// `count` lines down (`+`).
    Next,
    /// `count` lines up (`-`).
    Prev,
    /// `count - 1` lines down (`_`).
    Current,
}

/// A cursor motion. It carries no count: callers pass the count they parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Down,
    Up,
    Word(WordTo, CharClass),
    LineStart,
    FirstNonBlank,
    /// `+`, `-`, `_`: the first non-blank of a line some lines away.
    Line(LineTo),
    /// `g_`: the last non-blank, `count - 1` lines down.
    LastNonBlank,
    /// `|`: the display column `count` of the line.
    Column,
    LineEnd,
    BufferStart,
    BufferEnd,
    Paragraph {
        forward: bool,
    },
    MatchBracket,
    /// `(` and `)`: the start of the previous or next sentence.
    Sentence {
        forward: bool,
    },
    /// `go`: byte `count` of the buffer.
    ByteOffset,
    /// `f`, `F`, `t`, `T`; `repeat` is `;` / `,`, which steps past a till target
    /// instead of sticking to it.
    Find {
        kind: FindKind,
        ch: char,
        repeat: bool,
    },
    /// `[x` / `]x`: the previous or next block of a kind a document knows.
    Block(BlockMotion),
    /// `H`, `M`, `L`: rows of the visible screen.
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
}

/// What a motion is resolved for: moving the cursor, or spanning text for an
/// operator. Operators change how a few motions behave (`cw`, `dw` on a line's
/// last word, `dl` on its last character).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    Move,
    Operator,
    /// The `c` operator, whose `w` stops at the end of the word.
    Change,
}

/// How far a motion reaches, which decides the span an operator takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Up to, not including, the target.
    Exclusive,
    /// Through the target character.
    Inclusive,
    /// Every line from the cursor's to the target's.
    Linewise,
}

impl Motion {
    /// The motion a key triggers on its own.
    pub fn from_char(c: char) -> Option<Self> {
        let word = |to| Some(Self::Word(to, CharClass::Word));
        let big_word = |to| Some(Self::Word(to, CharClass::WORD));
        match c {
            'h' => Some(Self::Left),
            'l' => Some(Self::Right),
            'j' => Some(Self::Down),
            'k' => Some(Self::Up),
            'w' => word(WordTo::Next),
            'b' => word(WordTo::Prev),
            'e' => word(WordTo::End),
            'W' => big_word(WordTo::Next),
            'B' => big_word(WordTo::Prev),
            'E' => big_word(WordTo::End),
            '0' => Some(Self::LineStart),
            '^' => Some(Self::FirstNonBlank),
            '+' => Some(Self::Line(LineTo::Next)),
            '-' => Some(Self::Line(LineTo::Prev)),
            '_' => Some(Self::Line(LineTo::Current)),
            '|' => Some(Self::Column),
            '$' => Some(Self::LineEnd),
            'G' => Some(Self::BufferEnd),
            '}' => Some(Self::Paragraph { forward: true }),
            '{' => Some(Self::Paragraph { forward: false }),
            '%' => Some(Self::MatchBracket),
            '(' => Some(Self::Sentence { forward: false }),
            ')' => Some(Self::Sentence { forward: true }),
            'H' => Some(Self::ScreenTop),
            'M' => Some(Self::ScreenMiddle),
            'L' => Some(Self::ScreenBottom),
            _ => None,
        }
    }

    /// The motion `g` followed by `c` triggers.
    pub fn from_g(c: char) -> Option<Self> {
        match c {
            'g' => Some(Self::BufferStart),
            '_' => Some(Self::LastNonBlank),
            'o' => Some(Self::ByteOffset),
            'e' => Some(Self::Word(WordTo::PrevEnd, CharClass::Word)),
            'E' => Some(Self::Word(WordTo::PrevEnd, CharClass::WORD)),
            _ => None,
        }
    }

    /// The motion a key press triggers on its own, including arrow keys.
    pub fn from_key(code: crossterm::event::KeyCode) -> Option<Self> {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Char(c) => Self::from_char(c),
            KeyCode::Left | KeyCode::Backspace => Some(Self::Left),
            KeyCode::Right => Some(Self::Right),
            KeyCode::Up => Some(Self::Up),
            KeyCode::Down => Some(Self::Down),
            KeyCode::Home => Some(Self::LineStart),
            KeyCode::End => Some(Self::LineEnd),
            _ => None,
        }
    }

    /// `;` (or `,` when `reverse`): the last find motion, repeated.
    pub fn repeat_find(last: Option<(FindKind, char)>, reverse: bool) -> Option<Self> {
        let (kind, ch) = last?;
        Some(Self::Find {
            kind: if reverse { kind.reversed() } else { kind },
            ch,
            repeat: true,
        })
    }

    /// Motions to another line that keep the cursor's column the way `j` and
    /// `k` do.
    pub fn keeps_column(self, count: Option<usize>) -> bool {
        match self {
            Self::Up | Self::Down | Self::BufferStart | Self::BufferEnd => true,
            Self::MatchBracket => count.is_some(),
            _ => false,
        }
    }

    /// Motions that take the cursor somewhere far; the jump list remembers
    /// where they started.
    pub fn is_jump(self) -> bool {
        matches!(
            self,
            Self::BufferStart
                | Self::BufferEnd
                | Self::Paragraph { .. }
                | Self::MatchBracket
                | Self::Sentence { .. }
                | Self::ByteOffset
                | Self::Block(_)
                | Self::ScreenTop
                | Self::ScreenMiddle
                | Self::ScreenBottom
        )
    }
}

/// Where `motion` takes the cursor from `from` in `buf`, and how far it
/// reaches; `None` when it cannot move (no such character, no bracket pair, or
/// the motion needs a screen).
pub(crate) fn resolve(
    buf: &str,
    from: usize,
    motion: Motion,
    count: Option<usize>,
    usage: Use,
) -> Option<(usize, Reach)> {
    let n = count.unwrap_or(1).max(1);
    let repeat = |step: fn(&str, usize) -> usize| (0..n).fold(from, |p, _| step(buf, p));
    Some(match motion {
        Motion::Left => (repeat(move_left), Reach::Exclusive),
        Motion::Right if usage != Use::Move => (repeat(move_right_inclusive), Reach::Exclusive),
        Motion::Right => (repeat(move_right_normal), Reach::Exclusive),
        Motion::Down => (lines_away(buf, from, n, false)?, Reach::Linewise),
        Motion::Up => (lines_away(buf, from, n, true)?, Reach::Linewise),
        Motion::LineStart => (line_start(buf, from), Reach::Exclusive),
        Motion::FirstNonBlank => (first_non_blank(buf, from), Reach::Exclusive),
        Motion::Line(to) => {
            let pos = match to {
                LineTo::Next => lines_away(buf, from, n, false)?,
                LineTo::Prev => lines_away(buf, from, n, true)?,
                LineTo::Current => lines_away(buf, from, n - 1, false)?,
            };
            (first_non_blank(buf, pos), Reach::Linewise)
        }
        Motion::LastNonBlank => {
            let pos = lines_away(buf, from, n - 1, false)?;
            let sol = line_start(buf, pos);
            let end = sol
                + buf[sol..line_end(buf, pos)]
                    .trim_end_matches([' ', '\t'])
                    .len();
            (prev_grapheme_boundary(buf, end).max(sol), Reach::Inclusive)
        }
        Motion::Column => {
            let sol = line_start(buf, from);
            let line = &buf[sol..line_end(buf, from)];
            (sol + cell_to_byte(line, n - 1), Reach::Exclusive)
        }
        Motion::LineEnd => {
            let end = line_end(buf, lines_away(buf, from, n - 1, false)?);
            if usage == Use::Move {
                (end, Reach::Exclusive)
            } else {
                // An operator takes the last character; an empty line has none.
                let last = prev_grapheme_boundary(buf, end).max(line_start(buf, end));
                (last, Reach::Inclusive)
            }
        }
        Motion::BufferStart => (
            count.map_or(0, |c| goto_line(buf, c.saturating_sub(1))),
            Reach::Linewise,
        ),
        Motion::BufferEnd => (
            count.map_or(buf.len(), |c| goto_line(buf, c.saturating_sub(1))),
            Reach::Linewise,
        ),
        Motion::Word(..) | Motion::Paragraph { .. } | Motion::Sentence { .. } => {
            let mut scan = Scan::new(buf, from);
            let reach = scan_motion(&mut scan, motion, n, usage)?;
            (scan.offset(scan.cur), reach)
        }
        Motion::ByteOffset => {
            let at = count.unwrap_or(1).saturating_sub(1).min(buf.len());
            (snap_grapheme(buf, at), Reach::Exclusive)
        }
        Motion::MatchBracket => match count {
            Some(percent) if percent <= 100 => {
                let lines = buf.matches('\n').count() + 1;
                (
                    goto_line(buf, (percent * lines).div_ceil(100).max(1) - 1),
                    Reach::Linewise,
                )
            }
            Some(_) => return None,
            None => (find_matching_bracket(buf, from)?, Reach::Inclusive),
        },
        Motion::Find { kind, ch, repeat } => (
            find_counted(buf, from, kind, ch, repeat, n)?,
            find_reach(kind),
        ),
        Motion::Block(_) | Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => {
            return None
        }
    })
}

/// The motions that walk text with vim's scans (words, paragraphs, sentences),
/// run on `scan`, which ends where the cursor goes. `None` when the motion
/// fails and cancels an operator; a failed `Use::Move` still leaves the cursor
/// where the scan stopped.
pub(crate) fn scan_motion(scan: &mut Scan, motion: Motion, n: usize, usage: Use) -> Option<Reach> {
    let operator = usage != Use::Move;
    let start = scan.offset(scan.cur);
    let mut reach = Reach::Exclusive;
    match motion {
        // `cw` acts like `ce` on a non-blank, staying on the end of the word it starts on.
        Motion::Word(WordTo::Next, class)
            if usage == Use::Change && !matches!(scan.char_at_cursor(), '\0' | ' ' | '\t') =>
        {
            scan.end_word(n, class == CharClass::WORD, true, false);
            reach = Reach::Inclusive;
        }
        Motion::Word(WordTo::Next, class) => {
            scan.fwd_word(n, class == CharClass::WORD, operator);
        }
        Motion::Word(WordTo::End, class) => {
            scan.end_word(n, class == CharClass::WORD, false, false);
            reach = Reach::Inclusive;
        }
        Motion::Word(WordTo::Prev, class) => {
            if !scan.bck_word(n, class == CharClass::WORD, false) && operator {
                return None;
            }
        }
        Motion::Word(WordTo::PrevEnd, class) => {
            reach = Reach::Inclusive;
            if !scan.bckend_word(n, class == CharClass::WORD, false) && operator {
                return None;
            }
        }
        Motion::Paragraph { forward } => {
            if !scan.findpar(forward, n) {
                return None;
            }
            if scan.oap.inclusive {
                reach = Reach::Inclusive;
            }
            return Some(reach);
        }
        Motion::Sentence { forward } => {
            if !scan.findsent(forward, n) {
                return None;
            }
        }
        _ => return None,
    }
    // A forward motion may not rest on a line's end, which `Use::Move` leaves
    // to the caller so Visual mode can.
    if operator && scan.offset(scan.cur) > start && scan.adjust_cursor() {
        reach = Reach::Inclusive;
    }
    Some(reach)
}

/// The text span an operator takes for a motion from `from` that landed on
/// `pos`: a character range, or the range of the lines it covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Span {
    Chars(usize, usize),
    Lines(usize, usize),
    /// Lines taken because an exclusive end sat in column 0, which differs from
    /// `Lines` only in where a delete leaves the cursor.
    AdjustedLines(usize, usize),
}

pub(crate) fn operator_span(buf: &str, from: usize, pos: usize, reach: Reach) -> Option<Span> {
    let (lo, hi) = if pos < from { (pos, from) } else { (from, pos) };
    match reach {
        Reach::Linewise => Some(Span::Lines(line_start(buf, lo), line_end(buf, hi))),
        Reach::Inclusive => {
            // On an empty line there is no character to include.
            let end = if buf[hi..].starts_with('\n') || hi >= buf.len() {
                hi
            } else {
                advance_chars(buf, hi, 1)
            };
            Some(Span::Chars(lo, end))
        }
        Reach::Exclusive if hi > lo && line_start(buf, hi) == hi => {
            // An exclusive motion ending in column 0 stops at the end of the
            // previous line, and becomes linewise when it starts at or before
            // the first non-blank of its line.
            let before = hi - 1;
            if lo <= first_non_blank(buf, lo) {
                Some(Span::AdjustedLines(line_start(buf, lo), before))
            } else {
                (lo < before).then_some(Span::Chars(lo, before))
            }
        }
        Reach::Exclusive => Some(Span::Chars(lo, hi)),
    }
}

/// Direction + variant for `f`/`F`/`t`/`T`-style find-char motions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindKind {
    Forward,
    ForwardTill,
    Backward,
    BackwardTill,
}

impl FindKind {
    fn till(self) -> bool {
        matches!(self, FindKind::ForwardTill | FindKind::BackwardTill)
    }

    /// Reverse direction (for `,` repeating last `f`/`t`).
    pub fn reversed(self) -> Self {
        match self {
            FindKind::Forward => FindKind::Backward,
            FindKind::ForwardTill => FindKind::BackwardTill,
            FindKind::Backward => FindKind::Forward,
            FindKind::BackwardTill => FindKind::ForwardTill,
        }
    }
}

pub(crate) fn move_left(buf: &str, cpos: usize) -> usize {
    if cpos == 0 {
        return 0;
    }
    let sol = line_start(buf, cpos);
    if cpos <= sol {
        return cpos; // Don't cross line boundary.
    }
    prev_grapheme_boundary(buf, cpos)
}

/// Move right, staying within the current line and not landing on '\n'.
pub(crate) fn move_right_normal(buf: &str, cpos: usize) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let eol = line_end(buf, cpos);
    let last_on_line = if eol > line_start(buf, cpos) {
        prev_grapheme_boundary(buf, eol)
    } else {
        eol // Empty line - stay put.
    };
    if cpos >= last_on_line {
        return cpos;
    }
    next_grapheme_boundary(buf, cpos)
}

/// Move right inclusive (for operator motions on `l`): onto the line's end,
/// never across it.
pub(crate) fn move_right_inclusive(buf: &str, cpos: usize) -> usize {
    next_grapheme_boundary(buf, cpos).min(line_end(buf, cpos))
}

/// End of line for normal mode (on last char, not past it).
pub(crate) fn line_end_normal(buf: &str, cpos: usize) -> usize {
    let end = line_end(buf, cpos);
    if end > line_start(buf, cpos) {
        prev_grapheme_boundary(buf, end)
    } else {
        end
    }
}

pub(crate) fn first_non_blank(buf: &str, cpos: usize) -> usize {
    first_non_blank_at(buf, line_start(buf, cpos))
}

pub(crate) fn first_non_blank_at(buf: &str, from: usize) -> usize {
    let eol = line_end(buf, from);
    let mut pos = from;
    while pos < eol {
        let next = next_grapheme_boundary(buf, pos);
        let c = buf[pos..next].chars().next().unwrap();
        if c != ' ' && c != '\t' {
            break;
        }
        pos = next;
    }
    pos
}

/// Content range of the current line, no newline.
pub(crate) fn current_line_content_range(buf: &str, cpos: usize) -> (usize, usize) {
    (line_start(buf, cpos), line_end(buf, cpos))
}

pub(crate) fn goto_line(buf: &str, line_idx: usize) -> usize {
    let mut pos = 0;
    for _ in 0..line_idx {
        match buf[pos..].find('\n') {
            Some(i) => pos += i + 1,
            None => return pos,
        }
    }
    pos
}

/// Move down one line, preserving preferred column (`curswant`). Returns `(new_cpos, actual_col)`.
/// Columns are in terminal display cells (wide-char safe); the returned byte
/// offset always lands on a char boundary.
pub(crate) fn move_down_col(buf: &str, cpos: usize, want_col: Option<usize>) -> (usize, usize) {
    let sol = line_start(buf, cpos);
    let eol = line_end(buf, cpos);
    let col = want_col.unwrap_or_else(|| byte_to_cell(&buf[sol..eol], cpos - sol));
    if eol >= buf.len() {
        return (cpos, col);
    }
    let next_sol = eol + 1;
    let next_eol = line_end(buf, next_sol);
    let next_line = &buf[next_sol..next_eol];
    (next_sol + cell_to_byte(next_line, col), col)
}

pub(crate) fn move_up_col(buf: &str, cpos: usize, want_col: Option<usize>) -> (usize, usize) {
    let sol = line_start(buf, cpos);
    let eol = line_end(buf, cpos);
    let col = want_col.unwrap_or_else(|| byte_to_cell(&buf[sol..eol], cpos - sol));
    if sol == 0 {
        return (cpos, col);
    }
    let prev_eol = sol - 1;
    let prev_sol = line_start(buf, prev_eol);
    let prev_line = &buf[prev_sol..prev_eol];
    (prev_sol + cell_to_byte(prev_line, col), col)
}

/// The position `steps` lines below (or above) `from`, stopping at the buffer's
/// last (first) line; `None` when there is nowhere to go.
fn lines_away(buf: &str, from: usize, steps: usize, up: bool) -> Option<usize> {
    let step = if up { move_up } else { move_down };
    let target = (0..steps).fold(from, |p, _| step(buf, p));
    (steps == 0 || line_start(buf, target) != line_start(buf, from)).then_some(target)
}

pub(crate) fn move_down(buf: &str, cpos: usize) -> usize {
    move_down_col(buf, cpos, None).0
}

pub(crate) fn move_up(buf: &str, cpos: usize) -> usize {
    move_up_col(buf, cpos, None).0
}

// ── Find char on line ───────────────────────────────────────────────────────

pub(crate) fn find_char(buf: &str, cpos: usize, kind: FindKind, ch: char) -> Option<usize> {
    let sol = line_start(buf, cpos);
    let eol = line_end(buf, cpos);

    match kind {
        FindKind::Forward | FindKind::ForwardTill => {
            // When `cpos` sits on the line's terminating `\n` (insert mode at
            // end-of-line),  `next_grapheme_boundary` advances past the newline
            // while `eol` stays put, which inverts the slice range. Clamp.
            let start = next_grapheme_boundary(buf, cpos).min(eol);
            for (i, grapheme) in smelt_buffer::cell_width::grapheme_indices(&buf[start..eol]) {
                if grapheme.starts_with(ch) {
                    let pos = start + i;
                    return Some(match kind {
                        FindKind::ForwardTill => prev_grapheme_boundary(buf, pos).max(cpos),
                        _ => pos,
                    });
                }
            }
            None
        }
        FindKind::Backward | FindKind::BackwardTill => {
            let search = &buf[sol..cpos];
            for (i, grapheme) in smelt_buffer::cell_width::grapheme_indices(search).rev() {
                if grapheme.starts_with(ch) {
                    let pos = sol + i;
                    return Some(match kind {
                        FindKind::BackwardTill => next_grapheme_boundary(buf, pos).min(cpos),
                        _ => pos,
                    });
                }
            }
            None
        }
    }
}

/// One step of a find-char motion on the line holding `pos`. A repeat (`;`,
/// `,`) of a till search starts past its last target so it does not get stuck.
pub(crate) fn find_step(
    buf: &str,
    pos: usize,
    kind: FindKind,
    ch: char,
    repeat: bool,
) -> Option<usize> {
    let search_pos = match kind {
        FindKind::ForwardTill if repeat => next_grapheme_boundary(buf, pos),
        FindKind::BackwardTill if repeat => prev_grapheme_boundary(buf, pos),
        _ => pos,
    };
    find_char(buf, search_pos, kind, ch)
}

/// `n` find steps from `pos`. A counted till (`2tx`) stops short of the n-th
/// occurrence, so it counts occurrences rather than repeating a till that would
/// stick to the adjacent one.
pub(crate) fn find_counted(
    buf: &str,
    pos: usize,
    kind: FindKind,
    ch: char,
    repeat: bool,
    n: usize,
) -> Option<usize> {
    if repeat || !kind.till() {
        return (0..n).try_fold(pos, |p, _| find_step(buf, p, kind, ch, repeat));
    }
    let plain = match kind {
        FindKind::ForwardTill => FindKind::Forward,
        _ => FindKind::Backward,
    };
    let target = (0..n).try_fold(pos, |p, _| find_char(buf, p, plain, ch))?;
    Some(match kind {
        FindKind::ForwardTill => prev_grapheme_boundary(buf, target),
        _ => next_grapheme_boundary(buf, target),
    })
}

/// Forward finds include their target character, backward finds stop short of it.
pub(crate) fn find_reach(kind: FindKind) -> Reach {
    match kind {
        FindKind::Forward | FindKind::ForwardTill => Reach::Inclusive,
        FindKind::Backward | FindKind::BackwardTill => Reach::Exclusive,
    }
}

// ── Match bracket ───────────────────────────────────────────────────────────

pub(crate) fn find_matching_bracket(buf: &str, cpos: usize) -> Option<usize> {
    let eol = line_end(buf, cpos);
    let mut start = cpos;
    while start < eol {
        let c = buf[start..].chars().next()?;
        if matches!(c, '(' | ')' | '[' | ']' | '{' | '}') {
            break;
        }
        start += c.len_utf8();
    }
    if start >= eol && (start >= buf.len() || buf.as_bytes()[start] == b'\n') {
        return None;
    }
    let bracket = buf[start..].chars().next()?;
    let (open, close, forward) = match bracket {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let mut depth = 0i32;
    if forward {
        for (i, c) in buf[start..].char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(start + i);
                }
            }
        }
    } else {
        for (i, c) in buf[..=start].char_indices().rev() {
            if c == close {
                depth += 1;
            } else if c == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

// ── Char-step helpers ───────────────────────────────────────────────────────

pub(crate) fn advance_chars(buf: &str, pos: usize, n: usize) -> usize {
    let mut p = pos;
    for _ in 0..n {
        if p >= buf.len() {
            break;
        }
        p = next_grapheme_boundary(buf, p);
    }
    p
}

/// Clamp cursor to valid normal-mode position.
/// If the buffer ends with `'\n'`, `buf.len()` is valid (cursor on the empty trailing line).
pub(crate) fn clamp_normal(buf: &str, cpos: &mut usize) {
    if buf.is_empty() {
        *cpos = 0;
        return;
    }
    *cpos = snap_grapheme(buf, *cpos);
    if *cpos >= buf.len() {
        *cpos = if buf.ends_with('\n') {
            buf.len()
        } else {
            prev_grapheme_boundary(buf, buf.len())
        };
    } else if buf.as_bytes()[*cpos] == b'\n' && *cpos > 0 {
        // Cursor must not sit on an interior '\n'.
        let sol = line_start(buf, *cpos);
        if *cpos > sol {
            *cpos = prev_grapheme_boundary(buf, *cpos);
        }
    }
    debug_assert_eq!(
        snap_grapheme(buf, *cpos),
        *cpos,
        "clamp_normal postcondition: cpos {} not on grapheme boundary in buf len {}",
        *cpos,
        buf.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── FindKind ──────────────────────────────────────────────────────────

    #[test]
    fn find_kind_reversed_flips_direction_keeping_till_flavour() {
        assert_eq!(FindKind::Forward.reversed(), FindKind::Backward);
        assert_eq!(FindKind::Backward.reversed(), FindKind::Forward);
        assert_eq!(FindKind::ForwardTill.reversed(), FindKind::BackwardTill);
        assert_eq!(FindKind::BackwardTill.reversed(), FindKind::ForwardTill);
    }

    // ── Horizontal: move_left / move_right ────────────────────────────────

    #[test]
    fn move_left_steps_back_one_char_within_a_line() {
        assert_eq!(move_left("abc", 2), 1);
        assert_eq!(move_left("abc", 1), 0);
    }

    #[test]
    fn move_left_at_start_of_buffer_stays() {
        assert_eq!(move_left("abc", 0), 0);
        assert_eq!(move_left("", 0), 0);
    }

    #[test]
    fn move_left_does_not_cross_line_boundary() {
        // "abc\ndef": at start of "def" (byte 4), moving left should stay.
        assert_eq!(move_left("abc\ndef", 4), 4);
    }

    #[test]
    fn move_left_walks_past_wide_chars() {
        // "a漢b": boundaries 0,1,4,5. From 4 (start of b) we land on 1 (start of 漢).
        assert_eq!(move_left("a漢b", 4), 1);
        assert_eq!(move_left("a漢b", 1), 0);
    }

    #[test]
    fn horizontal_motion_keeps_graphemes_atomic() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}"] {
            let text = format!("a{grapheme}b");
            let start = 1;
            let end = start + grapheme.len();
            assert_eq!(move_right_normal(&text, start), end, "{grapheme:?}");
            assert_eq!(move_left(&text, end), start, "{grapheme:?}");
        }
    }

    #[test]
    fn move_right_normal_stops_at_last_char_on_line() {
        // "abc": last char 'c' is at byte 2. Moving right from 0 → 1, 1 → 2,
        // 2 → 2 (already at last char, don't fall off).
        assert_eq!(move_right_normal("abc", 0), 1);
        assert_eq!(move_right_normal("abc", 1), 2);
        assert_eq!(move_right_normal("abc", 2), 2);
    }

    #[test]
    fn move_right_normal_does_not_land_on_newline() {
        // "ab\ncd": at byte 1 ('b'), right shouldn't move onto byte 2 ('\n').
        assert_eq!(move_right_normal("ab\ncd", 1), 1);
    }

    #[test]
    fn move_right_normal_on_empty_buffer_is_zero() {
        assert_eq!(move_right_normal("", 0), 0);
    }

    #[test]
    fn move_right_normal_stays_on_empty_line() {
        // Cursor on an empty line - the start/end are the same.
        assert_eq!(move_right_normal("\nabc", 0), 0);
    }

    #[test]
    fn move_right_inclusive_stops_at_the_line_end() {
        // The operator-mode variant (used as the motion target for `l`/`dl`)
        // reaches the line end so the last character is included, never the
        // next line.
        assert_eq!(move_right_inclusive("ab\ncd", 1), 2);
        assert_eq!(move_right_inclusive("ab\ncd", 2), 2);
        assert_eq!(move_right_inclusive("ab\n\ncd", 3), 3);
    }

    // ── line_end_normal / first_non_blank ─────────────────────────────────

    #[test]
    fn line_end_normal_lands_on_last_char_not_past_it() {
        assert_eq!(line_end_normal("abc", 0), 2);
        // Multi-line: "abc\ndef" - at byte 0, end of line 0 is byte 2.
        assert_eq!(line_end_normal("abc\ndef", 0), 2);
        // On line 1 (byte 4), end is byte 6 ('f').
        assert_eq!(line_end_normal("abc\ndef", 4), 6);
    }

    #[test]
    fn line_end_normal_on_empty_line_stays_at_line_start() {
        // "ab\n\ncd": line 1 is empty (just between newlines).
        assert_eq!(line_end_normal("ab\n\ncd", 3), 3);
    }

    #[test]
    fn first_non_blank_skips_leading_whitespace_to_first_content_char() {
        assert_eq!(first_non_blank("  abc", 3), 2);
        assert_eq!(first_non_blank("\t\thello", 4), 2);
        assert_eq!(first_non_blank(" \u{301}hello", 4), " \u{301}".len());
        assert_eq!(first_non_blank("\u{600} hello", 4), 0);
        assert_eq!(first_non_blank("abc", 1), 0);
    }

    #[test]
    fn first_non_blank_returns_eol_when_line_is_all_whitespace() {
        // The function walks while pos < eol; if no non-blank found, returns eol.
        assert_eq!(first_non_blank("   ", 0), 3);
    }

    // ── goto_line ─────────────────────────────────────────────────────────

    #[test]
    fn goto_line_jumps_to_first_byte_of_target_line() {
        let buf = "a\nbb\nccc";
        assert_eq!(goto_line(buf, 0), 0);
        assert_eq!(goto_line(buf, 1), 2);
        assert_eq!(goto_line(buf, 2), 5);
    }

    #[test]
    fn goto_line_past_last_line_returns_last_known_position() {
        let buf = "a\nb";
        assert_eq!(goto_line(buf, 99), 2);
    }

    // ── vertical: move_down / move_up ─────────────────────────────────────

    #[test]
    fn move_down_preserves_column_when_target_line_is_long_enough() {
        // "abc\ndef": from col 2 ('c'), down should land on col 2 ('f' = byte 6).
        assert_eq!(move_down("abc\ndef", 2), 6);
    }

    #[test]
    fn move_down_lands_at_logical_column_past_last_char_for_short_target() {
        // "abcde\nxy": from col 4 ('e'), down lands at byte 8 - the position
        // *past* 'y'. The function returns the "logical" target; callers run
        // `clamp_normal` to pull the cursor back to a valid char.
        assert_eq!(move_down("abcde\nxy", 4), 8);
    }

    #[test]
    fn move_down_on_last_line_stays_put() {
        let buf = "abc\ndef";
        assert_eq!(move_down(buf, 5), 5);
    }

    #[test]
    fn move_up_preserves_column_when_prev_line_is_long_enough() {
        assert_eq!(move_up("abc\ndef", 6), 2);
    }

    #[test]
    fn move_up_lands_at_logical_column_for_short_prev_line() {
        // "xy\nabcde": from col 4 of line 1 (byte 7 = 'd'), up returns the
        // logical position one past the last char of "xy". Callers run
        // `clamp_normal` to pull cursor onto a valid char.
        assert_eq!(move_up("xy\nabcde", 7), 2);
    }

    #[test]
    fn move_up_on_first_line_stays_put() {
        assert_eq!(move_up("abc", 1), 1);
    }

    #[test]
    fn move_down_col_propagates_curswant_through_short_lines() {
        // From col 5 in long line, down to short line clamps; col is preserved
        // for the next move_down so a third (long) line restores col 5.
        let buf = "longline\nxy\nbacktoit";
        let (pos1, col1) = move_down_col(buf, 5, None);
        // Land on 'xy' line, col clamped to 1.
        assert_eq!(pos1, 11);
        assert_eq!(col1, 5, "curswant preserved");
        // Second move with the remembered col.
        let (pos2, col2) = move_down_col(buf, pos1, Some(col1));
        assert_eq!(pos2, 17, "back to col 5 on the long line");
        assert_eq!(col2, 5);
    }

    // ── find_char / repeat_find ───────────────────────────────────────────

    #[test]
    fn find_char_forward_lands_on_match() {
        // "abc abc" - f c from byte 0 → byte 2.
        assert_eq!(find_char("abc abc", 0, FindKind::Forward, 'c'), Some(2));
    }

    #[test]
    fn find_char_forward_till_lands_one_char_before_match() {
        // t c from byte 0 → byte 1 (one before the 'c').
        assert_eq!(find_char("abc abc", 0, FindKind::ForwardTill, 'c'), Some(1));
    }

    #[test]
    fn find_char_backward_lands_on_match() {
        // F a from end of "abc abc" → byte 4 (the second 'a').
        assert_eq!(find_char("abc abc", 6, FindKind::Backward, 'a'), Some(4));
    }

    #[test]
    fn find_char_backward_till_lands_one_char_after_match() {
        // T a from byte 6 → byte 5 (one after 'a').
        assert_eq!(
            find_char("abc abc", 6, FindKind::BackwardTill, 'a'),
            Some(5)
        );
    }

    #[test]
    fn find_char_returns_none_when_no_match_on_line() {
        assert_eq!(find_char("abc", 0, FindKind::Forward, 'z'), None);
    }

    #[test]
    fn find_char_does_not_cross_newline() {
        // 'z' is on line 2 - forward search on line 0 must not find it.
        assert_eq!(find_char("abc\ndefz", 0, FindKind::Forward, 'z'), None);
    }

    #[test]
    fn repeat_find_advances_past_previous_match_to_find_the_next() {
        // From byte 2 (a 'c'), `;` (Forward 'c') should land on byte 6 (next 'c').
        let find = Motion::Find {
            kind: FindKind::Forward,
            ch: 'c',
            repeat: true,
        };
        assert_eq!(
            resolve("abc abc", 2, find, None, Use::Move),
            Some((6, Reach::Inclusive))
        );
    }

    #[test]
    fn repeating_a_till_find_steps_past_its_last_target() {
        let find = Motion::Find {
            kind: FindKind::ForwardTill,
            ch: 'c',
            repeat: true,
        };
        // `tc` stopped at byte 1; `;` goes on to the next `c`, stopping at byte 5.
        assert_eq!(
            resolve("abc abc", 1, find, None, Use::Move),
            Some((5, Reach::Inclusive))
        );
    }

    #[test]
    fn a_find_that_matches_nothing_does_not_move() {
        let find = Motion::Find {
            kind: FindKind::Forward,
            ch: 'z',
            repeat: false,
        };
        assert_eq!(resolve("abc", 0, find, None, Use::Move), None);
        assert_eq!(resolve("abc abc", 0, find, Some(2), Use::Move), None);
    }

    #[test]
    fn word_end_backward_lands_on_the_last_char_of_the_previous_word() {
        let ge = |pos| {
            let motion = Motion::Word(WordTo::PrevEnd, CharClass::Word);
            resolve("foo bar  baz", pos, motion, None, Use::Move).map(|(p, _)| p)
        };
        assert_eq!(ge(9), Some(6), "from the start of baz");
        assert_eq!(ge(5), Some(2), "from inside bar");
        assert_eq!(ge(4), Some(2), "from the start of bar");
        assert_eq!(ge(2), Some(0), "no earlier word stays at the start");
    }

    #[test]
    fn paragraph_motions_stop_on_empty_lines_and_fail_past_the_buffer() {
        let b = "one\ntwo\n\nthree\n\n\nfour";
        let para = |forward, from, count| {
            let motion = Motion::Paragraph { forward };
            resolve(b, from, motion, count, Use::Move)
        };
        assert_eq!(para(true, 0, None), Some((8, Reach::Exclusive)));
        assert_eq!(para(true, 8, None), Some((15, Reach::Exclusive)));
        assert_eq!(
            para(true, 16, None),
            Some((b.len() - 1, Reach::Inclusive)),
            "the last line ends on its last character"
        );
        assert_eq!(para(true, 0, Some(9)), None);
        assert_eq!(para(false, b.len(), None), Some((16, Reach::Exclusive)));
        assert_eq!(para(false, 5, None), Some((0, Reach::Exclusive)));
    }

    #[test]
    fn paragraphs_keep_whitespace_only_lines_inside() {
        let motion = Motion::Paragraph { forward: true };
        assert_eq!(
            resolve("one\n  \ntwo\n\nthree", 0, motion, None, Use::Move),
            Some((11, Reach::Exclusive))
        );
    }

    #[test]
    fn dw_on_the_last_word_of_a_line_stops_at_the_line_end() {
        let w = Motion::Word(WordTo::Next, CharClass::Word);
        assert_eq!(
            resolve("foo bar\nbaz", 4, w, None, Use::Operator),
            Some((6, Reach::Inclusive))
        );
        assert_eq!(
            resolve("foo bar\nbaz", 4, w, None, Use::Move),
            Some((8, Reach::Exclusive))
        );
    }

    #[test]
    fn cw_on_a_word_changes_to_its_end_and_keeps_the_spacing() {
        let w = Motion::Word(WordTo::Next, CharClass::Word);
        assert_eq!(
            resolve("foo bar", 0, w, None, Use::Change),
            Some((2, Reach::Inclusive))
        );
    }

    #[test]
    fn operator_spans_follow_vim_rules_for_exclusive_motions_ending_in_column_zero() {
        let buf = "ab cd\nef\n\ngh";
        // From the start of the line, the span takes whole lines.
        assert_eq!(
            operator_span(buf, 0, 9, Reach::Exclusive),
            Some(Span::AdjustedLines(0, 8))
        );
        // From mid-line it stops at the end of the previous line.
        assert_eq!(
            operator_span(buf, 3, 9, Reach::Exclusive),
            Some(Span::Chars(3, 8))
        );
        assert_eq!(
            operator_span(buf, 3, 4, Reach::Inclusive),
            Some(Span::Chars(3, 5))
        );
        assert_eq!(
            operator_span(buf, 3, 9, Reach::Linewise),
            Some(Span::Lines(0, 9))
        );
    }

    // ── find_matching_bracket ─────────────────────────────────────────────

    #[test]
    fn find_matching_bracket_steps_from_open_to_close_on_same_line() {
        // "(abc)" - `%` at byte 0 should land on byte 4.
        assert_eq!(find_matching_bracket("(abc)", 0), Some(4));
        // From the close back to the open.
        assert_eq!(find_matching_bracket("(abc)", 4), Some(0));
    }

    #[test]
    fn find_matching_bracket_handles_nested_pairs() {
        // "((x))" - outer open at 0 matches close at 4.
        assert_eq!(find_matching_bracket("((x))", 0), Some(4));
        // Inner open at 1 matches close at 3.
        assert_eq!(find_matching_bracket("((x))", 1), Some(3));
    }

    #[test]
    fn find_matching_bracket_skips_forward_from_non_bracket_char_on_line() {
        // From the 'a' in "(abc)", we scan forward to the first bracket-like
        // char on this line - that's `)` at byte 4. Vim's `%` matches the
        // opening `(` for it.
        assert_eq!(find_matching_bracket("(abc)", 1), Some(0));
    }

    #[test]
    fn find_matching_bracket_returns_none_when_no_bracket_on_line() {
        assert_eq!(find_matching_bracket("hello", 0), None);
    }

    #[test]
    fn find_matching_bracket_returns_none_on_unmatched_bracket() {
        assert_eq!(find_matching_bracket("(unmatched", 0), None);
    }

    // ── advance_chars ─────────────────────────────────────

    #[test]
    fn advance_chars_walks_n_char_boundaries_clamping_at_end() {
        assert_eq!(advance_chars("abcd", 0, 2), 2);
        assert_eq!(advance_chars("abcd", 0, 10), 4); // clamped to len
        assert_eq!(advance_chars("a漢b", 0, 2), 4); // wide char counts as one step
    }

    // ── clamp_normal ──────────────────────────────────────────────────────

    #[test]
    fn clamp_normal_pulls_cursor_back_from_past_end() {
        // No trailing newline - cursor at len becomes len-1 (boundary).
        let mut cpos = 99;
        clamp_normal("abc", &mut cpos);
        assert_eq!(cpos, 2);
    }

    #[test]
    fn clamp_normal_leaves_cursor_at_len_when_buffer_ends_with_newline() {
        // Trailing newline → an empty trailing line where cpos==len is valid.
        let mut cpos = 99;
        clamp_normal("abc\n", &mut cpos);
        assert_eq!(cpos, 4);
    }

    #[test]
    fn clamp_normal_on_empty_buffer_pins_cursor_to_zero() {
        let mut cpos = 42;
        clamp_normal("", &mut cpos);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn clamp_normal_moves_cursor_off_an_interior_newline() {
        // "abc\ndef" - cursor at byte 3 (the '\n') must shift back to byte 2.
        let mut cpos = 3;
        clamp_normal("abc\ndef", &mut cpos);
        assert_eq!(cpos, 2);
    }
}
