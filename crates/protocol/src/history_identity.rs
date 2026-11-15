//! Content identity of a conversation. A backend that keeps its own copy of the
//! conversation compares identities instead of receiving the history each turn.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::event::ModelHistorySource;
use crate::history::HistoryItem;

/// Hash chain over the conversation items of a canonical history prefix.
/// Notes and system items are not conversation and leave it unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HistoryChain([u8; 32]);

impl HistoryChain {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn extend(self, item: &HistoryItem) -> Self {
        if !is_conversation(item) {
            return self;
        }
        let json = serde_json::to_vec(item).expect("history items are JSON serializable");
        self.extend_encoded(item, &json)
    }

    /// Extends the chain with `item`, whose `serde_json` encoding is `json`.
    pub fn extend_encoded(self, item: &HistoryItem, json: &[u8]) -> Self {
        if !is_conversation(item) {
            return self;
        }
        let mut next = Sha256::new();
        next.update(self.0);
        next.update(Sha256::digest(json));
        Self(next.finalize().into())
    }

    pub fn of<'a>(items: impl IntoIterator<Item = &'a HistoryItem>) -> Self {
        items.into_iter().fold(Self::default(), Self::extend)
    }
}

fn is_conversation(item: &HistoryItem) -> bool {
    !matches!(item, HistoryItem::Note(_) | HistoryItem::System { .. })
}

/// A turn's model history by content: the canonical history `[0, len)` and,
/// after compaction, the summary that stands in for `[0, start)`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryIdentity {
    /// Items in the canonical history, notes included, so it indexes it.
    /// `key` leaves it out.
    pub len: usize,
    pub chain: HistoryChain,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointIdentity {
    pub start: usize,
    /// Chain of the summary item.
    pub summary: HistoryChain,
}

impl HistoryIdentity {
    /// The identity of the whole canonical `history`, without a checkpoint.
    pub fn of(history: &[HistoryItem]) -> Self {
        Self {
            len: history.len(),
            chain: HistoryChain::of(history),
            checkpoint: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0 && self.checkpoint.is_none()
    }

    pub fn extend(&mut self, item: &HistoryItem) {
        self.len += 1;
        self.chain = self.chain.extend(item);
    }

    /// The conversation as a hex digest. Histories that differ only in notes
    /// share it.
    pub fn key(&self) -> String {
        let mut key = Sha256::new();
        key.update(self.chain.0);
        if let Some(checkpoint) = &self.checkpoint {
            key.update((checkpoint.start as u64).to_le_bytes());
            key.update(checkpoint.summary.0);
        }
        key.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// The history a turn starts from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TurnHistory {
    /// The model-visible history.
    Items(ModelHistorySource),
    /// The history by content, for backends that keep their own conversation.
    /// They request the items from the host only when they must import them.
    Identity(HistoryIdentity),
}

impl Default for TurnHistory {
    fn default() -> Self {
        Self::Items(ModelHistorySource::default())
    }
}

impl From<ModelHistorySource> for TurnHistory {
    fn from(source: ModelHistorySource) -> Self {
        Self::Items(source)
    }
}

impl TurnHistory {
    pub fn items(&self) -> Option<&ModelHistorySource> {
        match self {
            Self::Items(source) => Some(source),
            Self::Identity(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Content, HistoryNote};

    fn user(text: &str) -> HistoryItem {
        HistoryItem::user(Content::text(text))
    }

    #[test]
    fn chain_depends_on_conversation_order_and_ignores_notes() {
        let a = HistoryChain::of(&[user("one"), user("two")]);
        assert_ne!(a, HistoryChain::of(&[user("two"), user("one")]));
        assert_eq!(
            a,
            HistoryChain::of(&[
                user("one"),
                HistoryItem::note(HistoryNote::context("cwd")),
                user("two"),
            ])
        );
        let item = user("three");
        let json = serde_json::to_vec(&item).unwrap();
        assert_eq!(a.extend(&item), a.extend_encoded(&item, &json));
    }

    #[test]
    fn identity_extends_like_the_history_it_covers_and_keys_its_checkpoint() {
        let mut identity = HistoryIdentity::of(&[user("one")]);
        identity.extend(&HistoryItem::note(HistoryNote::context("cwd")));
        identity.extend(&user("two"));
        assert_eq!(identity.len, 3);
        assert_eq!(
            identity.key(),
            HistoryIdentity::of(&[user("one"), user("two")]).key()
        );
        let compacted = HistoryIdentity {
            checkpoint: Some(CheckpointIdentity {
                start: 1,
                summary: HistoryChain::of(&[user("summary")]),
            }),
            ..identity.clone()
        };
        assert_ne!(compacted.key(), identity.key());
    }
}
