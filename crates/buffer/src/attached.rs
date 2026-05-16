//! Source text paired with attachment ids in lockstep.
//!
//! `AttachedText` enforces a single invariant:
//!
//! ```text
//! source.matches(ATTACHMENT_MARKER).count() == ids.len()
//! ```
//!
//! Every `\u{FFFC}` in `source` has a matching `AttachmentId` at the same
//! ordinal position in `ids`. All mutation methods preserve this — callers
//! cannot desync the two halves by accident, because there is no
//! `&mut String` exposed.

use crate::attachment::AttachmentId;
use crate::attachment::ATTACHMENT_MARKER;
use crate::text;
use core::ops::{Deref, Range};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachedText {
    source: String,
    ids: Vec<AttachmentId>,
}

impl AttachedText {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from raw parts. Panics in debug if marker count ≠ ids.len().
    pub fn from_parts(source: String, ids: Vec<AttachmentId>) -> Self {
        let out = Self { source, ids };
        out.check();
        out
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn ids(&self) -> &[AttachmentId] {
        &self.ids
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
        self.source = source;
        self.ids = ids;
        self.check();
    }

    /// Replace `source[range]` with `new`. Attachment ids whose markers
    /// survive into `new` (e.g. case-mapped markers, which case-fold to
    /// themselves) keep their ids; only markers actually removed get
    /// dropped. Endpoints are snapped.
    pub fn replace_range(&mut self, range: Range<usize>, new: &str) {
        let start = text::snap(&self.source, range.start);
        let end = text::snap(&self.source, range.end).max(start);
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
    /// `ATTACHMENT_MARKER` (debug-asserts; in release the marker would
    /// arrive without a paired id, breaking the invariant).
    /// Returns the snapped insertion offset.
    pub fn insert_str(&mut self, pos: usize, s: &str) -> usize {
        debug_assert!(
            !s.contains(ATTACHMENT_MARKER),
            "AttachedText::insert_str: input contains attachment marker; use insert_marker"
        );
        let p = text::snap(&self.source, pos);
        self.source.insert_str(p, s);
        p
    }

    /// Insert a single `char` at `pos`. The char must not be
    /// `ATTACHMENT_MARKER` (debug-asserts). Returns the snapped insertion
    /// offset.
    pub fn insert(&mut self, pos: usize, c: char) -> usize {
        debug_assert!(
            c != ATTACHMENT_MARKER,
            "AttachedText::insert: char is attachment marker; use insert_marker"
        );
        let p = text::snap(&self.source, pos);
        self.source.insert(p, c);
        p
    }

    /// Atomically write an attachment marker + register `id` at the matching
    /// ordinal position. Returns the snapped insertion offset of the marker.
    pub fn insert_marker(&mut self, pos: usize, id: AttachmentId) -> usize {
        let p = text::snap(&self.source, pos);
        let idx = self.source[..p].matches(ATTACHMENT_MARKER).count();
        self.ids.insert(idx, id);
        self.source.insert(p, ATTACHMENT_MARKER);
        self.check();
        p
    }

    /// Strip every `ATTACHMENT_MARKER` from `source` and drop every id.
    /// Returns the previous id list so callers that want to re-bind ids to
    /// a different host (e.g. when collapsing into a queued message) can.
    pub fn strip_attachments(&mut self) -> Vec<AttachmentId> {
        let drained = core::mem::take(&mut self.ids);
        self.source = self.source.replace(ATTACHMENT_MARKER, "");
        drained
    }

    fn check(&self) {
        debug_assert_eq!(
            self.source.matches(ATTACHMENT_MARKER).count(),
            self.ids.len(),
            "AttachedText: source marker count ≠ ids.len()"
        );
    }
}

impl Deref for AttachedText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.source
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
        let mut t = AttachedText::from_parts(format!("a{ATTACHMENT_MARKER}b"), vec![id(1)]);
        t.replace_range(0..t.len(), "xyz");
        assert_eq!(&*t, "xyz");
        assert_eq!(t.ids(), &[] as &[AttachmentId]);
    }

    #[test]
    fn replace_range_preserves_markers_that_survive_in_replacement() {
        let mut t = AttachedText::from_parts(format!("a{ATTACHMENT_MARKER}b"), vec![id(7)]);
        let upper = t.to_uppercase();
        t.replace_range(0..t.len(), &upper);
        assert_eq!(&*t, &format!("A{ATTACHMENT_MARKER}B"));
        assert_eq!(t.ids(), &[id(7)]);
    }

    #[test]
    fn insert_marker_registers_id_at_ordinal_index() {
        let mut t = AttachedText::from_parts(
            format!("{ATTACHMENT_MARKER}{ATTACHMENT_MARKER}"),
            vec![id(1), id(3)],
        );
        // Insert in the middle (between the two existing markers).
        t.insert_marker(ATTACHMENT_MARKER.len_utf8(), id(2));
        assert_eq!(t.ids(), &[id(1), id(2), id(3)]);
        assert_eq!(t.matches(ATTACHMENT_MARKER).count(), 3);
    }

    #[test]
    fn strip_attachments_clears_both_halves() {
        let mut t = AttachedText::from_parts(format!("a{ATTACHMENT_MARKER}b"), vec![id(1)]);
        let drained = t.strip_attachments();
        assert_eq!(drained, vec![id(1)]);
        assert_eq!(&*t, "ab");
        assert!(t.ids().is_empty());
    }
}
