use protocol::HistoryItem;
use serde_json::Value;

use crate::history::TranscriptDescriptorRecord;
use crate::meta::{SessionIdentity, SessionMetadata};

macro_rules! typed_u64 {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Eq,
            Ord,
            PartialEq,
            PartialOrd,
            serde::Deserialize,
            serde::Serialize,
        )]
        pub struct $name(u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }

            pub const fn checked_add(self, amount: u64) -> Option<Self> {
                match self.0.checked_add(amount) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            pub fn as_usize(self) -> Option<usize> {
                usize::try_from(self.0).ok()
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl TryFrom<usize> for $name {
            type Error = std::num::TryFromIntError;

            fn try_from(value: usize) -> Result<Self, Self::Error> {
                Ok(Self(u64::try_from(value)?))
            }
        }
    };
}

typed_u64!(Revision);
typed_u64!(HistoryIndex);
typed_u64!(HistoryLen);
typed_u64!(DescriptorIndex);
typed_u64!(DescriptorLen);
typed_u64!(SaveId);

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SessionCommit {
    pub session_id: String,
    pub save_id: SaveId,
    pub expected: StoreHead,
    pub identity: SessionIdentity,
    pub metadata: SessionMetadata,
    pub history: HistorySuffix,
    pub side_tables: SideTableSuffixes,
    pub descriptors: Option<TranscriptDescriptorSuffix>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct HistorySuffix {
    pub start: HistoryIndex,
    pub final_len: HistoryLen,
    pub items: Vec<HistoryItem>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SideTableSuffixes {
    pub start: HistoryIndex,
    pub turn_metas: Vec<(HistoryIndex, Value)>,
    pub metadata_snapshots: Vec<(HistoryIndex, Value)>,
    pub context_snapshots: Vec<(HistoryIndex, Value)>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TranscriptDescriptorSuffix {
    pub start: DescriptorIndex,
    pub records: Vec<TranscriptDescriptorRecord>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StoreHead {
    pub revision: Revision,
    pub history_len: HistoryLen,
    pub descriptor_len: DescriptorLen,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SaveReceipt {
    pub session_id: String,
    pub save_id: SaveId,
    pub previous: StoreHead,
    pub current: StoreHead,
}

impl SaveReceipt {
    pub const fn head(&self) -> StoreHead {
        self.current
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum HistoryIndexBound {
    BeforeFinalLen,
    AtOrBeforeFinalLen,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum SessionPersistenceDisposition {
    Retry,
    #[default]
    Reopen,
    ReadOnly,
    OwnershipLost,
}

impl SessionPersistenceDisposition {
    pub const fn should_retry_automatically(self) -> bool {
        matches!(self, Self::Retry)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum SessionCommitFailure {
    SessionMismatch {
        expected: String,
        actual: Option<String>,
    },
    IdentityMismatch {
        stored: SessionIdentity,
        attempted: SessionIdentity,
    },
    StaleBase {
        expected: StoreHead,
        current: StoreHead,
    },
    InvalidHistorySuffix {
        start: HistoryIndex,
        final_len: HistoryLen,
        item_count: u64,
    },
    InvalidDescriptorSuffix {
        start: DescriptorIndex,
        current_len: DescriptorLen,
    },
    InvalidSideTableSuffix {
        start: HistoryIndex,
        final_len: HistoryLen,
    },
    InvalidSideTableRow {
        table: String,
        index: HistoryIndex,
        final_len: HistoryLen,
        bound: HistoryIndexBound,
    },
    OwnershipLost,
    Busy {
        operation: String,
        attempts: u32,
        waited_ms: u64,
    },
    UnsupportedSchema {
        found: i32,
        expected: i32,
    },
    InvalidCommand {
        message: String,
    },
    Integrity {
        message: String,
    },
    Io {
        message: String,
        #[serde(default)]
        disposition: SessionPersistenceDisposition,
    },
    Sqlite {
        message: String,
        #[serde(default)]
        disposition: SessionPersistenceDisposition,
    },
}

impl SessionCommitFailure {
    pub fn is_recoverable_stale_base(&self) -> bool {
        matches!(self, Self::StaleBase { .. })
    }

    pub const fn disposition(&self) -> SessionPersistenceDisposition {
        match self {
            Self::StaleBase { .. } | Self::Busy { .. } => SessionPersistenceDisposition::Retry,
            Self::OwnershipLost => SessionPersistenceDisposition::OwnershipLost,
            Self::Io { disposition, .. } | Self::Sqlite { disposition, .. } => *disposition,
            Self::SessionMismatch { .. }
            | Self::IdentityMismatch { .. }
            | Self::InvalidHistorySuffix { .. }
            | Self::InvalidDescriptorSuffix { .. }
            | Self::InvalidSideTableSuffix { .. }
            | Self::InvalidSideTableRow { .. }
            | Self::UnsupportedSchema { .. }
            | Self::InvalidCommand { .. }
            | Self::Integrity { .. } => SessionPersistenceDisposition::ReadOnly,
        }
    }

    pub fn invalidates_connection(&self) -> bool {
        matches!(self, Self::Sqlite { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_failure_preserves_persistence_disposition() {
        let stale = SessionCommitFailure::StaleBase {
            expected: StoreHead {
                revision: Revision::new(1),
                history_len: HistoryLen::new(2),
                descriptor_len: DescriptorLen::new(3),
            },
            current: StoreHead {
                revision: Revision::new(4),
                history_len: HistoryLen::new(5),
                descriptor_len: DescriptorLen::new(6),
            },
        };
        let ownership_lost = SessionCommitFailure::OwnershipLost;
        let integrity = SessionCommitFailure::Integrity {
            message: "bad suffix".into(),
        };
        let sqlite = SessionCommitFailure::Sqlite {
            message: "connection failed".into(),
            disposition: SessionPersistenceDisposition::Reopen,
        };
        let busy = SessionCommitFailure::Busy {
            operation: "commit session".into(),
            attempts: 6,
            waited_ms: 250,
        };
        let io = SessionCommitFailure::Io {
            message: "temporary filesystem failure".into(),
            disposition: SessionPersistenceDisposition::Retry,
        };

        assert!(stale.is_recoverable_stale_base());
        assert!(!ownership_lost.is_recoverable_stale_base());
        assert!(!integrity.is_recoverable_stale_base());
        assert_eq!(stale.disposition(), SessionPersistenceDisposition::Retry);
        assert_eq!(busy.disposition(), SessionPersistenceDisposition::Retry);
        assert_eq!(io.disposition(), SessionPersistenceDisposition::Retry);
        assert_eq!(sqlite.disposition(), SessionPersistenceDisposition::Reopen);
        assert_eq!(
            ownership_lost.disposition(),
            SessionPersistenceDisposition::OwnershipLost
        );
        assert_eq!(
            integrity.disposition(),
            SessionPersistenceDisposition::ReadOnly
        );
        assert!(sqlite.invalidates_connection());
        assert!(!stale.invalidates_connection());
    }

    #[test]
    fn commit_failure_without_disposition_defaults_to_manual_reopen() {
        let failure: SessionCommitFailure = serde_json::from_value(serde_json::json!({
            "Io": { "message": "legacy failure" }
        }))
        .expect("deserialize commit failure");

        assert_eq!(failure.disposition(), SessionPersistenceDisposition::Reopen);
    }

    #[test]
    fn typed_lengths_do_not_compare_across_domains() {
        let history_len = HistoryLen::new(7);
        let descriptor_len = DescriptorLen::new(7);

        assert_eq!(history_len.get(), descriptor_len.get());
        assert_eq!(history_len.as_usize(), Some(7));
    }

    #[test]
    fn commit_failure_serializes_complete_stale_head() {
        let failure = SessionCommitFailure::StaleBase {
            expected: StoreHead {
                revision: Revision::new(301),
                history_len: HistoryLen::new(302),
                descriptor_len: DescriptorLen::new(303),
            },
            current: StoreHead {
                revision: Revision::new(109),
                history_len: HistoryLen::new(110),
                descriptor_len: DescriptorLen::new(111),
            },
        };

        let json = serde_json::to_value(&failure).expect("serialize failure");

        assert_eq!(json["StaleBase"]["expected"]["descriptor_len"], 303);
        assert_eq!(json["StaleBase"]["current"]["revision"], 109);
        assert_eq!(json["StaleBase"]["current"]["history_len"], 110);
        assert_eq!(json["StaleBase"]["current"]["descriptor_len"], 111);
    }
}
