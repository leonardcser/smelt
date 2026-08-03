use crate::history::{StoredTranscriptBlock, TranscriptRecordSlice};
use crate::meta::{SessionIdentity, SessionMetadata};
use crate::session_commit::StoreHead;

#[derive(Clone, Debug, PartialEq)]
pub struct StoredSession {
    pub identity: SessionIdentity,
    pub metadata: SessionMetadata,
    pub head: StoreHead,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FullSession {
    pub session: StoredSession,
    pub history: Vec<protocol::HistoryItem>,
    pub turn_metas: Vec<(u64, serde_json::Value)>,
    pub metadata_snapshots: Vec<(u64, serde_json::Value)>,
    pub context_snapshots: Vec<(u64, serde_json::Value)>,
    pub transcript_records: Vec<StoredTranscriptBlock>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionResumeSnapshot {
    pub session: StoredSession,
    pub retained_history_len: usize,
    pub history_text_bytes: u64,
    pub missing_object_references: Vec<String>,
    pub transcript_record_tail: TranscriptRecordSlice,
}
