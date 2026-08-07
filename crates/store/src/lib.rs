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
mod snapshot;

pub use catalog::{
    archive_corrupt_catalog, catalog_reconcile_lock_path, catalog_session_pending_token,
    clear_catalog_session_pending, pending_catalog_session_ids, rebuild_catalog, Catalog,
    CatalogAvailability, CatalogCursor, CatalogMetadata, CatalogPage, CatalogQuery, CatalogReader,
    CatalogReconcileLock, CatalogSession, CATALOG_SCHEMA_VERSION, MAX_CATALOG_PAGE_SIZE,
};
pub use compression::{
    benchmark_zstd_compression, CompressionReport, CompressionSample, ObjectCompression,
    DEFAULT_ZSTD_LEVEL, DEFAULT_ZSTD_MIN_BYTES, DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
};
pub use diagnostics::{DoctorReport, StorageStats};
pub use error::{Result, StoreError};
pub use history::{
    StoredTranscriptBlock, TranscriptBlockMetadataRecord, TranscriptExtentChunk,
    TranscriptExtentProfile, TranscriptRecordHydration, TranscriptRecordOffset,
    TranscriptRecordRange, TranscriptRecordSlice, TranscriptSearchCandidate,
    TranscriptSearchDirection, TRANSCRIPT_EXTENT_CHUNK_RECORDS, TRANSCRIPT_EXTENT_PROFILE_WIDTHS,
};
pub use lineage_access::{
    cleanup_abandoned_lineage_artifacts, lineage_session_ids, verify_lineage_backup,
    LineageReclamation, LineageSessionReader, LineageSessionState, LineageVacuum,
    OwnedLineageWriter,
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
pub use snapshot::{FullSession, SessionResumeSnapshot, StoredSession};
