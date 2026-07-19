mod access;
mod blob_staging;
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
pub use compression::{
    benchmark_zstd_compression, CompressionReport, CompressionSample, ObjectCompression,
    DEFAULT_ZSTD_LEVEL, DEFAULT_ZSTD_MIN_BYTES, DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
};
#[cfg(any(test, feature = "test-util"))]
pub use db::SessionDb;
pub use db::{
    session_commit_fingerprint, DoctorReport, FullSession, SessionResumeSnapshot, StorageStats,
    StoredSession,
};
pub use error::{Result, StoreError};
pub use history::{
    TranscriptBlockMetadataRecord, TranscriptDescriptorHydration, TranscriptDescriptorIndex,
    TranscriptDescriptorRange, TranscriptDescriptorRecord, TranscriptDescriptorSlice,
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
    DescriptorIndex, DescriptorLen, HistoryIndex, HistoryIndexBound, HistoryLen, HistorySuffix,
    Revision, SaveReceipt, SessionCommit, SessionCommitFailure, SideTableSuffixes, StoreHead,
    TranscriptDescriptorSuffix,
};
