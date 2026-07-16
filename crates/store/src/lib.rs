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
mod session_snapshot;

pub use access::{LegacyAttachmentBlob, OwnedSessionWriter, SessionMaintenance, SessionReader};
pub use compression::{
    benchmark_zstd_compression, CompressionReport, CompressionSample, ObjectCompression,
    DEFAULT_ZSTD_LEVEL, DEFAULT_ZSTD_MIN_BYTES, DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
};
#[cfg(any(test, feature = "test-util"))]
pub use db::SessionDb;
pub use db::{DoctorReport, SessionResumeSnapshot, StorageStats};
pub use error::{Result, StoreError};
pub use history::{
    TranscriptBlockMetadataRecord, TranscriptDescriptorHydration, TranscriptDescriptorIndex,
    TranscriptDescriptorRange, TranscriptDescriptorRecord, TranscriptDescriptorSlice,
    TranscriptSearchCandidate, TranscriptSearchDirection,
};
pub use meta::{SessionMeta, SessionState, WriterOwner};
pub use object::{ObjectCodec, ObjectMeta, StoredObject, MAX_OBJECT_RAW_SIZE};
pub use request_audit::{
    RequestAuditOrder, RequestAuditPayloadMode, RequestAuditPayloads, RequestAuditQuery,
    RequestAuditStats, RequestAuditSummary,
};
pub use schema::SCHEMA_VERSION;
pub use session_commit::{
    DescriptorIndex, DescriptorLen, HistoryIndex, HistoryIndexBound, HistoryLen, HistorySuffix,
    Revision, SaveId, SaveReceipt, SessionCommit, SessionCommitFailure, SideTableSuffixes,
    StoreHead, TranscriptDescriptorSuffix,
};
pub use session_snapshot::{SessionSaveReport, SessionSnapshot};
