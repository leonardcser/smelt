mod catalog;
mod compression;
mod diagnostics;
mod error;
mod filesystem;
mod history;
mod jsonl_export;
mod lineage;
mod lineage_access;
mod lineage_search;
mod meta;
mod object;
mod request_audit;
mod schema;
mod session_command;
mod session_commit;
mod session_writer;
mod snapshot;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStoreLayout {
    sessions_root: std::path::PathBuf,
}

impl SessionStoreLayout {
    pub fn from_sessions_root(sessions_root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            sessions_root: sessions_root.into(),
        }
    }

    pub fn from_state_root(state_root: impl AsRef<std::path::Path>) -> Self {
        Self::from_sessions_root(state_root.as_ref().join("sessions"))
    }

    pub fn sessions_root(&self) -> &std::path::Path {
        &self.sessions_root
    }

    pub fn catalog_path(&self) -> std::path::PathBuf {
        self.sessions_root.join("catalog.db")
    }

    pub fn catalog_pending_dir(&self) -> std::path::PathBuf {
        self.sessions_root.join(".catalog-pending")
    }

    pub fn catalog_pending_path(&self, session_id: &str) -> std::path::PathBuf {
        self.catalog_pending_dir().join(session_id)
    }

    pub fn locks_dir(&self) -> std::path::PathBuf {
        self.sessions_root.join(".locks")
    }

    pub fn journals_dir(&self) -> std::path::PathBuf {
        self.sessions_root.join(".journals")
    }

    pub fn session_journal_path(&self, session_id: &str) -> std::path::PathBuf {
        self.journals_dir().join(format!("{session_id}.jsonl"))
    }

    pub fn lineage_lock_path(&self, lineage_id: &str) -> std::path::PathBuf {
        self.locks_dir().join(format!("{lineage_id}.lock"))
    }

    pub fn catalog_marker_lock_path(&self, session_id: &str) -> std::path::PathBuf {
        self.locks_dir()
            .join(format!("{session_id}.catalog-marker.lock"))
    }

    pub fn trash_dir(&self) -> std::path::PathBuf {
        self.sessions_root.join(".trash")
    }

    pub fn artifacts_dir(&self) -> std::path::PathBuf {
        self.sessions_root.join(".artifacts")
    }

    pub fn session_artifact_dir(&self, session_id: &str) -> std::path::PathBuf {
        self.artifacts_dir().join(session_id)
    }

    pub fn lineage_dir(&self, lineage_id: &str) -> std::path::PathBuf {
        self.sessions_root.join(lineage_id)
    }

    pub fn lineage_database_path(&self, lineage_id: &str) -> std::path::PathBuf {
        self.lineage_dir(lineage_id).join("lineage.db")
    }

    pub fn lineage_search_path(&self, lineage_id: &str) -> std::path::PathBuf {
        self.lineage_dir(lineage_id).join("search.db")
    }

    pub fn staging_lineage_dir(&self, lineage_id: &str) -> std::path::PathBuf {
        self.sessions_root.join(format!(".staging-{lineage_id}"))
    }

    pub fn staging_lineage_database_path(&self, lineage_id: &str) -> std::path::PathBuf {
        self.staging_lineage_dir(lineage_id).join("lineage.db")
    }
}

pub use catalog::{
    catalog_session_pending_token, clear_catalog_session_pending, pending_catalog_session_ids,
    Catalog, CatalogAvailability, CatalogCursor, CatalogMarkerLock, CatalogMetadata, CatalogPage,
    CatalogQuery, CatalogReader, CatalogReconciliation, CatalogSession, CATALOG_SCHEMA_VERSION,
    MAX_CATALOG_PAGE_SIZE,
};
pub use compression::{
    benchmark_zstd_compression, CompressionReport, CompressionSample, ObjectCompression,
    DEFAULT_ZSTD_LEVEL, DEFAULT_ZSTD_MIN_BYTES, DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
};
pub use diagnostics::{DoctorReport, StorageStats};
pub use error::{Result, StoreError};
pub use history::{
    StoredTranscriptBlock, TranscriptBlockMetadataRecord, TranscriptExtentProfile,
    TranscriptNavigationRecord, TranscriptRecordHydration, TranscriptRecordOffset,
    TranscriptRecordProfile, TranscriptRecordRange, TranscriptRecordSlice, TranscriptRowLocation,
    TranscriptSearchCandidate, TranscriptSearchDirection, TRANSCRIPT_EXTENT_PROFILE_WIDTHS,
};
pub use lineage_access::{
    cleanup_abandoned_lineages, lineage_session_ids, lineage_session_locations,
    verify_lineage_backup, LineageReclamation, LineageSessionLocation, LineageSessionReader,
    LineageSessionState, LineageVacuum, OwnedLineageWriter,
};
pub use lineage_search::{
    LineageSearchProjector, SearchProjectionState, SearchProjectionStatus, SEARCH_FORMAT_VERSION,
};
pub use meta::{SessionCostUsd, SessionIdentity, SessionMetadata};
pub use object::{ObjectCodec, ObjectMeta, StoredObject, MAX_OBJECT_RAW_SIZE};
pub use request_audit::{
    RequestAuditOrder, RequestAuditPayloadMode, RequestAuditPayloads, RequestAuditQuery,
    RequestAuditStats, RequestAuditSummary,
};
pub use schema::LINEAGE_SCHEMA_VERSION;
pub use session_command::{
    session_commit_fingerprint, submit_turn_fingerprint, turn_transition_fingerprint,
};
pub use session_commit::{
    HistoryIndex, HistoryIndexBound, HistoryLen, HistorySuffix, NewTurn, Revision, SaveReceipt,
    SessionCommit, SessionCommitFailure, SideTableSuffixes, StartupRecoveryReceipt, StoreHead,
    StoredTurn, SubmitTurn, SubmitTurnReceipt, TranscriptRecordCount, TranscriptRecordIndex,
    TranscriptRecordSuffix, TurnId, TurnKind, TurnState, TurnTransition, TurnTransitionReceipt,
};
pub use session_writer::{
    SessionBatchBarrier, SessionEventBatch, SessionEventCommand, SessionEventReceipt,
    SessionJournalRecovery, SessionWriter,
};
pub use snapshot::{FullSession, SessionResumeSnapshot, StoredSession};
