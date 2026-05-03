use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub type AttachmentId = u64;

/// A single attachment: either a collapsed paste or an image.
#[derive(Clone, Debug)]
pub enum Attachment {
    Paste { content: String },
    Image { label: String, data_url: String },
}

impl Attachment {
    pub fn display_label(&self) -> String {
        match self {
            Attachment::Paste { content } => {
                let lines = content.lines().count().max(1);
                format!("[pasted {lines} lines]")
            }
            Attachment::Image { label, .. } => format!("[{label}]"),
        }
    }

    pub fn expanded_text(&self) -> &str {
        match self {
            Attachment::Paste { content } => content.as_str(),
            Attachment::Image { .. } => "",
        }
    }

    fn content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        match self {
            Attachment::Paste { content } => {
                hasher.update(b"paste:");
                hasher.update(content.as_bytes());
            }
            Attachment::Image { data_url, .. } => {
                hasher.update(b"image:");
                hasher.update(data_url.as_bytes());
            }
        }
        format!("{:x}", hasher.finalize())
    }
}

// ── Store ────────────────────────────────────────────────────────────────────

/// Global attachment registry. Owns all attachment data for the session.
pub struct AttachmentStore {
    entries: HashMap<AttachmentId, Attachment>,
    next_id: AttachmentId,
    /// Content hash → ID for deduplication.
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

    /// Insert an attachment, deduplicating by content hash.
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

    /// Insert an image and return its ID. Convenience wrapper.
    pub fn insert_image(&mut self, label: String, data_url: String) -> AttachmentId {
        self.insert(Attachment::Image { label, data_url })
    }

    /// Insert a paste and return its ID. Convenience wrapper.
    pub fn insert_paste(&mut self, content: String) -> AttachmentId {
        self.insert(Attachment::Paste { content })
    }

    // ── Blob persistence ─────────────────────────────────────────────────

    /// Snapshot `(filename, data_url)` pairs for every image attachment.
    /// Shared by the background persister and the sync `save_blobs` path.
    pub fn image_blobs(&self) -> Vec<(String, String)> {
        self.entries
            .values()
            .filter_map(|att| match att {
                Attachment::Image { data_url, .. } => {
                    let hash = att.content_hash();
                    let ext = mime_to_ext(data_url);
                    Some((format!("{hash}.{ext}"), data_url.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Write all image attachments as blob files and return the
    /// data_url → `blob:<filename>` replacement map.
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

    /// Read blob files and resolve `blob:` refs back to data URLs.
    /// Returns a map from `blob:<filename>` → data URL string.
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
        let id = store.insert_paste("line1\nline2\nline3".into());
        assert_eq!(store.display_label(id), "[pasted 3 lines]");

        let id2 = store.insert_image("screenshot.png".into(), "data:...".into());
        assert_eq!(store.display_label(id2), "[screenshot.png]");
    }

    #[test]
    fn expanded_text_paste() {
        let mut store = AttachmentStore::new();
        let id = store.insert_paste("hello world".into());
        assert_eq!(store.expanded_text(id), "hello world");
    }

    #[test]
    fn expanded_text_image() {
        let mut store = AttachmentStore::new();
        let id = store.insert_image("img.png".into(), "data:...".into());
        assert_eq!(store.expanded_text(id), "");
    }
}
