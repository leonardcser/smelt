//! Vim's jump list: the positions a window left through jumps such as `G`,
//! `}` or `]u`, walked with `Ctrl-O` and `Ctrl-I`.

use crate::row::DocPosition;

/// Positions kept per window, like vim's `jumplist`.
const CAPACITY: usize = 100;

/// A move through the jump list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JumpMove {
    /// `Ctrl-O`: back to where the cursor was `n` jumps ago.
    Older(usize),
    /// `Ctrl-I`: forward again after going back.
    Newer(usize),
    /// `''`: the position before the latest jump; repeating it toggles.
    Previous,
}

/// Rows are the identity of an entry: a later jump from the same row replaces
/// the earlier one.
#[derive(Clone, Debug, Default)]
pub struct JumpList {
    entries: Vec<DocPosition>,
    /// The entry the cursor stands on after moving through the list, or
    /// `entries.len()` while it is at the newest end.
    index: usize,
}

impl JumpList {
    /// Remember `from` as the origin of a jump about to be made.
    pub fn record(&mut self, from: DocPosition) {
        self.entries.retain(|entry| entry.row != from.row);
        if self.entries.len() == CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(from);
        self.index = self.entries.len();
    }

    /// Where `jump` takes a cursor standing on `from`, or `None` when the list
    /// has nothing that far back or forward.
    pub fn resolve(&mut self, from: DocPosition, jump: JumpMove) -> Option<DocPosition> {
        match jump {
            JumpMove::Older(count) => self.step(from, -(count.max(1) as isize)),
            JumpMove::Newer(count) => self.step(from, count.max(1) as isize),
            JumpMove::Previous => {
                let target = *self.entries.last().filter(|entry| entry.row != from.row)?;
                self.record(from);
                Some(target)
            }
        }
    }

    fn step(&mut self, from: DocPosition, delta: isize) -> Option<DocPosition> {
        let mut index = self.index;
        if index == self.entries.len() {
            if delta > 0 {
                return None;
            }
            // Going back from the newest end keeps `from` so `Ctrl-I` returns to it.
            self.record(from);
            index = self.entries.len() - 1;
        }
        let target = index.checked_add_signed(delta)?;
        let position = *self.entries.get(target)?;
        self.index = target;
        Some(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(row: u32) -> DocPosition {
        DocPosition {
            row: row.into(),
            byte_col: 0,
        }
    }

    fn list(rows: &[u32]) -> JumpList {
        let mut list = JumpList::default();
        for &row in rows {
            list.record(at(row));
        }
        list
    }

    #[test]
    fn older_walks_back_and_newer_returns() {
        let mut jumps = list(&[1, 10]);
        assert_eq!(jumps.resolve(at(50), JumpMove::Older(1)), Some(at(10)));
        assert_eq!(jumps.resolve(at(10), JumpMove::Older(1)), Some(at(1)));
        assert_eq!(jumps.resolve(at(1), JumpMove::Older(1)), None);
        assert_eq!(jumps.resolve(at(1), JumpMove::Newer(1)), Some(at(10)));
        assert_eq!(jumps.resolve(at(10), JumpMove::Newer(1)), Some(at(50)));
        assert_eq!(jumps.resolve(at(50), JumpMove::Newer(1)), None);
    }

    #[test]
    fn counts_skip_entries() {
        let mut jumps = list(&[1, 10, 20]);
        assert_eq!(jumps.resolve(at(50), JumpMove::Older(3)), Some(at(1)));
        assert_eq!(jumps.resolve(at(1), JumpMove::Newer(2)), Some(at(20)));
    }

    #[test]
    fn too_large_a_count_stays_put() {
        let mut jumps = list(&[1]);
        assert_eq!(jumps.resolve(at(50), JumpMove::Older(5)), None);
        assert_eq!(jumps.resolve(at(50), JumpMove::Older(1)), Some(at(1)));
    }

    #[test]
    fn nothing_to_go_forward_to_at_the_newest_end() {
        let mut jumps = list(&[1]);
        assert_eq!(jumps.resolve(at(5), JumpMove::Newer(1)), None);
    }

    #[test]
    fn a_new_jump_from_the_same_row_replaces_the_old_entry() {
        let mut jumps = list(&[1, 10, 1]);
        assert_eq!(jumps.entries, vec![at(10), at(1)]);
        assert_eq!(jumps.resolve(at(50), JumpMove::Older(2)), Some(at(10)));
    }

    #[test]
    fn jumping_after_going_back_starts_from_the_newest_end() {
        let mut jumps = list(&[1, 10]);
        jumps.resolve(at(50), JumpMove::Older(2));
        jumps.record(at(1));
        assert_eq!(jumps.resolve(at(70), JumpMove::Older(1)), Some(at(1)));
    }

    #[test]
    fn previous_toggles_between_two_rows() {
        let mut jumps = list(&[1]);
        assert_eq!(jumps.resolve(at(50), JumpMove::Previous), Some(at(1)));
        assert_eq!(jumps.resolve(at(1), JumpMove::Previous), Some(at(50)));
        assert_eq!(jumps.resolve(at(50), JumpMove::Previous), Some(at(1)));
    }

    #[test]
    fn previous_without_a_distinct_origin_does_nothing() {
        assert_eq!(JumpList::default().resolve(at(1), JumpMove::Previous), None);
        assert_eq!(list(&[7]).resolve(at(7), JumpMove::Previous), None);
    }

    #[test]
    fn the_oldest_entries_fall_off_past_the_capacity() {
        let rows: Vec<u32> = (0..CAPACITY as u32 + 5).collect();
        let jumps = list(&rows);
        assert_eq!(jumps.entries.len(), CAPACITY);
        assert_eq!(jumps.entries[0], at(5));
    }
}
