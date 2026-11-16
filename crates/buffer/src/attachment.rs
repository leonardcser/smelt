//! Attachments: content a prompt carries alongside its text.
//!
//! A buffer marks each attachment with one `ATTACHMENT_MARKER` char and keeps
//! the [`Attachment`] at the same ordinal in its attachment list. Attachments
//! are values: undo snapshots, the kill ring and the stash hold them directly,
//! so they outlive the buffer they came from.

use sha2::{Digest, Sha256};
use std::sync::Arc;

pub use smelt_text::ATTACHMENT_MARKER;

/// An attached image. Clones share the content; equality is identity, so two
/// pastes of the same picture are two attachments. Use [`content_key`] where
/// equal bytes must count as one image, such as caches and the request
/// payload.
///
/// [`content_key`]: Attachment::content_key
#[derive(Clone, Debug)]
pub struct Attachment(Arc<Image>);

#[derive(Debug)]
struct Image {
    label: String,
    data_url: String,
    content_key: u64,
}

impl Attachment {
    pub fn image(label: String, data_url: String) -> Self {
        let digest = Sha256::new()
            .chain_update(b"image:")
            .chain_update(data_url.as_bytes())
            .finalize();
        let content_key = u64::from_le_bytes(digest[..8].try_into().expect("8 digest bytes"));
        Self(Arc::new(Image {
            label,
            data_url,
            content_key,
        }))
    }

    pub fn label(&self) -> &str {
        &self.0.label
    }

    pub fn data_url(&self) -> &str {
        &self.0.data_url
    }

    /// A key for caches of derived data, equal for equal content.
    pub fn content_key(&self) -> u64 {
        self.0.content_key
    }

    /// The `[label]` the prompt shows, with control chars made visible.
    pub fn display_label(&self) -> String {
        let label: String = self
            .0
            .label
            .chars()
            .map(|ch| if ch.is_control() { '\u{FFFD}' } else { ch })
            .collect();
        format!("[{label}]")
    }
}

impl PartialEq for Attachment {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Attachment {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_content_shares_a_content_key_but_not_identity() {
        let a = Attachment::image("a.png".into(), "data:image/png;base64,AAA".into());
        let b = Attachment::image("b.png".into(), "data:image/png;base64,AAA".into());
        let c = Attachment::image("a.png".into(), "data:image/png;base64,CCC".into());
        assert_eq!(a.content_key(), b.content_key());
        assert_ne!(a.content_key(), c.content_key());
        assert_ne!(a, b);
        assert_eq!(a, a.clone());
    }

    #[test]
    fn display_label_brackets_the_label_and_hides_control_chars() {
        let a = Attachment::image("shot\n1.png".into(), "data:".into());
        assert_eq!(a.display_label(), "[shot\u{FFFD}1.png]");
    }
}
