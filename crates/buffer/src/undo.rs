//! Undo/redo history for the input buffer and vim motions.

use crate::attachment::AttachmentId;

#[derive(Clone)]
pub struct UndoEntry {
    pub buf: String,
    pub cpos: usize,
    pub attachments: Vec<AttachmentId>,
}

impl UndoEntry {
    pub fn snapshot(buf: &str, cpos: usize, attachments: &[AttachmentId]) -> Self {
        Self {
            buf: buf.to_string(),
            cpos,
            attachments: attachments.to_vec(),
        }
    }
}

pub struct UndoHistory {
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    cap: Option<usize>,
}

impl Default for UndoHistory {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Clone for UndoHistory {
    fn clone(&self) -> Self {
        Self {
            undo: self.undo.clone(),
            redo: self.redo.clone(),
            cap: self.cap,
        }
    }
}

impl UndoHistory {
    pub fn new(cap: Option<usize>) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            cap,
        }
    }

    /// Push a snapshot, clearing redo.
    pub fn save(&mut self, entry: UndoEntry) {
        self.redo.clear();
        self.undo.push(entry);
        if let Some(cap) = self.cap {
            while self.undo.len() > cap {
                self.undo.remove(0);
            }
        }
    }

    /// Pop the most recent snapshot onto redo, return it.
    pub fn undo(&mut self, current: UndoEntry) -> Option<UndoEntry> {
        let entry = self.undo.pop()?;
        self.redo.push(current);
        Some(entry)
    }

    /// Pop the most recent redo onto undo, return it.
    pub fn redo(&mut self, current: UndoEntry) -> Option<UndoEntry> {
        let entry = self.redo.pop()?;
        self.undo.push(current);
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(s: &str) -> UndoEntry {
        UndoEntry::snapshot(s, s.len(), &[])
    }

    #[test]
    fn empty_history_returns_none_for_undo_and_redo() {
        let mut h = UndoHistory::default();
        assert!(h.undo(entry("current")).is_none());
        assert!(h.redo(entry("current")).is_none());
    }

    #[test]
    fn undo_pops_most_recent_save_lifo() {
        let mut h = UndoHistory::default();
        h.save(entry("A"));
        h.save(entry("B"));
        let popped = h.undo(entry("C")).expect("undo yields B");
        assert_eq!(popped.buf, "B");
        let popped = h.undo(entry("B")).expect("undo yields A");
        assert_eq!(popped.buf, "A");
        assert!(h.undo(entry("A")).is_none(), "stack now empty");
    }

    #[test]
    fn undo_routes_current_to_redo_for_recovery() {
        let mut h = UndoHistory::default();
        h.save(entry("A"));
        let _ = h.undo(entry("CURRENT"));
        // After undo, redo should yield the previous current.
        let restored = h.redo(entry("A")).expect("redo yields CURRENT");
        assert_eq!(restored.buf, "CURRENT");
    }

    #[test]
    fn save_after_undo_clears_redo_stack() {
        let mut h = UndoHistory::default();
        h.save(entry("A"));
        let _ = h.undo(entry("B"));
        // Redo stack has [B]. A new save must discard it.
        h.save(entry("C"));
        assert!(h.redo(entry("X")).is_none());
    }

    #[test]
    fn cap_evicts_oldest_entry_when_exceeded() {
        let mut h = UndoHistory::new(Some(2));
        h.save(entry("A"));
        h.save(entry("B"));
        h.save(entry("C")); // A should be evicted, stack is now [B, C]
        assert_eq!(h.undo(entry("D")).unwrap().buf, "C");
        assert_eq!(h.undo(entry("C")).unwrap().buf, "B");
        assert!(h.undo(entry("B")).is_none(), "A was evicted");
    }

    #[test]
    fn default_history_is_unbounded() {
        let mut h = UndoHistory::default();
        for i in 0..100 {
            h.save(entry(&format!("v{i}")));
        }
        // All 100 entries must be present; undo 100 times yields a result each.
        for _ in 0..100 {
            assert!(h.undo(entry("x")).is_some());
        }
        assert!(h.undo(entry("x")).is_none());
    }

    #[test]
    fn entry_snapshot_captures_all_three_fields() {
        let atts = vec![42, 99];
        let e = UndoEntry::snapshot("hello", 3, &atts);
        assert_eq!(e.buf, "hello");
        assert_eq!(e.cpos, 3);
        assert_eq!(e.attachments, atts);
    }

    #[test]
    fn clone_is_independent_of_original() {
        let mut h = UndoHistory::default();
        h.save(entry("A"));
        let mut h2 = h.clone();
        h2.save(entry("B")); // mutate the clone
        assert_eq!(h.undo(entry("x")).unwrap().buf, "A");
        // Original has nothing left now; clone still has [A, B].
        assert_eq!(h2.undo(entry("x")).unwrap().buf, "B");
        assert_eq!(h2.undo(entry("x")).unwrap().buf, "A");
    }
}
