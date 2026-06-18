mod compression;
mod db;
mod error;
mod history;
mod legacy;
mod meta;
mod object;
mod request_audit;
mod schema;
mod session_snapshot;

pub use compression::{
    benchmark_zstd_compression, CompressionReport, CompressionSample, ObjectCompression,
    DEFAULT_ZSTD_LEVEL, DEFAULT_ZSTD_MIN_BYTES, DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
};
pub use db::{OpenMode, OpenOptions, SessionDb};
pub use error::{Result, StoreError};
pub use history::{
    TranscriptDescriptorHydration, TranscriptDescriptorIndex, TranscriptDescriptorRange,
    TranscriptDescriptorRecord, TranscriptDescriptorSlice, TranscriptSearchCandidate,
};
pub use legacy::{LegacyImportReport, RequestAttemptSummary};
pub use meta::{SessionMeta, SessionState, WriterLease};
pub use object::{ObjectCodec, ObjectMeta, StoredObject};
pub use request_audit::{
    RequestAuditOrder, RequestAuditPayloads, RequestAuditQuery, RequestAuditSummary,
};
pub use schema::SCHEMA_VERSION;
pub use session_snapshot::{SessionSaveReport, SessionSnapshot};
