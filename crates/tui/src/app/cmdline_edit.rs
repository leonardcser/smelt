//! Pure text-editing primitives for the `:` cmdline payload.
//!
//! Each function operates on `(payload, cursor_in_chars)` and returns the
//! new state without touching `TuiApp`, the UI tree, or history state. The
//! `&mut self` methods in [`cmdline.rs`](super::cmdline) read state, call
//! into here, then write the result back.
//!
//! Cursor positions are *character* indices (not byte indices); the payload
//! is the part after the leading `:` prefix.

/// Backspace from `cur` chars into `payload`. Returns the new payload and
/// cursor. The caller closes the cmdline when this returns `None` (i.e.
/// when the payload was already empty before the keystroke).
pub(crate) fn backspace(payload: &str, cur: usize) -> Option<(String, usize)> {
    if payload.is_empty() {
        return None;
    }
    if cur == 0 {
        return Some((payload.to_string(), 0));
    }
    let chars: Vec<char> = payload.chars().collect();
    let new: String = chars[..cur - 1]
        .iter()
        .copied()
        .chain(chars[cur..].iter().copied())
        .collect();
    Some((new, cur - 1))
}

/// Insert `c` at `cur`. Cursor is clamped to the payload length so callers
/// can pass an unverified cursor without checking.
pub(crate) fn insert_char(payload: &str, cur: usize, c: char) -> (String, usize) {
    let chars: Vec<char> = payload.chars().collect();
    let cur = cur.min(chars.len());
    let new: String = chars[..cur]
        .iter()
        .copied()
        .chain(std::iter::once(c))
        .chain(chars[cur..].iter().copied())
        .collect();
    (new, cur + 1)
}

/// Forward-delete one char at `cur`. No-op when the cursor is at end-of-line.
pub(crate) fn delete_forward(payload: &str, cur: usize) -> (String, usize) {
    let chars: Vec<char> = payload.chars().collect();
    if cur >= chars.len() {
        return (payload.to_string(), cur);
    }
    let new: String = chars[..cur]
        .iter()
        .copied()
        .chain(chars[cur + 1..].iter().copied())
        .collect();
    (new, cur)
}

/// `Ctrl-W` / `M-Backspace` — delete one word backward. A "word" stops at
/// any non-alphanumeric, non-underscore character. Returns `None` only
/// when the payload was already empty (caller closes the cmdline).
pub(crate) fn delete_word_back(payload: &str, cur: usize) -> Option<(String, usize)> {
    if payload.is_empty() {
        return None;
    }
    let chars: Vec<char> = payload.chars().collect();
    let split = cur.min(chars.len());
    let prefix: String = chars[..split].iter().collect();
    let trimmed_end = prefix.trim_end();
    let new_cursor = match trimmed_end.rfind(|c: char| !c.is_alphanumeric() && c != '_') {
        Some(boundary) => {
            // The boundary char itself is kept; cursor lands after it.
            let boundary_char_len = trimmed_end[boundary..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            trimmed_end[..boundary + boundary_char_len].chars().count()
        }
        None => 0,
    };
    let head: String = chars[..new_cursor].iter().collect();
    let tail: String = chars[split..].iter().collect();
    Some((format!("{head}{tail}"), new_cursor))
}

/// Clamp a relative cursor move into `[0, count]`. `delta` is signed —
/// negative values move left.
pub(crate) fn clamp_move(count: usize, cur: usize, delta: i32) -> usize {
    let count = count as i32;
    let cur = cur as i32;
    (cur + delta).clamp(0, count) as usize
}

/// One step of the cmdline history-browse state machine. Borrows the
/// matched entry directly from the caller's history slice.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HistoryStep<'a> {
    /// No history exists; the keystroke is a no-op.
    NoHistory,
    /// Already at the boundary in this direction; cursor doesn't move.
    Boundary,
    /// Move browse cursor to `idx` and replace payload with `entry`.
    /// `stash_current` is `true` when this is the FIRST Up — the caller
    /// must save the current payload so Down past the newest entry
    /// can restore it.
    Browse {
        idx: usize,
        entry: &'a str,
        stash_current: bool,
    },
    /// Down past the newest entry: restore the stashed payload and exit
    /// browse mode.
    Restore { stash: String },
}

/// Owned counterpart of [`HistoryStep`] used by `&mut self` callers that
/// need to drop the immutable history borrow before mutating other state.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HistoryStepOwned {
    NoHistory,
    Boundary,
    Browse {
        idx: usize,
        entry: String,
        stash_current: bool,
    },
    Restore {
        stash: String,
    },
}

impl HistoryStep<'_> {
    pub(crate) fn into_owned(self) -> HistoryStepOwned {
        match self {
            HistoryStep::NoHistory => HistoryStepOwned::NoHistory,
            HistoryStep::Boundary => HistoryStepOwned::Boundary,
            HistoryStep::Browse {
                idx,
                entry,
                stash_current,
            } => HistoryStepOwned::Browse {
                idx,
                entry: entry.to_string(),
                stash_current,
            },
            HistoryStep::Restore { stash } => HistoryStepOwned::Restore { stash },
        }
    }
}

/// History navigation: Up. Always moves toward older entries; saturates at
/// index 0. Returns the next browse index + the entry to display.
pub(crate) fn history_up<'a>(history: &'a [String], browse: Option<usize>) -> HistoryStep<'a> {
    if history.is_empty() {
        return HistoryStep::NoHistory;
    }
    let stash_current = browse.is_none();
    let next_idx = match browse {
        None => history.len() - 1,
        Some(0) => 0,
        Some(i) => i - 1,
    };
    HistoryStep::Browse {
        idx: next_idx,
        entry: history[next_idx].as_str(),
        stash_current,
    }
}

/// History navigation: Down. Moves toward newer entries; one step past the
/// newest entry restores the stashed payload and exits browse mode.
pub(crate) fn history_down<'a>(
    history: &'a [String],
    browse: Option<usize>,
    stash: &str,
) -> HistoryStep<'a> {
    let Some(idx) = browse else {
        return HistoryStep::Boundary;
    };
    if idx + 1 >= history.len() {
        return HistoryStep::Restore {
            stash: stash.to_string(),
        };
    }
    let next_idx = idx + 1;
    HistoryStep::Browse {
        idx: next_idx,
        entry: history[next_idx].as_str(),
        stash_current: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── insert_char ──────────────────────────────────────────────────────

    #[test]
    fn insert_char_at_cursor_position() {
        let (out, cur) = insert_char("ello", 0, 'h');
        assert_eq!(out, "hello");
        assert_eq!(cur, 1);
    }

    #[test]
    fn insert_char_appends_at_end_of_line() {
        let (out, cur) = insert_char("hi", 2, '!');
        assert_eq!(out, "hi!");
        assert_eq!(cur, 3);
    }

    #[test]
    fn insert_char_clamps_overflow_cursor_to_end() {
        // Callers sometimes pass an unvalidated cursor; insertion clamps it.
        let (out, cur) = insert_char("ab", 99, 'c');
        assert_eq!(out, "abc");
        assert_eq!(cur, 3);
    }

    #[test]
    fn insert_char_counts_unicode_chars_not_bytes() {
        let (out, cur) = insert_char("日本", 1, '!');
        assert_eq!(out, "日!本");
        assert_eq!(cur, 2);
    }

    // ── backspace ────────────────────────────────────────────────────────

    #[test]
    fn backspace_on_empty_payload_signals_close() {
        // The dispatcher closes the cmdline on this signal.
        assert_eq!(backspace("", 0), None);
    }

    #[test]
    fn backspace_at_position_zero_is_a_no_op() {
        let (out, cur) = backspace("hello", 0).unwrap();
        assert_eq!(out, "hello");
        assert_eq!(cur, 0);
    }

    #[test]
    fn backspace_removes_one_char_before_cursor() {
        let (out, cur) = backspace("hello", 3).unwrap();
        assert_eq!(out, "helo");
        assert_eq!(cur, 2);
    }

    #[test]
    fn backspace_works_with_multi_byte_unicode() {
        let (out, cur) = backspace("日本語", 2).unwrap();
        assert_eq!(out, "日語");
        assert_eq!(cur, 1);
    }

    // ── delete_forward ───────────────────────────────────────────────────

    #[test]
    fn delete_forward_at_end_of_line_is_a_no_op() {
        let (out, cur) = delete_forward("abc", 3);
        assert_eq!(out, "abc");
        assert_eq!(cur, 3);
    }

    #[test]
    fn delete_forward_removes_char_at_cursor_and_keeps_cursor_position() {
        let (out, cur) = delete_forward("abcd", 1);
        assert_eq!(out, "acd");
        assert_eq!(cur, 1);
    }

    // ── delete_word_back ─────────────────────────────────────────────────

    #[test]
    fn delete_word_back_on_empty_payload_signals_close() {
        assert_eq!(delete_word_back("", 0), None);
    }

    #[test]
    fn delete_word_back_strips_trailing_word() {
        let (out, cur) = delete_word_back("hello world", 11).unwrap();
        // The trailing word "world" goes; the space stays.
        assert_eq!(out, "hello ");
        assert_eq!(cur, 6);
    }

    #[test]
    fn delete_word_back_with_only_word_chars_deletes_to_start() {
        let (out, cur) = delete_word_back("oneword", 7).unwrap();
        assert_eq!(out, "");
        assert_eq!(cur, 0);
    }

    #[test]
    fn delete_word_back_keeps_punctuation_boundary_char() {
        // Boundary char (the slash) is kept; only the trailing word goes.
        let (out, cur) = delete_word_back("/cmd arg", 8).unwrap();
        assert_eq!(out, "/cmd ");
        assert_eq!(cur, 5);
    }

    #[test]
    fn delete_word_back_underscore_counts_as_word_char() {
        // `foo_bar` is one word — Ctrl-W deletes the whole thing.
        let (out, cur) = delete_word_back("foo_bar", 7).unwrap();
        assert_eq!(out, "");
        assert_eq!(cur, 0);
    }

    #[test]
    fn delete_word_back_preserves_tail_after_cursor() {
        let (out, cur) = delete_word_back("hello world end", 11).unwrap();
        assert_eq!(out, "hello  end");
        assert_eq!(cur, 6);
    }

    // ── clamp_move ───────────────────────────────────────────────────────

    #[test]
    fn clamp_move_clamps_below_zero_to_start() {
        assert_eq!(clamp_move(5, 0, -3), 0);
        assert_eq!(clamp_move(5, 2, -10), 0);
    }

    #[test]
    fn clamp_move_clamps_overflow_to_end() {
        // Cursor can sit at `count` (one past last char) just like at index 0.
        assert_eq!(clamp_move(5, 4, 10), 5);
    }

    #[test]
    fn clamp_move_passes_through_in_range_moves() {
        assert_eq!(clamp_move(10, 3, 2), 5);
        assert_eq!(clamp_move(10, 5, -2), 3);
    }

    // ── history_up / history_down ────────────────────────────────────────

    fn hist(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn history_up_on_empty_history_is_a_noop() {
        assert_eq!(history_up(&[], None), HistoryStep::NoHistory);
    }

    #[test]
    fn history_up_from_fresh_state_browses_newest_and_stashes_current() {
        let h = hist(&["old", "newer", "newest"]);
        assert_eq!(
            history_up(&h, None),
            HistoryStep::Browse {
                idx: 2,
                entry: "newest",
                stash_current: true,
            }
        );
    }

    #[test]
    fn history_up_after_browsing_moves_one_step_older() {
        let h = hist(&["old", "newer", "newest"]);
        assert_eq!(
            history_up(&h, Some(2)),
            HistoryStep::Browse {
                idx: 1,
                entry: "newer",
                stash_current: false,
            }
        );
    }

    #[test]
    fn history_up_at_oldest_entry_saturates_in_place() {
        let h = hist(&["a", "b"]);
        assert_eq!(
            history_up(&h, Some(0)),
            HistoryStep::Browse {
                idx: 0,
                entry: "a",
                stash_current: false,
            }
        );
    }

    #[test]
    fn history_down_without_an_active_browse_is_a_noop() {
        let h = hist(&["a", "b"]);
        assert_eq!(history_down(&h, None, "stash"), HistoryStep::Boundary);
    }

    #[test]
    fn history_down_within_history_advances_to_newer_entry() {
        let h = hist(&["a", "b", "c"]);
        assert_eq!(
            history_down(&h, Some(0), "stash"),
            HistoryStep::Browse {
                idx: 1,
                entry: "b",
                stash_current: false,
            }
        );
    }

    #[test]
    fn history_down_past_newest_restores_stash_and_exits_browse() {
        let h = hist(&["a", "b"]);
        assert_eq!(
            history_down(&h, Some(1), "my draft"),
            HistoryStep::Restore {
                stash: "my draft".to_string(),
            }
        );
    }
}
