use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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
            Attachment::Image { label, .. } => format!("[{label}]"),
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

    // ── Blob persistence ─────────────────────────────────────────────────

    /// `(filename, data_url)` pairs for every image attachment.
    pub fn image_blobs(&self) -> Vec<(String, String)> {
        self.entries
            .values()
            .map(|att| match att {
                Attachment::Image { data_url, .. } => {
                    let hash = att.content_hash();
                    let ext = mime_to_ext(data_url);
                    (format!("{hash}.{ext}"), data_url.clone())
                }
            })
            .collect()
    }

    /// Write all image attachments to `blob_dir` and return a data_url → `blob:<filename>` map.
    pub fn save_blobs(&self, blob_dir: &Path) -> HashMap<String, String> {
        let blobs = self.image_blobs();
        if blobs.is_empty() {
            return HashMap::new();
        }
        let _ = fs::create_dir_all(blob_dir);
        let mut url_to_blob = HashMap::with_capacity(blobs.len());
        for (filename, data_url) in blobs {
            let blob_path = blob_dir.join(&filename);
            if !blob_path.exists() {
                let _ = fs::write(&blob_path, data_url.as_bytes());
            }
            url_to_blob.insert(data_url, format!("blob:{filename}"));
        }
        url_to_blob
    }

    /// Read blob files, returning a `blob:<filename>` → data URL map.
    pub fn load_blobs(blob_dir: &Path) -> HashMap<String, String> {
        let mut blob_to_url = HashMap::new();
        let Ok(entries) = fs::read_dir(blob_dir) else {
            return blob_to_url;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if let Ok(data) = fs::read_to_string(&path) {
                blob_to_url.insert(format!("blob:{name}"), data);
            }
        }
        blob_to_url
    }
}

fn mime_to_ext(data_url: &str) -> &str {
    if data_url.starts_with("data:image/jpeg") {
        "jpg"
    } else if data_url.starts_with("data:image/gif") {
        "gif"
    } else if data_url.starts_with("data:image/webp") {
        "webp"
    } else if data_url.starts_with("data:image/svg") {
        "svg"
    } else {
        "png"
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

    // ── Blob persistence ─────────────────────────────────────────────────

    #[test]
    fn image_blobs_filename_uses_extension_matching_mime_type() {
        let mut store = AttachmentStore::new();
        store.insert_image("jpg.jpg".into(), "data:image/jpeg;base64,j".into());
        store.insert_image("gif.gif".into(), "data:image/gif;base64,g".into());
        store.insert_image("webp.webp".into(), "data:image/webp;base64,w".into());
        store.insert_image("svg.svg".into(), "data:image/svg+xml;base64,s".into());
        store.insert_image("png.png".into(), "data:image/png;base64,p".into());
        let blobs = store.image_blobs();
        let exts: std::collections::HashSet<String> = blobs
            .iter()
            .filter_map(|(name, _)| name.rsplit_once('.').map(|(_, e)| e.to_string()))
            .collect();
        for e in ["jpg", "gif", "webp", "svg", "png"] {
            assert!(exts.contains(e), "expected extension {e}; got {exts:?}");
        }
    }

    #[test]
    fn image_blobs_filenames_are_content_addressed() {
        let mut store = AttachmentStore::new();
        // Different *labels*, same data_url → same content hash → same filename.
        let id_a = store.insert_image("rename1.png".into(), "data:image/png;base64,SAME".into());
        // Same data_url, dedup will return the same id; filename is one entry.
        let id_b = store.insert_image("rename2.png".into(), "data:image/png;base64,SAME".into());
        assert_eq!(id_a, id_b, "dedup confirms content hash drives identity");
        let blobs = store.image_blobs();
        assert_eq!(blobs.len(), 1);
        // Filename is content-derived, not label-derived: neither label appears.
        assert!(!blobs[0].0.contains("rename1"));
        assert!(!blobs[0].0.contains("rename2"));
    }

    #[test]
    fn save_blobs_then_load_blobs_round_trips_data_urls() {
        let mut store = AttachmentStore::new();
        store.insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        store.insert_image("b.jpg".into(), "data:image/jpeg;base64,BBB".into());
        let tmp = tempfile::tempdir().unwrap();
        let saved = store.save_blobs(tmp.path());
        // Save returns data_url → blob:<filename>.
        assert_eq!(saved.len(), 2);
        assert!(saved.values().all(|v| v.starts_with("blob:")));
        // Load reads them back as blob:<filename> → data_url.
        let loaded = AttachmentStore::load_blobs(tmp.path());
        for (data_url, blob_ref) in &saved {
            assert_eq!(loaded.get(blob_ref), Some(data_url));
        }
    }

    #[test]
    fn save_blobs_skips_existing_files() {
        let mut store = AttachmentStore::new();
        store.insert_image("a.png".into(), "data:image/png;base64,AAA".into());
        let tmp = tempfile::tempdir().unwrap();
        let _ = store.save_blobs(tmp.path());
        // Mutate the file on disk; a second save_blobs must not overwrite.
        let blob_path = std::fs::read_dir(tmp.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&blob_path, b"sentinel-content").unwrap();
        let _ = store.save_blobs(tmp.path());
        let on_disk = std::fs::read(&blob_path).unwrap();
        assert_eq!(on_disk, b"sentinel-content");
    }

    #[test]
    fn load_blobs_on_missing_dir_returns_empty_map() {
        let missing = std::path::PathBuf::from("/no/such/dir/should/exist/here");
        let loaded = AttachmentStore::load_blobs(&missing);
        assert!(loaded.is_empty());
    }
}
