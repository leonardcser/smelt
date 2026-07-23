mod access;
mod blob_staging;
mod catalog;
mod compression;
mod db;
mod error;
mod history;
mod jsonl_export;
mod meta;
mod object;
mod request_audit;
mod schema;
mod session_commit;

pub use access::{
    cleanup_abandoned_session_artifacts, ArtifactCleanupReport, LegacyAttachmentBlob,
    OwnedSessionWriter, SessionMaintenance, SessionReader,
};
pub use catalog::{
    archive_corrupt_catalog, rebuild_catalog, Catalog, CatalogAvailability, CatalogCursor,
    CatalogMetadata, CatalogPage, CatalogQuery, CatalogReader, CatalogReconcileLock,
    CatalogSession, CATALOG_SCHEMA_VERSION, MAX_CATALOG_PAGE_SIZE,
};
pub use compression::{
    benchmark_zstd_compression, CompressionReport, CompressionSample, ObjectCompression,
    DEFAULT_ZSTD_LEVEL, DEFAULT_ZSTD_MIN_BYTES, DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
};
#[cfg(any(test, feature = "test-util"))]
pub use db::SessionDb;
pub use db::{
    session_commit_fingerprint, submit_turn_fingerprint, DoctorReport, FullSession,
    SessionResumeSnapshot, StorageStats, StoredSession,
};
pub use error::{Result, StoreError};
pub use history::{
    StoredTranscriptBlock, TranscriptBlockMetadataRecord, TranscriptRecordHydration,
    TranscriptRecordOffset, TranscriptRecordRange, TranscriptRecordSlice,
    TranscriptSearchCandidate, TranscriptSearchDirection,
};
pub use meta::{SessionCostUsd, SessionIdentity, SessionMeta, SessionMetadata, WriterOwner};
pub use object::{ObjectCodec, ObjectMeta, StoredObject, MAX_OBJECT_RAW_SIZE};
pub use request_audit::{
    RequestAuditOrder, RequestAuditPayloadMode, RequestAuditPayloads, RequestAuditQuery,
    RequestAuditStats, RequestAuditSummary,
};
pub use schema::SCHEMA_VERSION;
pub use session_commit::{
    HistoryIndex, HistoryIndexBound, HistoryLen, HistorySuffix, NewTurn, Revision, SaveReceipt,
    SessionCommit, SessionCommitFailure, SideTableSuffixes, StartupRecoveryReceipt, StoreHead,
    StoredTurn, SubmitTurn, SubmitTurnReceipt, TranscriptRecordCount, TranscriptRecordIndex,
    TranscriptRecordSuffix, TurnId, TurnKind, TurnState, TurnTransition, TurnTransitionReceipt,
};
