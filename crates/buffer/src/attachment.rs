use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub type AttachmentId = u64;

/// The character used as a placeholder for attachments in source text.
pub const ATTACHMENT_MARKER: char = '\u{FFFC}';

/// A single attachment (currently images only).
#[derive(Clone, Debug)]
pub enum Attachment {
    Image { label: String, data_url: String },
}

impl Attachment {
    pub fn display_label(&self) -> String {
        match self {
            Attachment::Image { label, .. } => format!("[{}]", display_safe_label(label)),
        }
    }

    pub fn expanded_text(&self) -> &str {
        match self {
            Attachment::Image { .. } => "",
        }
    }

    fn content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        match self {
            Attachment::Image { data_url, .. } => {
                hasher.update(b"image:");
                hasher.update(data_url.as_bytes());
            }
        }
        format!("{:x}", hasher.finalize())
    }
}

fn display_safe_label(label: &str) -> String {
    label
        .chars()
        .map(|ch| if ch.is_control() { '\u{FFFD}' } else { ch })
        .collect()
}

// ── Store ────────────────────────────────────────────────────────────────────

/// Session-global attachment registry.
pub struct AttachmentStore {
    entries: HashMap<AttachmentId, Attachment>,
    next_id: AttachmentId,
    hash_to_id: HashMap<String, AttachmentId>,
}

impl Default for AttachmentStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AttachmentStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
            hash_to_id: HashMap::new(),
        }
    }

    /// Insert an attachment, deduplicating by content hash. Returns the id.
    pub fn insert(&mut self, att: Attachment) -> AttachmentId {
        let hash = att.content_hash();
        if let Some(&existing) = self.hash_to_id.get(&hash) {
            return existing;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.hash_to_id.insert(hash, id);
        self.entries.insert(id, att);
        id
    }

    pub fn get(&self, id: AttachmentId) -> Option<&Attachment> {
        self.entries.get(&id)
    }

    pub fn display_label(&self, id: AttachmentId) -> String {
        self.entries
            .get(&id)
            .map(|a| a.display_label())
            .unwrap_or_else(|| "[?]".into())
    }

    pub fn expanded_text(&self, id: AttachmentId) -> &str {
        self.entries
            .get(&id)
            .map(|a| a.expanded_text())
            .unwrap_or("")
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.hash_to_id.clear();
        self.next_id = 1;
    }

    pub fn insert_image(&mut self, label: String, data_url: String) -> AttachmentId {
        self.insert(Attachment::Image { label, data_url })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_deduplicates() {
        let mut store = AttachmentStore::new();
        let id1 = store.insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id2 = store.insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        assert_eq!(id1, id2);
        assert_eq!(store.entries.len(), 1);
    }

    #[test]
    fn different_content_different_ids() {
        let mut store = AttachmentStore::new();
        let id1 = store.insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let id2 = store.insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        assert_ne!(id1, id2);
        assert_eq!(store.entries.len(), 2);
    }

    #[test]
    fn display_label() {
        let mut store = AttachmentStore::new();
        let id = store.insert_image("screenshot.png".into(), "data:...".into());
        assert_eq!(store.display_label(id), "[screenshot.png]");
    }

    #[test]
    fn expanded_text_image() {
        let mut store = AttachmentStore::new();
        let id = store.insert_image("img.png".into(), "data:...".into());
        assert_eq!(store.expanded_text(id), "");
    }

    // ── id lookup ─────────────────────────────────────────────────────────

    #[test]
    fn get_returns_attachment_for_known_id() {
        let mut store = AttachmentStore::new();
        let id = store.insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let att = store.get(id).expect("known id resolves");
        match att {
            Attachment::Image { label, .. } => assert_eq!(label, "a.png"),
        }
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let store = AttachmentStore::new();
        assert!(store.get(99999).is_none());
    }

    #[test]
    fn display_label_for_unknown_id_returns_question_mark_placeholder() {
        let store = AttachmentStore::new();
        assert_eq!(store.display_label(99999), "[?]");
    }

    #[test]
    fn expanded_text_for_unknown_id_returns_empty() {
        let store = AttachmentStore::new();
        assert_eq!(store.expanded_text(99999), "");
    }

    // ── clear ─────────────────────────────────────────────────────────────

    #[test]
    fn clear_removes_all_entries_and_resets_id_minting() {
        let mut store = AttachmentStore::new();
        let id1 = store.insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        store.insert_image("b.png".into(), "data:image/png;base64,BBB".into());
        store.clear();
        assert!(store.get(id1).is_none(), "old ids are gone after clear");
        // Re-inserting the same content gets id 1 again - counter was reset.
        let new_id = store.insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        assert_eq!(new_id, 1);
    }
}
