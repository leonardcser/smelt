//! Source text paired with attachment ids in lockstep.
//!
//! `AttachedTextMut` is a thin borrowing wrapper around
//! `(&mut String, &mut Vec<AttachmentId>)` that enforces a single
//! invariant on every mutation:
//!
//! ```text
//! source.matches(ATTACHMENT_MARKER).count() == ids.len()
//! ```
//!
//! Every `\u{FFFC}` in `source` has a matching `AttachmentId` at the same
//! ordinal position in `ids`. Callers cannot desync the two halves by
//! accident because there is no `&mut String` or `&mut Vec<_>` reachable
//! from the wrapper - only safe mutation methods.

use crate::attachment::AttachmentId;
use crate::attachment::ATTACHMENT_MARKER;
use crate::text;
use core::ops::{Deref, Range};

pub struct AttachedTextMut<'a> {
    source: &'a mut String,
    ids: &'a mut Vec<AttachmentId>,
}

impl<'a> AttachedTextMut<'a> {
    /// Build a borrowing wrapper. Debug-asserts the paired invariant.
    pub fn new(source: &'a mut String, ids: &'a mut Vec<AttachmentId>) -> Self {
        let out = Self { source, ids };
        out.check();
        out
    }

    pub fn as_str(&self) -> &str {
        self.source
    }

    pub fn ids(&self) -> &[AttachmentId] {
        self.ids
    }

    pub fn len(&self) -> usize {
        self.source.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    pub fn is_empty_overall(&self) -> bool {
        self.source.is_empty() && self.ids.is_empty()
    }

    pub fn clear(&mut self) {
        self.source.clear();
        self.ids.clear();
    }

    /// Wholesale swap. The caller asserts `source` and `ids` are paired.
    pub fn install(&mut self, source: String, ids: Vec<AttachmentId>) {
        *self.source = source;
        *self.ids = ids;
        self.check();
    }

    /// Replace only the id list. Used by undo-restore paths where the
    /// source has already been swapped through a separate code path (e.g.
    /// `install_source`) and the snapshot's id list needs to be re-bound.
    /// Debug-asserts the marker count matches.
    pub fn set_ids(&mut self, ids: Vec<AttachmentId>) {
        *self.ids = ids;
        self.check();
    }

    /// Replace `source[range]` with `new`. Attachment ids whose markers
    /// survive into `new` (e.g. case-mapped markers, which case-fold to
    /// themselves) keep their ids; only markers actually removed get
    /// dropped. Endpoints are snapped.
    pub fn replace_range(&mut self, range: Range<usize>, new: &str) {
        let start = text::snap(self.source, range.start);
        let end = text::snap(self.source, range.end).max(start);
        let before = self.source[..start].matches(ATTACHMENT_MARKER).count();
        let removed = self.source[start..end].matches(ATTACHMENT_MARKER).count();
        let kept = new.matches(ATTACHMENT_MARKER).count();
        let drop = removed.saturating_sub(kept);
        self.source.replace_range(start..end, new);
        let drain_end = (before + drop).min(self.ids.len());
        self.ids.drain(before..drain_end);
        self.check();
    }

    /// Insert a plain string at `pos`. `s` must not contain
    /// `ATTACHMENT_MARKER` (debug-asserts). Returns the snapped offset.
    pub fn insert_str(&mut self, pos: usize, s: &str) -> usize {
        debug_assert!(
            !s.contains(ATTACHMENT_MARKER),
            "AttachedTextMut::insert_str: input contains attachment marker; use insert_marker"
        );
        let p = text::snap(self.source, pos);
        self.source.insert_str(p, s);
        p
    }

    /// Insert a single `char` at `pos`. The char must not be
    /// `ATTACHMENT_MARKER` (debug-asserts). Returns the snapped offset.
    pub fn insert(&mut self, pos: usize, c: char) -> usize {
        debug_assert!(
            c != ATTACHMENT_MARKER,
            "AttachedTextMut::insert: char is attachment marker; use insert_marker"
        );
        let p = text::snap(self.source, pos);
        self.source.insert(p, c);
        p
    }

    /// Atomically write an attachment marker + register `id` at the matching
    /// ordinal position. Returns the snapped offset of the marker.
    pub fn insert_marker(&mut self, pos: usize, id: AttachmentId) -> usize {
        let p = text::snap(self.source, pos);
        let idx = self.source[..p].matches(ATTACHMENT_MARKER).count();
        self.ids.insert(idx, id);
        self.source.insert(p, ATTACHMENT_MARKER);
        self.check();
        p
    }

    /// Strip every `ATTACHMENT_MARKER` from `source` and drop every id.
    /// Returns the previous id list so callers can re-bind ids elsewhere.
    pub fn strip_attachments(&mut self) -> Vec<AttachmentId> {
        let drained = core::mem::take(self.ids);
        *self.source = self.source.replace(ATTACHMENT_MARKER, "");
        drained
    }

    fn check(&self) {
        debug_assert_eq!(
            self.source.matches(ATTACHMENT_MARKER).count(),
            self.ids.len(),
            "AttachedTextMut: source marker count ≠ ids.len()"
        );
    }
}

impl Deref for AttachedTextMut<'_> {
    type Target = str;
    fn deref(&self) -> &str {
        self.source
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> AttachmentId {
        n
    }

    #[test]
    fn replace_range_drops_markers_removed_from_source() {
        let mut s = format!("a{ATTACHMENT_MARKER}b");
        let mut ids = vec![id(1)];
        let mut t = AttachedTextMut::new(&mut s, &mut ids);
        let n = t.len();
        t.replace_range(0..n, "xyz");
        assert_eq!(s, "xyz");
        assert!(ids.is_empty());
    }

    #[test]
    fn replace_range_preserves_markers_that_survive_in_replacement() {
        let mut s = format!("a{ATTACHMENT_MARKER}b");
        let mut ids = vec![id(7)];
        let mut t = AttachedTextMut::new(&mut s, &mut ids);
        let upper = t.to_uppercase();
        let n = t.len();
        t.replace_range(0..n, &upper);
        assert_eq!(s, format!("A{ATTACHMENT_MARKER}B"));
        assert_eq!(ids, vec![id(7)]);
    }

    #[test]
    fn insert_marker_registers_id_at_ordinal_index() {
        let mut s = format!("{ATTACHMENT_MARKER}{ATTACHMENT_MARKER}");
        let mut ids = vec![id(1), id(3)];
        let mut t = AttachedTextMut::new(&mut s, &mut ids);
        t.insert_marker(ATTACHMENT_MARKER.len_utf8(), id(2));
        assert_eq!(ids, vec![id(1), id(2), id(3)]);
        assert_eq!(s.matches(ATTACHMENT_MARKER).count(), 3);
    }

    #[test]
    fn strip_attachments_clears_both_halves() {
        let mut s = format!("a{ATTACHMENT_MARKER}b");
        let mut ids = vec![id(1)];
        let mut t = AttachedTextMut::new(&mut s, &mut ids);
        let drained = t.strip_attachments();
        assert_eq!(drained, vec![id(1)]);
        assert_eq!(s, "ab");
        assert!(ids.is_empty());
    }
}
