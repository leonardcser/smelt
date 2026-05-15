//! Pure cursor-motion primitives over `&str` byte positions.

use super::text::{
    char_class, line_end, line_start, next_char_boundary, prev_char_boundary, CharClass,
};

/// Direction + variant for `f`/`F`/`t`/`T`-style find-char motions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindKind {
    Forward,
    ForwardTill,
    Backward,
    BackwardTill,
}

impl FindKind {
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
    prev_char_boundary(buf, cpos)
}

/// Move right, staying within the current line and not landing on '\n'.
pub(crate) fn move_right_normal(buf: &str, cpos: usize) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let eol = line_end(buf, cpos);
    let last_on_line = if eol > line_start(buf, cpos) {
        prev_char_boundary(buf, eol)
    } else {
        eol // Empty line — stay put.
    };
    if cpos >= last_on_line {
        return cpos;
    }
    next_char_boundary(buf, cpos)
}

/// Move right inclusive (for operator motions on `l`).
pub(crate) fn move_right_inclusive(buf: &str, cpos: usize) -> usize {
    next_char_boundary(buf, cpos).min(buf.len())
}

pub(crate) fn word_end_pos(buf: &str, cpos: usize, mode: CharClass) -> usize {
    let next = next_char_boundary(buf, cpos);
    if next >= buf.len() {
        return cpos;
    }
    let chars: Vec<(usize, char)> = buf[next..].char_indices().collect();
    if chars.is_empty() {
        return cpos;
    }
    let mut i = 0;
    while i < chars.len() && char_class(chars[i].1, mode) == 0 {
        i += 1;
    }
    if i >= chars.len() {
        return prev_char_boundary(buf, buf.len());
    }
    let target_class = char_class(chars[i].1, mode);
    while i + 1 < chars.len() && char_class(chars[i + 1].1, mode) == target_class {
        i += 1;
    }
    next + chars[i].0
}

/// End of line for normal mode (on last char, not past it).
pub(crate) fn line_end_normal(buf: &str, cpos: usize) -> usize {
    let end = line_end(buf, cpos);
    if end > line_start(buf, cpos) {
        prev_char_boundary(buf, end)
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
        let c = buf[pos..].chars().next().unwrap();
        if c != ' ' && c != '\t' {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

/// Full current line range including trailing newline (for dd).
pub(crate) fn current_line_range(buf: &str, cpos: usize) -> (usize, usize) {
    let start = line_start(buf, cpos);
    let end = line_end(buf, cpos);
    (start, end)
}

/// Content range of the current line, no newline (for S/cc).
pub(crate) fn current_line_content_range(buf: &str, cpos: usize) -> (usize, usize) {
    let start = line_start(buf, cpos);
    let end = line_end(buf, cpos);
    (start, end)
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
pub(crate) fn move_down_col(buf: &str, cpos: usize, want_col: Option<usize>) -> (usize, usize) {
    let sol = line_start(buf, cpos);
    let col = want_col.unwrap_or(cpos - sol);
    let eol = line_end(buf, cpos);
    if eol >= buf.len() {
        return (cpos, col);
    }
    let next_sol = eol + 1;
    let next_eol = line_end(buf, next_sol);
    let next_len = next_eol - next_sol;
    (next_sol + col.min(next_len), col)
}

pub(crate) fn move_up_col(buf: &str, cpos: usize, want_col: Option<usize>) -> (usize, usize) {
    let sol = line_start(buf, cpos);
    if sol == 0 {
        let col = want_col.unwrap_or(cpos - sol);
        return (cpos, col);
    }
    let col = want_col.unwrap_or(cpos - sol);
    let prev_eol = sol - 1;
    let prev_sol = line_start(buf, prev_eol);
    let prev_len = prev_eol - prev_sol;
    (prev_sol + col.min(prev_len), col)
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
            // end-of-line),  `next_char_boundary` advances past the newline
            // while `eol` stays put, which inverts the slice range. Clamp.
            let start = next_char_boundary(buf, cpos).min(eol);
            for (i, c) in buf[start..eol].char_indices() {
                if c == ch {
                    let pos = start + i;
                    return Some(match kind {
                        FindKind::ForwardTill => prev_char_boundary(buf, pos).max(cpos),
                        _ => pos,
                    });
                }
            }
            None
        }
        FindKind::Backward | FindKind::BackwardTill => {
            let search = &buf[sol..cpos];
            for (i, c) in search.char_indices().rev() {
                if c == ch {
                    let pos = sol + i;
                    return Some(match kind {
                        FindKind::BackwardTill => next_char_boundary(buf, pos).min(cpos),
                        _ => pos,
                    });
                }
            }
            None
        }
    }
}

/// Repeat a find-char motion `n` times; adjusts for till variants so `;`/`,` don't get stuck.
pub(crate) fn repeat_find(buf: &str, mut pos: usize, kind: FindKind, ch: char, n: usize) -> usize {
    for _ in 0..n {
        let search_pos = match kind {
            FindKind::ForwardTill => next_char_boundary(buf, pos),
            FindKind::BackwardTill => prev_char_boundary(buf, pos),
            _ => pos,
        };
        if let Some(p) = find_char(buf, search_pos, kind, ch) {
            pos = p;
        }
    }
    pos
}

// ── Match bracket ───────────────────────────────────────────────────────────

pub(crate) fn find_matching_bracket(buf: &str, cpos: usize) -> Option<usize> {
    let eol = line_end(buf, cpos);
    let mut start = cpos;
    while start < eol {
        let c = buf[start..].chars().next()?;
        if matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>') {
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
        '<' => ('<', '>', true),
        '>' => ('<', '>', false),
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
        p = next_char_boundary(buf, p);
    }
    p
}

pub(crate) fn retreat_chars(buf: &str, pos: usize, n: usize) -> usize {
    let mut p = pos;
    for _ in 0..n {
        if p == 0 {
            break;
        }
        p = prev_char_boundary(buf, p);
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
    if *cpos >= buf.len() {
        *cpos = if buf.ends_with('\n') {
            buf.len()
        } else {
            prev_char_boundary(buf, buf.len())
        };
        return;
    }
    // Cursor must not sit on an interior '\n'.
    if buf.as_bytes()[*cpos] == b'\n' && *cpos > 0 {
        let sol = line_start(buf, *cpos);
        if *cpos > sol {
            *cpos = prev_char_boundary(buf, *cpos);
        }
    }
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
        // Cursor on an empty line — the start/end are the same.
        assert_eq!(move_right_normal("\nabc", 0), 0);
    }

    #[test]
    fn move_right_inclusive_can_cross_newline_into_next_line() {
        // The operator-mode variant (used as the motion target for `l`/`dl`).
        // From end of "ab" (byte 2 = '\n'), it advances to byte 3 (start of "cd").
        assert_eq!(move_right_inclusive("ab\ncd", 1), 2);
        assert_eq!(move_right_inclusive("ab\ncd", 2), 3);
    }

    // ── line_end_normal / first_non_blank ─────────────────────────────────

    #[test]
    fn line_end_normal_lands_on_last_char_not_past_it() {
        assert_eq!(line_end_normal("abc", 0), 2);
        // Multi-line: "abc\ndef" — at byte 0, end of line 0 is byte 2.
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
        assert_eq!(first_non_blank("abc", 1), 0);
    }

    #[test]
    fn first_non_blank_returns_eol_when_line_is_all_whitespace() {
        // The function walks while pos < eol; if no non-blank found, returns eol.
        assert_eq!(first_non_blank("   ", 0), 3);
    }

    // ── current_line_range / current_line_content_range ───────────────────

    #[test]
    fn current_line_range_returns_content_range_for_first_line() {
        // "abc\ndef": line 0 spans bytes 0..3 (not including '\n' at byte 3).
        assert_eq!(current_line_range("abc\ndef", 0), (0, 3));
        // Cursor anywhere on the line — same answer.
        assert_eq!(current_line_range("abc\ndef", 2), (0, 3));
    }

    #[test]
    fn current_line_range_and_content_range_are_currently_identical() {
        // Sibling functions with different doc comments but identical bodies.
        // Pinning the current behaviour; documented intent ("with newline" /
        // "without newline") disagrees — see audit notes.
        let buf = "alpha\nbeta\ngamma";
        assert_eq!(
            current_line_range(buf, 0),
            current_line_content_range(buf, 0)
        );
        assert_eq!(
            current_line_range(buf, 7),
            current_line_content_range(buf, 7)
        );
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
        // "abcde\nxy": from col 4 ('e'), down lands at byte 8 — the position
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
        // "abc abc" — f c from byte 0 → byte 2.
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
        // 'z' is on line 2 — forward search on line 0 must not find it.
        assert_eq!(find_char("abc\ndefz", 0, FindKind::Forward, 'z'), None);
    }

    #[test]
    fn repeat_find_advances_past_previous_match_to_find_the_next() {
        // From byte 2 (a 'c'), `;` (Forward 'c') should land on byte 6 (next 'c').
        assert_eq!(repeat_find("abc abc", 2, FindKind::Forward, 'c', 1), 6);
    }

    // ── find_matching_bracket ─────────────────────────────────────────────

    #[test]
    fn find_matching_bracket_steps_from_open_to_close_on_same_line() {
        // "(abc)" — `%` at byte 0 should land on byte 4.
        assert_eq!(find_matching_bracket("(abc)", 0), Some(4));
        // From the close back to the open.
        assert_eq!(find_matching_bracket("(abc)", 4), Some(0));
    }

    #[test]
    fn find_matching_bracket_handles_nested_pairs() {
        // "((x))" — outer open at 0 matches close at 4.
        assert_eq!(find_matching_bracket("((x))", 0), Some(4));
        // Inner open at 1 matches close at 3.
        assert_eq!(find_matching_bracket("((x))", 1), Some(3));
    }

    #[test]
    fn find_matching_bracket_skips_forward_from_non_bracket_char_on_line() {
        // From the 'a' in "(abc)", we scan forward to the first bracket-like
        // char on this line — that's `)` at byte 4. Vim's `%` matches the
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

    // ── advance_chars / retreat_chars ─────────────────────────────────────

    #[test]
    fn advance_chars_walks_n_char_boundaries_clamping_at_end() {
        assert_eq!(advance_chars("abcd", 0, 2), 2);
        assert_eq!(advance_chars("abcd", 0, 10), 4); // clamped to len
        assert_eq!(advance_chars("a漢b", 0, 2), 4); // wide char counts as one step
    }

    #[test]
    fn retreat_chars_walks_n_char_boundaries_clamping_at_zero() {
        assert_eq!(retreat_chars("abcd", 4, 2), 2);
        assert_eq!(retreat_chars("abcd", 4, 99), 0);
        assert_eq!(retreat_chars("a漢b", 5, 2), 1); // back across a wide char
    }

    // ── clamp_normal ──────────────────────────────────────────────────────

    #[test]
    fn clamp_normal_pulls_cursor_back_from_past_end() {
        // No trailing newline — cursor at len becomes len-1 (boundary).
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
        // "abc\ndef" — cursor at byte 3 (the '\n') must shift back to byte 2.
        let mut cpos = 3;
        clamp_normal("abc\ndef", &mut cpos);
        assert_eq!(cpos, 2);
    }
}
