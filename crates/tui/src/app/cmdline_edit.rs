//! Pure history-browse primitives for the `:` cmdline.
//!
//! Text editing itself lives in [`crate::line_input`], shared by command,
//! search, and dialog inputs.

/// One step of the cmdline history-browse state machine. Borrows the
/// matched entry directly from the caller's history slice.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HistoryStep<'a> {
    /// No history exists; the keystroke is a no-op.
    NoHistory,
    /// Already at the boundary in this direction; cursor doesn't move.
    Boundary,
    /// Move browse cursor to `idx` and replace payload with `entry`.
    /// `stash_current` is `true` when this is the FIRST Up - the caller
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
