//! Source text paired with its attachments in lockstep.
//!
//! `AttachedTextMut` is a thin borrowing wrapper around
//! `(&mut String, &mut Vec<Attachment>)` that enforces a single
//! invariant on every mutation:
//!
//! ```text
//! source.matches(ATTACHMENT_MARKER).count() == attachments.len()
//! ```
//!
//! Every `\u{FFFC}` in `source` has a matching [`Attachment`] at the same
//! ordinal position. Callers cannot desync the two halves by accident
//! because there is no `&mut String` or `&mut Vec<_>` reachable from the
//! wrapper - only safe mutation methods. [`AttachedText`] is the owned form
//! the kill ring and registers carry.

use crate::attachment::Attachment;
use crate::attachment::ATTACHMENT_MARKER;
use crate::text;
use core::ops::{Deref, Range};
use std::borrow::Cow;

/// Owned text with its attachments, as cut, copied and pasted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttachedText {
    text: String,
    attachments: Vec<Attachment>,
}

impl AttachedText {
    /// Pair `text` with `attachments`, one per marker. Debug-asserts the pairing.
    pub fn new(text: String, attachments: Vec<Attachment>) -> Self {
        debug_assert_eq!(
            text.matches(ATTACHMENT_MARKER).count(),
            attachments.len(),
            "AttachedText: marker count ≠ attachments.len()"
        );
        Self { text, attachments }
    }

    /// `source[range]` with the attachments its markers stand for, where
    /// `attachments` pairs with every marker in `source`.
    pub fn slice_of(source: &str, attachments: &[Attachment], range: Range<usize>) -> Self {
        let start = text::snap(source, range.start);
        let end = text::snap(source, range.end).max(start);
        let text = source[start..end].to_string();
        let first = source[..start].matches(ATTACHMENT_MARKER).count();
        let count = text.matches(ATTACHMENT_MARKER).count();
        match attachments.get(first..first + count) {
            Some(attachments) => Self::new(text, attachments.to_vec()),
            None => {
                debug_assert!(false, "AttachedText::slice_of: markers without attachments");
                Self::plain(text)
            }
        }
    }

    /// Text without attachments. Stray markers are dropped.
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            text: strip_markers(text),
            attachments: Vec::new(),
        }
    }

    /// The text, with one `ATTACHMENT_MARKER` per attachment.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    /// The text and its attachments, still paired.
    pub fn into_parts(self) -> (String, Vec<Attachment>) {
        (self.text, self.attachments)
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The text with the attachment markers removed.
    pub fn plain_text(&self) -> Cow<'_, str> {
        if self.attachments.is_empty() {
            Cow::Borrowed(&self.text)
        } else {
            Cow::Owned(self.text.replace(ATTACHMENT_MARKER, ""))
        }
    }

    /// The text with each attachment shown as its `[label]`.
    pub fn labelled(&self) -> Cow<'_, str> {
        if self.attachments.is_empty() {
            return Cow::Borrowed(&self.text);
        }
        let mut out = String::with_capacity(self.text.len());
        let mut attachments = self.attachments.iter();
        for ch in self.text.chars() {
            if ch != ATTACHMENT_MARKER {
                out.push(ch);
            } else if let Some(attachment) = attachments.next() {
                out.push_str(&attachment.display_label());
            }
        }
        Cow::Owned(out)
    }

    /// The inverse of [`labelled`](Self::labelled). Each of `attachments`
    /// takes the first free `[label]` of its own, wherever it is, and that
    /// label becomes its marker, together with any grapheme the label shares.
    /// Attachments with no label left come back unbound. Stray markers in
    /// `labelled` are dropped.
    pub fn from_labelled(labelled: &str, attachments: &[Attachment]) -> (Self, Vec<Attachment>) {
        let labelled = strip_markers(labelled.to_string());
        let mut claimed: Vec<(Range<usize>, &Attachment)> = Vec::new();
        let mut unbound = Vec::new();
        for attachment in attachments {
            let display = attachment.display_label();
            let raw = format!("[{}]", attachment.label());
            let free = [display.as_str(), raw.as_str()]
                .into_iter()
                .find_map(|label| {
                    labelled.match_indices(label).find_map(|(at, _)| {
                        let range = text::covering_grapheme_range(&labelled, at..at + label.len());
                        let overlaps = claimed
                            .iter()
                            .any(|(taken, _)| range.start < taken.end && taken.start < range.end);
                        (!overlaps).then_some(range)
                    })
                });
            match free {
                Some(range) => claimed.push((range, attachment)),
                None => unbound.push(attachment.clone()),
            }
        }
        claimed.sort_by_key(|(range, _)| range.start);

        let mut text = String::with_capacity(labelled.len());
        let mut end = 0;
        for (range, _) in &claimed {
            text.push_str(text::slice(&labelled, end..range.start));
            text.push(ATTACHMENT_MARKER);
            end = range.end;
        }
        text.push_str(text::slice(&labelled, end..labelled.len()));
        let bound = claimed.into_iter().map(|(_, a)| a.clone()).collect();
        (Self::new(text, bound), unbound)
    }

    pub fn push_str(&mut self, s: &str) {
        self.text.push_str(&strip_markers(s.to_string()));
    }

    pub fn push(&mut self, other: &AttachedText) {
        self.text.push_str(&other.text);
        self.attachments.extend_from_slice(&other.attachments);
    }

    /// `self` joined `count` times with `separator` between copies.
    pub fn repeat(&self, count: usize, separator: &str) -> Self {
        let mut out = Self::default();
        for i in 0..count {
            if i > 0 {
                out.push_str(separator);
            }
            out.push(self);
        }
        out
    }
}

fn strip_markers(text: String) -> String {
    if text.contains(ATTACHMENT_MARKER) {
        text.replace(ATTACHMENT_MARKER, "")
    } else {
        text
    }
}

#[derive(Clone, Default)]
pub(crate) struct SourceChanges {
    pub revision: u64,
    pub changed_from: Option<usize>,
}

impl SourceChanges {
    pub fn record(&mut self, start: usize) {
        self.revision = self.revision.wrapping_add(1);
        self.changed_from = Some(
            self.changed_from
                .map_or(start, |previous| previous.min(start)),
        );
    }
}

pub struct AttachedTextMut<'a> {
    source: &'a mut String,
    attachments: &'a mut Vec<Attachment>,
    changes: Option<&'a mut SourceChanges>,
    /// When false, pasted attachments arrive as plain text.
    holds_attachments: bool,
}

impl<'a> AttachedTextMut<'a> {
    /// Build a borrowing wrapper. Debug-asserts the paired invariant.
    pub fn new(source: &'a mut String, attachments: &'a mut Vec<Attachment>) -> Self {
        let out = Self {
            source,
            attachments,
            changes: None,
            holds_attachments: true,
        };
        out.check();
        out
    }

    pub(crate) fn tracked(
        source: &'a mut String,
        attachments: &'a mut Vec<Attachment>,
        changes: &'a mut SourceChanges,
        holds_attachments: bool,
    ) -> Self {
        let mut out = Self::new(source, attachments);
        out.changes = Some(changes);
        out.holds_attachments = holds_attachments;
        out
    }

    fn record_change(&mut self, start: usize) {
        if let Some(changes) = self.changes.as_mut() {
            changes.record(start);
        }
    }

    pub fn as_str(&self) -> &str {
        self.source
    }

    pub fn attachments(&self) -> &[Attachment] {
        self.attachments
    }

    /// `source[range]` with the attachments its markers stand for.
    pub fn slice(&self, range: Range<usize>) -> AttachedText {
        AttachedText::slice_of(self.source, self.attachments, range)
    }

    pub fn len(&self) -> usize {
        self.source.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    pub fn is_empty_overall(&self) -> bool {
        self.source.is_empty() && self.attachments.is_empty()
    }

    pub fn clear(&mut self) {
        if self.is_empty_overall() {
            return;
        }
        self.record_change(0);
        self.source.clear();
        self.attachments.clear();
    }

    /// Wholesale swap. The caller asserts `source` and `ids` are paired.
    pub fn install(&mut self, source: String, attachments: Vec<Attachment>) {
        if *self.source == source && *self.attachments == attachments {
            return;
        }
        self.record_change(0);
        *self.source = source;
        *self.attachments = attachments;
        self.check();
    }

    /// Replace only the attachment list. Used by undo-restore paths where the
    /// source has already been swapped through a separate code path (e.g.
    /// `install_source`) and the snapshot's attachments need to be re-bound.
    /// Debug-asserts the marker count matches.
    pub fn set_attachments(&mut self, attachments: Vec<Attachment>) {
        if *self.attachments == attachments {
            return;
        }
        self.record_change(0);
        *self.attachments = attachments;
        self.check();
    }

    /// Replace `source[range]` with `new`. Attachments whose markers
    /// survive into `new` (e.g. case-mapped markers, which case-fold to
    /// themselves) are kept; only markers actually removed get
    /// dropped. Endpoints are snapped.
    pub fn replace_range(&mut self, range: Range<usize>, new: &str) {
        let start = text::snap(self.source, range.start);
        let end = text::snap(self.source, range.end).max(start);
        if &self.source[start..end] == new {
            return;
        }
        self.record_change(start);
        let before = self.source[..start].matches(ATTACHMENT_MARKER).count();
        let removed = self.source[start..end].matches(ATTACHMENT_MARKER).count();
        let kept = new.matches(ATTACHMENT_MARKER).count();
        let drop = removed.saturating_sub(kept);
        self.source.replace_range(start..end, new);
        let drain_end = (before + drop).min(self.attachments.len());
        self.attachments.drain(before..drain_end);
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
        if !s.is_empty() {
            self.record_change(p);
            self.source.insert_str(p, s);
        }
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
        self.record_change(p);
        self.source.insert(p, c);
        p
    }

    /// Insert `attached` at `pos`, attachments included when this text holds
    /// them. Returns the inserted byte range.
    pub fn insert_attached(&mut self, pos: usize, attached: &AttachedText) -> Range<usize> {
        self.replace_attached(pos..pos, attached)
    }

    /// Replace `source[range]` with `attached`: the attachments in `range`
    /// go, and those of `attached` come in when this text holds them.
    /// Endpoints are snapped. Returns the inserted byte range.
    pub fn replace_attached(
        &mut self,
        range: Range<usize>,
        attached: &AttachedText,
    ) -> Range<usize> {
        let start = text::snap(self.source, range.start);
        let end = text::snap(self.source, range.end).max(start);
        let (new, incoming) = if self.holds_attachments {
            (Cow::Borrowed(attached.text()), attached.attachments())
        } else {
            (attached.plain_text(), &[][..])
        };
        if start == end && new.is_empty() {
            return start..start;
        }
        self.record_change(start);
        let before = self.source[..start].matches(ATTACHMENT_MARKER).count();
        let removed = self.source[start..end].matches(ATTACHMENT_MARKER).count();
        self.attachments
            .splice(before..before + removed, incoming.iter().cloned());
        self.source.replace_range(start..end, &new);
        self.check();
        start..start + new.len()
    }

    /// Atomically write an attachment marker + register `attachment` at the
    /// matching ordinal position. Returns the snapped offset of the marker.
    pub fn insert_marker(&mut self, pos: usize, attachment: Attachment) -> usize {
        let p = text::snap(self.source, pos);
        self.record_change(p);
        let idx = self.source[..p].matches(ATTACHMENT_MARKER).count();
        self.attachments.insert(idx, attachment);
        self.source.insert(p, ATTACHMENT_MARKER);
        self.check();
        p
    }

    /// Strip every `ATTACHMENT_MARKER` from `source` and drop every attachment.
    /// Returns the previous attachments so callers can re-bind them elsewhere.
    pub fn strip_attachments(&mut self) -> Vec<Attachment> {
        if self.attachments.is_empty() {
            return Vec::new();
        }
        self.record_change(0);
        let drained = core::mem::take(self.attachments);
        *self.source = self.source.replace(ATTACHMENT_MARKER, "");
        drained
    }

    fn check(&self) {
        debug_assert_eq!(
            self.source.matches(ATTACHMENT_MARKER).count(),
            self.attachments.len(),
            "AttachedTextMut: source marker count ≠ attachments.len()"
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

    fn id(n: u64) -> Attachment {
        Attachment::image(format!("{n}.png"), format!("data:{n}"))
    }

    #[test]
    fn from_labelled_inverts_labelled() {
        let m = ATTACHMENT_MARKER;
        let text = AttachedText::new(format!("a {m} b {m}{m}"), vec![id(1), id(2), id(1)]);
        let labelled = text.labelled();
        assert_eq!(labelled, "a [1.png] b [2.png][1.png]");
        assert_eq!(
            AttachedText::from_labelled(&labelled, text.attachments()),
            (text.clone(), Vec::new())
        );
    }

    #[test]
    fn from_labelled_follows_labels_moved_out_of_order() {
        let m = ATTACHMENT_MARKER;
        let (one, two) = (id(1), id(2));
        let rebound =
            AttachedText::from_labelled("[2.png] then [1.png]", &[one.clone(), two.clone()]);
        assert_eq!(
            rebound,
            (
                AttachedText::new(format!("{m} then {m}"), vec![two, one]),
                Vec::new()
            )
        );
    }

    #[test]
    fn from_labelled_returns_attachments_whose_label_was_removed() {
        let m = ATTACHMENT_MARKER;
        let (one, two) = (id(1), id(2));
        let rebound = AttachedText::from_labelled(
            &format!("edited [2.png] {m}tail"),
            &[one.clone(), two.clone()],
        );
        assert_eq!(
            rebound,
            (
                AttachedText::new(format!("edited {m} tail"), vec![two]),
                vec![one]
            )
        );
    }

    #[test]
    fn tracked_edits_record_the_earliest_changed_byte() {
        let mut source = "a中bc".to_string();
        let mut ids = Vec::new();
        let mut changed = SourceChanges::default();
        {
            let mut text = AttachedTextMut::tracked(&mut source, &mut ids, &mut changed, true);
            text.insert(6, 'x');
            text.replace_range(4..5, "yy");
            text.insert_str(2, "z");
        }
        assert_eq!(changed.changed_from, Some(1));
        assert_eq!(changed.revision, 3);
        assert!(source.starts_with('a'));
    }

    #[test]
    fn unchanged_mutations_preserve_revision() {
        let mut source = "a中bc".to_string();
        let mut ids = Vec::new();
        let mut changes = SourceChanges::default();
        {
            let mut text = AttachedTextMut::tracked(&mut source, &mut ids, &mut changes, true);
            text.insert_str(2, "");
            text.replace_range(1..4, "中");
            text.set_attachments(Vec::new());
            assert!(text.strip_attachments().is_empty());
            text.install("a中bc".into(), Vec::new());
        }
        assert_eq!(changes.revision, 0);
        assert_eq!(changes.changed_from, None);
        {
            let mut text = AttachedTextMut::tracked(&mut source, &mut ids, &mut changes, true);
            text.clear();
            text.clear();
        }
        assert_eq!(changes.revision, 1);
        assert_eq!(changes.changed_from, Some(0));
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
        let seven = id(7);
        let mut ids = vec![seven.clone()];
        let mut t = AttachedTextMut::new(&mut s, &mut ids);
        let upper = t.to_uppercase();
        let n = t.len();
        t.replace_range(0..n, &upper);
        assert_eq!(s, format!("A{ATTACHMENT_MARKER}B"));
        assert_eq!(ids, vec![seven]);
    }

    #[test]
    fn insert_marker_registers_id_at_ordinal_index() {
        let mut s = format!("{ATTACHMENT_MARKER}{ATTACHMENT_MARKER}");
        let (one, two, three) = (id(1), id(2), id(3));
        let mut ids = vec![one.clone(), three.clone()];
        let mut t = AttachedTextMut::new(&mut s, &mut ids);
        t.insert_marker(ATTACHMENT_MARKER.len_utf8(), two.clone());
        assert_eq!(ids, vec![one, two, three]);
        assert_eq!(s.matches(ATTACHMENT_MARKER).count(), 3);
    }

    #[test]
    fn strip_attachments_clears_both_halves() {
        let mut s = format!("a{ATTACHMENT_MARKER}b");
        let one = id(1);
        let mut ids = vec![one.clone()];
        let mut t = AttachedTextMut::new(&mut s, &mut ids);
        let drained = t.strip_attachments();
        assert_eq!(drained, vec![one]);
        assert_eq!(s, "ab");
        assert!(ids.is_empty());
    }

    #[test]
    fn sliced_text_pastes_back_with_its_attachments() {
        let (one, two) = (id(1), id(2));
        let mut s = format!("a{ATTACHMENT_MARKER}b{ATTACHMENT_MARKER}c");
        let mut ids = vec![one.clone(), two.clone()];
        let mut t = AttachedTextMut::new(&mut s, &mut ids);
        let cut = t.slice(1..t.len() - 1);
        assert_eq!(cut.attachments(), [one.clone(), two.clone()]);
        assert_eq!(cut.labelled(), "[1.png]b[2.png]");
        let range = t.insert_attached(0, &cut);
        assert_eq!(range, 0..cut.text().len());
        assert_eq!(ids, vec![one.clone(), two.clone(), one, two]);
    }

    #[test]
    fn text_without_attachment_slots_takes_pastes_as_plain_text() {
        let mut s = String::from("ab");
        let mut ids = Vec::new();
        let mut changes = SourceChanges::default();
        let mut t = AttachedTextMut::tracked(&mut s, &mut ids, &mut changes, false);
        let pasted = AttachedText::new(format!("x{ATTACHMENT_MARKER}y"), vec![id(1)]);
        assert_eq!(t.insert_attached(1, &pasted), 1..3);
        assert_eq!(s, "axyb");
        assert!(ids.is_empty());
    }
}
