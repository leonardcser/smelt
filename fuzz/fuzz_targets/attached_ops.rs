#![no_main]

//! Direct fuzzing of `smelt_buffer::attached::AttachedTextMut` - the
//! lockstep `(source, attachment_ids)` invariant carrier. Bugs here
//! traced back to mis-counted markers across `replace_range` boundaries
//! (a removed marker whose byte range straddles the snap point), which
//! manifested far away as "attachment id list out of sync with source"
//! debug-asserts deep in the prompt code.
//!
//! The reference model is segment-based: an obviously-correct
//! `Vec<Seg>` where `Seg::Text(String)` and `Seg::Marker(id)` interleave.
//! After every op we rebuild the production state's segments and assert
//! they match the reference's segments - a stronger check than just
//! comparing `(source, ids)` because it catches realignment bugs that
//! would still pass the marker-count invariant.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use smelt_buffer::attached::AttachedTextMut;
use smelt_buffer::attachment::{AttachmentId, ATTACHMENT_MARKER};

/// Reference state. Segments alternate text/marker freely; rebuilding
/// `(source, ids)` from segments is a fold over the list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    Text(String),
    Marker(AttachmentId),
}

#[derive(Default, Clone)]
struct Ref {
    segs: Vec<Seg>,
}

impl Ref {
    fn from_state(source: &str, ids: &[AttachmentId]) -> Self {
        let mut segs = Vec::new();
        let mut cur = String::new();
        let mut id_iter = ids.iter().copied();
        for c in source.chars() {
            if c == ATTACHMENT_MARKER {
                if !cur.is_empty() {
                    segs.push(Seg::Text(core::mem::take(&mut cur)));
                }
                let id = id_iter
                    .next()
                    .expect("ATTACHED-INV: source marker without matching id");
                segs.push(Seg::Marker(id));
            } else {
                cur.push(c);
            }
        }
        assert!(
            id_iter.next().is_none(),
            "ATTACHED-INV: ids vec longer than marker count"
        );
        if !cur.is_empty() {
            segs.push(Seg::Text(cur));
        }
        Self { segs }
    }

    fn source(&self) -> String {
        let mut out = String::new();
        for s in &self.segs {
            match s {
                Seg::Text(t) => out.push_str(t),
                Seg::Marker(_) => out.push(ATTACHMENT_MARKER),
            }
        }
        out
    }

    fn ids(&self) -> Vec<AttachmentId> {
        self.segs
            .iter()
            .filter_map(|s| match s {
                Seg::Marker(id) => Some(*id),
                _ => None,
            })
            .collect()
    }

    /// Naive `replace_range`: snap endpoints via char_indices walk,
    /// rebuild `(source, ids)`, re-derive segments. Slow but
    /// unambiguous - divergence from prod is a real bug.
    fn replace_range(&mut self, range: core::ops::Range<usize>, new: &str) {
        let cur_source = self.source();
        let start = snap_chars(&cur_source, range.start);
        let end = snap_chars(&cur_source, range.end).max(start);
        // Count markers strictly before `start` (these ids survive).
        let mut before = 0usize;
        let mut byte = 0usize;
        for c in cur_source.chars() {
            let next = byte + c.len_utf8();
            if next <= start && c == ATTACHMENT_MARKER {
                before += 1;
            }
            byte = next;
            if next >= start {
                break;
            }
        }
        // Count markers strictly inside `start..end`.
        let mut removed = 0usize;
        let mut byte = 0usize;
        for c in cur_source.chars() {
            let next = byte + c.len_utf8();
            if byte >= start && next <= end && c == ATTACHMENT_MARKER {
                removed += 1;
            }
            byte = next;
            if next > end {
                break;
            }
        }
        let kept_in_new = new.matches(ATTACHMENT_MARKER).count();
        let drop_count = removed.saturating_sub(kept_in_new);
        // Build new source.
        let mut new_source = String::with_capacity(cur_source.len() + new.len());
        new_source.push_str(&cur_source[..start]);
        new_source.push_str(new);
        new_source.push_str(&cur_source[end..]);
        // Build new ids: keep first `before`, drop `drop_count`, keep rest.
        let mut ids = self.ids();
        let drain_end = (before + drop_count).min(ids.len());
        ids.drain(before..drain_end);
        *self = Ref::from_state(&new_source, &ids);
    }

    fn insert_str(&mut self, pos: usize, s: &str) -> usize {
        // Caller must not pass an embedded marker.
        let cur_source = self.source();
        let p = snap_chars(&cur_source, pos);
        let mut new_source = String::new();
        new_source.push_str(&cur_source[..p]);
        new_source.push_str(s);
        new_source.push_str(&cur_source[p..]);
        let ids = self.ids();
        *self = Ref::from_state(&new_source, &ids);
        p
    }

    fn insert_char(&mut self, pos: usize, c: char) -> usize {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.insert_str(pos, s)
    }

    fn insert_marker(&mut self, pos: usize, id: AttachmentId) -> usize {
        let cur_source = self.source();
        let p = snap_chars(&cur_source, pos);
        let idx_before = cur_source[..p].matches(ATTACHMENT_MARKER).count();
        let mut new_source = String::new();
        new_source.push_str(&cur_source[..p]);
        new_source.push(ATTACHMENT_MARKER);
        new_source.push_str(&cur_source[p..]);
        let mut ids = self.ids();
        ids.insert(idx_before, id);
        *self = Ref::from_state(&new_source, &ids);
        p
    }

    fn strip_attachments(&mut self) -> Vec<AttachmentId> {
        let dropped = self.ids();
        self.segs.retain(|s| matches!(s, Seg::Text(_)));
        dropped
    }

    fn clear(&mut self) {
        self.segs.clear();
    }
}

fn snap_chars(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut last = 0;
    for (i, _) in s.char_indices() {
        if i > pos {
            return last;
        }
        last = i;
    }
    last
}

#[derive(Arbitrary, Debug)]
enum AttachedOp {
    /// `replace_range` - the canonical id-realignment surface.
    Replace { start: u32, end: u32, with: String },
    /// `insert_str` - must not contain a marker; we strip them defensively.
    InsertStr { pos: u32, s: String },
    /// `insert` - single char; we substitute non-marker chars.
    Insert { pos: u32, ch: u32 },
    /// `insert_marker` - atomic source-marker + id push.
    InsertMarker { pos: u32, id: u64 },
    /// `strip_attachments` - drops every marker + id.
    Strip,
    /// `clear` - empties both halves.
    Clear,
    /// `install` - wholesale swap; we only accept self-consistent pairs.
    Install {
        src_ids: Vec<u64>,
        text_between: Vec<String>,
    },
}

#[derive(Arbitrary, Debug)]
struct Input {
    initial_text: String,
    initial_id_count: u8,
    ops: Vec<AttachedOp>,
}

fn assert_invariant(source: &str, ids: &[AttachmentId]) {
    let n_markers = source.matches(ATTACHMENT_MARKER).count();
    if n_markers != ids.len() {
        panic!(
            "ATTACHED-INV: marker count {n_markers} ≠ ids.len() {} (source={source:?})",
            ids.len()
        );
    }
}

fn run(input: Input) {
    // Build a self-consistent starting state. Strip any markers from the
    // arbitrary text, then prepend `initial_id_count` markers with ids.
    let stripped: String = input.initial_text.replace(ATTACHMENT_MARKER, "");
    let n = (input.initial_id_count as usize) % 4;
    let mut source = String::new();
    for i in 0..n {
        source.push(ATTACHMENT_MARKER);
        let _ = i;
    }
    source.push_str(&stripped);
    let mut ids: Vec<AttachmentId> = (0..n as u64).collect();
    let mut reference = Ref::from_state(&source, &ids);
    assert_invariant(&source, &ids);

    let take = input.ops.len().min(64);
    for op in input.ops.into_iter().take(take) {
        match op {
            AttachedOp::Replace { start, end, with } => {
                let with_safe: String = with.replace(ATTACHMENT_MARKER, ""); // marker-free for plain replace
                let range = (start as usize)..(end as usize);
                {
                    let mut t = AttachedTextMut::new(&mut source, &mut ids);
                    t.replace_range(range.clone(), &with_safe);
                }
                reference.replace_range(range, &with_safe);
            }
            AttachedOp::InsertStr { pos, s } => {
                let safe: String = s.replace(ATTACHMENT_MARKER, "");
                {
                    let mut t = AttachedTextMut::new(&mut source, &mut ids);
                    let _ = t.insert_str(pos as usize, &safe);
                }
                let _ = reference.insert_str(pos as usize, &safe);
            }
            AttachedOp::Insert { pos, ch } => {
                let c = char::from_u32(ch).unwrap_or('?');
                let c = if c == ATTACHMENT_MARKER { '?' } else { c };
                {
                    let mut t = AttachedTextMut::new(&mut source, &mut ids);
                    let _ = t.insert(pos as usize, c);
                }
                let _ = reference.insert_char(pos as usize, c);
            }
            AttachedOp::InsertMarker { pos, id } => {
                {
                    let mut t = AttachedTextMut::new(&mut source, &mut ids);
                    let _ = t.insert_marker(pos as usize, id);
                }
                let _ = reference.insert_marker(pos as usize, id);
            }
            AttachedOp::Strip => {
                let prod_dropped = {
                    let mut t = AttachedTextMut::new(&mut source, &mut ids);
                    t.strip_attachments()
                };
                let ref_dropped = reference.strip_attachments();
                assert_eq!(
                    prod_dropped, ref_dropped,
                    "strip_attachments returned different id lists"
                );
            }
            AttachedOp::Clear => {
                {
                    let mut t = AttachedTextMut::new(&mut source, &mut ids);
                    t.clear();
                }
                reference.clear();
            }
            AttachedOp::Install {
                src_ids,
                text_between,
            } => {
                // Build a self-consistent (source, ids) pair from the
                // arbitrary inputs: one marker per id, optional text
                // between them, no embedded markers.
                let n = src_ids.len();
                let mut built = String::new();
                for (i, id) in src_ids.iter().enumerate() {
                    let between = text_between.get(i).map(|s| s.as_str()).unwrap_or("");
                    let safe: String = between.replace(ATTACHMENT_MARKER, "");
                    built.push_str(&safe);
                    built.push(ATTACHMENT_MARKER);
                    let _ = id;
                }
                // Optional trailing text.
                if let Some(t) = text_between.get(n) {
                    built.push_str(&t.replace(ATTACHMENT_MARKER, ""));
                }
                let new_ids: Vec<AttachmentId> = src_ids.clone();
                {
                    let mut t = AttachedTextMut::new(&mut source, &mut ids);
                    t.install(built.clone(), new_ids.clone());
                }
                reference = Ref::from_state(&built, &new_ids);
            }
        }
        // 1) Production invariant (matches debug-assert in AttachedTextMut).
        assert_invariant(&source, &ids);
        // 2) Reference agreement - the differential.
        let ref_source = reference.source();
        let ref_ids = reference.ids();
        if source != ref_source {
            panic!(
                "ATTACHED-DIFF: source mismatch\n  prod: {source:?}\n   ref: {ref_source:?}\n   ids: {ids:?}"
            );
        }
        if ids != ref_ids {
            panic!(
                "ATTACHED-DIFF: ids mismatch\n  prod: {ids:?}\n   ref: {ref_ids:?}\n source: {source:?}"
            );
        }
    }
}

fuzz_target!(|input: Input| {
    run(input);
});
