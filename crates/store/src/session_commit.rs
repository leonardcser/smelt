use protocol::HistoryItem;
use serde_json::Value;

use crate::history::StoredTranscriptBlock;
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
typed_u64!(TranscriptRecordIndex);
typed_u64!(TranscriptRecordCount);
typed_u64!(TurnId);

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    User,
    Command,
    Continuation,
    Note,
}

impl TurnKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Command => "command",
            Self::Continuation => "continuation",
            Self::Note => "note",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "command" => Some(Self::Command),
            "continuation" => Some(Self::Continuation),
            "note" => Some(Self::Note),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Ready,
    Running,
    Completed,
    Interrupted,
    Failed,
    Cancelled,
}

impl TurnState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Interrupted | Self::Failed | Self::Cancelled
        )
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "ready" => Some(Self::Ready),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "interrupted" => Some(Self::Interrupted),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct NewTurn {
    pub kind: TurnKind,
    pub submitted_history_idx: HistoryIndex,
    pub continuation_of: Option<TurnId>,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SubmitTurn {
    pub session: SessionCommit,
    pub turn: NewTurn,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SubmitTurnReceipt {
    pub session: SaveReceipt,
    pub turn_id: TurnId,
}

impl SubmitTurnReceipt {
    pub const fn head(&self) -> StoreHead {
        self.session.current
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TurnTransition {
    pub session: SessionCommit,
    pub turn_id: TurnId,
    pub state: TurnState,
    pub at_ms: u64,
    pub terminal_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct TurnTransitionReceipt {
    pub session: SaveReceipt,
    pub turn_id: TurnId,
    pub state: TurnState,
}

impl TurnTransitionReceipt {
    pub const fn head(&self) -> StoreHead {
        self.session.current
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StartupRecoveryReceipt {
    pub session: SaveReceipt,
    pub interrupted_turns: Vec<TurnId>,
}

impl StartupRecoveryReceipt {
    pub const fn head(&self) -> StoreHead {
        self.session.current
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StoredTurn {
    pub turn_id: TurnId,
    pub submitted_history_idx: HistoryIndex,
    pub submitted_history_hash: String,
    pub submitted_revision: Revision,
    pub kind: TurnKind,
    pub state: TurnState,
    pub continuation_of: Option<TurnId>,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub terminal_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SessionCommit {
    pub session_id: String,
    pub expected: StoreHead,
    pub identity: SessionIdentity,
    pub metadata: SessionMetadata,
    pub history: HistorySuffix,
    pub side_tables: SideTableSuffixes,
    pub transcript_records: Option<TranscriptRecordSuffix>,
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
pub struct TranscriptRecordSuffix {
    pub start: TranscriptRecordIndex,
    pub records: Vec<StoredTranscriptBlock>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StoreHead {
    pub revision: Revision,
    pub history_len: HistoryLen,
    pub transcript_record_count: TranscriptRecordCount,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SaveReceipt {
    pub session_id: String,
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
    InvalidTranscriptRecordSuffix {
        start: TranscriptRecordIndex,
        current_len: TranscriptRecordCount,
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
    InvalidTurn {
        message: String,
    },
    TurnNotFound {
        turn_id: TurnId,
    },
    InvalidTurnTransition {
        turn_id: TurnId,
        from: TurnState,
        to: TurnState,
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
    },
    Sqlite {
        message: String,
    },
}

impl SessionCommitFailure {
    pub fn is_recoverable_stale_base(&self) -> bool {
        matches!(self, Self::StaleBase { .. })
    }

    pub fn invalidates_connection(&self) -> bool {
        matches!(self, Self::Sqlite { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_failures_report_structural_facts_without_runtime_policy() {
        let stale = SessionCommitFailure::StaleBase {
            expected: StoreHead {
                revision: Revision::new(1),
                history_len: HistoryLen::new(2),
                transcript_record_count: TranscriptRecordCount::new(3),
            },
            current: StoreHead {
                revision: Revision::new(4),
                history_len: HistoryLen::new(5),
                transcript_record_count: TranscriptRecordCount::new(6),
            },
        };
        let sqlite = SessionCommitFailure::Sqlite {
            message: "connection failed".into(),
        };

        assert!(stale.is_recoverable_stale_base());
        assert!(!sqlite.is_recoverable_stale_base());
        assert!(sqlite.invalidates_connection());
        assert!(!stale.invalidates_connection());
    }

    #[test]
    fn typed_lengths_do_not_compare_across_domains() {
        let history_len = HistoryLen::new(7);
        let record_len = TranscriptRecordCount::new(7);

        assert_eq!(history_len.get(), record_len.get());
        assert_eq!(history_len.as_usize(), Some(7));
    }

    #[test]
    fn commit_failure_serializes_complete_stale_head() {
        let failure = SessionCommitFailure::StaleBase {
            expected: StoreHead {
                revision: Revision::new(301),
                history_len: HistoryLen::new(302),
                transcript_record_count: TranscriptRecordCount::new(303),
            },
            current: StoreHead {
                revision: Revision::new(109),
                history_len: HistoryLen::new(110),
                transcript_record_count: TranscriptRecordCount::new(111),
            },
        };

        let json = serde_json::to_value(&failure).expect("serialize failure");

        assert_eq!(
            json["StaleBase"]["expected"]["transcript_record_count"],
            303
        );
        assert_eq!(json["StaleBase"]["current"]["revision"], 109);
        assert_eq!(json["StaleBase"]["current"]["history_len"], 110);
        assert_eq!(json["StaleBase"]["current"]["transcript_record_count"], 111);
    }
}
