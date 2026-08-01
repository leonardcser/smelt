mod access;
mod blob_staging;
mod catalog;
mod compression;
mod db;
mod error;
mod history;
mod jsonl_export;
mod lineage;
mod lineage_access;
mod lineage_search;
mod meta;
mod object;
mod request_audit;
mod schema;
mod session_commit;

pub use access::{
    cleanup_abandoned_session_artifacts, inspect_session_schema, migrate_session_schema,
    quarantine_orphaned_session, session_schema_status, ArtifactCleanupReport,
    LegacyAttachmentBlob, OwnedSessionWriter, SessionMaintenance, SessionOrphanQuarantine,
    SessionReader, SessionSchemaInspection, SessionSchemaMigration, SessionSchemaStatus,
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
    backup_session_database, session_commit_fingerprint, submit_turn_fingerprint,
    turn_transition_fingerprint, DoctorReport, FullSession, SessionResumeSnapshot, StorageStats,
    StoredSession,
};
pub use error::{Result, StoreError};
pub use history::{
    StoredTranscriptBlock, TranscriptBlockMetadataRecord, TranscriptExtentChunk,
    TranscriptExtentProfile, TranscriptRecordHydration, TranscriptRecordOffset,
    TranscriptRecordRange, TranscriptRecordSlice, TranscriptSearchCandidate,
    TranscriptSearchDirection, TRANSCRIPT_EXTENT_CHUNK_RECORDS, TRANSCRIPT_EXTENT_PROFILE_WIDTHS,
};
pub use lineage_access::{
    cleanup_abandoned_lineage_artifacts, lineage_session_ids, migrate_legacy_session,
    verify_lineage_backup, LineageReclamation, LineageSessionReader, LineageSessionState,
    LineageVacuum, OwnedLineageWriter,
};
pub use lineage_search::{
    LineageSearchProjector, SearchProjectionState, SearchProjectionStatus, SEARCH_FORMAT_VERSION,
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
