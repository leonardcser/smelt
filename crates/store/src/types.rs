use protocol::HistoryItem;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::transcript::StoredTranscriptBlock;

macro_rules! typed_u64 {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            Eq,
            Hash,
            Ord,
            PartialEq,
            PartialOrd,
            Deserialize,
            Serialize,
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

            pub fn as_usize(self) -> usize {
                usize::try_from(self.0).unwrap_or(usize::MAX)
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl From<usize> for $name {
            fn from(value: usize) -> Self {
                Self(value as u64)
            }
        }
    };
}

typed_u64!(HistoryLen);
typed_u64!(TranscriptRecordCount);

/// Stored lengths of a session's sequences.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct StoreHead {
    pub history_len: HistoryLen,
    pub transcript_record_count: TranscriptRecordCount,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionIdentity {
    pub id: String,
    pub created_at: i64,
    pub parent_id: Option<String>,
}

/// Scalar session fields, rewritten by every save.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SessionMetadata {
    pub title: Option<String>,
    pub slug: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    pub fast_mode: Option<bool>,
    pub accounting: Option<Value>,
    pub context_tokens: Option<u64>,
    pub context_tokens_history_len: Option<u64>,
    pub display_context_tokens: Option<u64>,
    pub cost_usd: f64,
    /// The id of the newest turn, so ids keep growing across resumes.
    pub last_turn_id: u64,
    pub updated_at: i64,
}

/// History items from `start` on replace the stored ones.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct HistorySuffix {
    pub start: u64,
    pub items: Vec<HistoryItem>,
}

/// Rows keyed at or after `start` replace the stored ones.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct SideRows {
    pub start: u64,
    pub rows: Vec<(u64, Value)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SideTable {
    TurnMetas = 0,
    MetadataSnapshots = 1,
    ContextSnapshots = 2,
    CheckpointEvents = 3,
}

impl SideTable {
    pub const ALL: [Self; 4] = [
        Self::TurnMetas,
        Self::MetadataSnapshots,
        Self::ContextSnapshots,
        Self::CheckpointEvents,
    ];
}

/// Transcript records from `start` on replace the stored ones.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct TranscriptRecordSuffix {
    pub start: u64,
    pub records: Vec<StoredTranscriptBlock>,
}

/// One atomic save. `None` leaves a part as stored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionCommit {
    pub metadata: SessionMetadata,
    pub first_user_message: Option<Option<String>>,
    pub checkpoint: Option<Option<Value>>,
    pub history: Option<HistorySuffix>,
    pub turn_metas: Option<SideRows>,
    pub metadata_snapshots: Option<SideRows>,
    pub context_snapshots: Option<SideRows>,
    pub checkpoint_events: Option<SideRows>,
    pub transcript: Option<TranscriptRecordSuffix>,
}

impl SessionCommit {
    pub(crate) fn side_rows(&self, table: SideTable) -> Option<&SideRows> {
        match table {
            SideTable::TurnMetas => self.turn_metas.as_ref(),
            SideTable::MetadataSnapshots => self.metadata_snapshots.as_ref(),
            SideTable::ContextSnapshots => self.context_snapshots.as_ref(),
            SideTable::CheckpointEvents => self.checkpoint_events.as_ref(),
        }
    }
}

/// Everything about a session except its sequences, read in one snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionState {
    pub identity: SessionIdentity,
    pub metadata: SessionMetadata,
    pub first_user_message: Option<String>,
    pub checkpoint: Option<Value>,
    pub head: StoreHead,
    pub history_text_bytes: u64,
    pub turn_metas: Vec<(u64, Value)>,
    pub metadata_snapshots: Vec<(u64, Value)>,
    pub context_snapshots: Vec<(u64, Value)>,
    pub checkpoint_events: Vec<(u64, Value)>,
}
