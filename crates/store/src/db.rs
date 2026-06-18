use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history::{
    self, TranscriptDescriptorRange, TranscriptDescriptorRecord, TranscriptDescriptorSlice,
    TranscriptSearchCandidate,
};
use crate::legacy::{self, LegacyImportReport, RequestAttemptSummary};
use crate::meta::{self, SessionMeta, SessionState, WriterLease};
use crate::object::{self, ObjectMeta, StoredObject};
use crate::request_audit::{self, RequestAuditPayloads, RequestAuditQuery, RequestAuditSummary};
use crate::schema;
use crate::session_snapshot::{self, SessionHistorySuffix, SessionSaveReport, SessionSnapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenMode {
    ReadWrite,
    ReadOnly,
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    pub mode: OpenMode,
    pub app_version: String,
    pub object_compression: ObjectCompression,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            mode: OpenMode::ReadWrite,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            object_compression: ObjectCompression::default(),
        }
    }
}

#[derive(Debug)]
pub struct SessionDb {
    conn: Connection,
    path: PathBuf,
    mode: OpenMode,
    object_compression: ObjectCompression,
}

impl SessionDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, OpenOptions::default())
    }

    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(
            path,
            OpenOptions {
                mode: OpenMode::ReadOnly,
                ..OpenOptions::default()
            },
        )
    }

    pub fn open_with_options(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self> {
        let _perf = smelt_perf::perf::begin(match options.mode {
            OpenMode::ReadWrite => "store:db:open_read_write",
            OpenMode::ReadOnly => "store:db:open_read_only",
        });
        let path = path.as_ref().to_path_buf();
        if matches!(options.mode, OpenMode::ReadWrite) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
        }

        let flags = match options.mode {
            OpenMode::ReadWrite => {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            }
            OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        };
        let mut conn = Connection::open_with_flags(&path, flags)?;
        apply_pragmas(&conn, options.mode)?;

        match options.mode {
            OpenMode::ReadWrite => schema::migrate(&mut conn, &options.app_version)?,
            OpenMode::ReadOnly => schema::validate_read_only_schema(&conn)?,
        }

        Ok(Self {
            conn,
            path,
            mode: options.mode,
            object_compression: options.object_compression,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn mode(&self) -> OpenMode {
        self.mode
    }

    pub fn object_compression(&self) -> ObjectCompression {
        self.object_compression
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn immediate_transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = f(&self.conn);
        match result {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

    pub fn user_version(&self) -> Result<i32> {
        schema::user_version(&self.conn)
    }

    pub fn quick_check(&self) -> Result<()> {
        let result: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if result == "ok" {
            Ok(())
        } else {
            Err(StoreError::Integrity(format!(
                "sqlite quick_check failed: {result}"
            )))
        }
    }

    pub fn schema_version(&self) -> Result<i32> {
        self.user_version()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        meta::set_meta(&self.conn, key, value)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        meta::meta(&self.conn, key)
    }

    pub fn set_writer_lease(&self, lease: &WriterLease) -> Result<()> {
        meta::set_writer_lease(&self.conn, lease)
    }

    pub fn writer_lease(&self) -> Result<Option<WriterLease>> {
        meta::writer_lease(&self.conn)
    }

    pub fn clear_writer_lease(&self) -> Result<()> {
        meta::clear_writer_lease(&self.conn)
    }

    pub fn upsert_session_state(&self, state: &SessionState) -> Result<()> {
        meta::upsert_session_state(&self.conn, state)
    }

    pub fn session_state(&self) -> Result<Option<SessionState>> {
        meta::session_state(&self.conn)
    }

    pub fn session_meta(&self) -> Result<Option<SessionMeta>> {
        meta::session_meta(&self.conn)
    }

    pub fn write_meta_sidecar(&self, path: impl AsRef<Path>) -> Result<Option<SessionMeta>> {
        meta::write_meta_sidecar(&self.conn, path)
    }

    pub fn put_object(&self, kind: &str, bytes: &[u8]) -> Result<StoredObject> {
        object::put_object(&self.conn, kind, bytes, self.object_compression)
    }

    pub fn put_object_uncompressed(&self, kind: &str, bytes: &[u8]) -> Result<StoredObject> {
        object::put_object(&self.conn, kind, bytes, ObjectCompression::none())
    }

    pub fn put_object_with_compression(
        &self,
        kind: &str,
        bytes: &[u8],
        compression: ObjectCompression,
    ) -> Result<StoredObject> {
        object::put_object(&self.conn, kind, bytes, compression)
    }

    pub fn object(&self, hash: &str) -> Result<Option<StoredObject>> {
        object::object(&self.conn, hash)
    }

    pub fn object_meta(&self, hash: &str) -> Result<Option<ObjectMeta>> {
        object::object_meta(&self.conn, hash)
    }

    pub fn object_bytes(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let Some(meta) = self.object_meta(hash)? else {
            return Ok(None);
        };
        object::object_bytes(&self.conn, &meta).map(Some)
    }

    pub fn import_legacy_session_dir(
        &self,
        session_dir: impl AsRef<Path>,
    ) -> Result<LegacyImportReport> {
        legacy::import_session_dir(&self.conn, session_dir.as_ref(), self.object_compression)
    }

    pub fn import_legacy_requests_jsonl(&self, session_dir: impl AsRef<Path>) -> Result<usize> {
        legacy::import_requests_jsonl(&self.conn, session_dir.as_ref(), self.object_compression)
    }

    pub fn export_history_jsonl(&self, out: impl Write) -> Result<()> {
        legacy::export_history_jsonl(&self.conn, out)
    }

    pub fn export_requests_jsonl(&self, out: impl Write) -> Result<()> {
        legacy::export_requests_jsonl(&self.conn, out)
    }

    pub fn request_attempts(&self) -> Result<Vec<RequestAttemptSummary>> {
        legacy::request_attempts(&self.conn)
    }

    pub fn append_request_attempt(
        &self,
        entry: &protocol::request_log::RequestLogEntry,
    ) -> Result<i64> {
        request_audit::append_request_attempt(&self.conn, entry, self.object_compression)
    }

    pub fn query_request_attempts(
        &self,
        query: &RequestAuditQuery,
    ) -> Result<Vec<RequestAuditSummary>> {
        request_audit::request_attempts(&self.conn, query)
    }

    pub fn request_payloads(
        &self,
        request_attempt_id: i64,
    ) -> Result<Option<RequestAuditPayloads>> {
        request_audit::request_payloads(&self.conn, request_attempt_id)
    }

    pub fn save_session_snapshot(
        &self,
        snapshot: &SessionSnapshot,
        expected_revision: Option<u64>,
    ) -> Result<SessionSaveReport> {
        session_snapshot::save_session_snapshot(
            &self.conn,
            snapshot,
            expected_revision,
            None,
            self.object_compression,
        )
    }

    pub fn save_session_snapshot_for_import(
        &self,
        snapshot: &SessionSnapshot,
    ) -> Result<SessionSaveReport> {
        session_snapshot::save_session_snapshot(
            &self.conn,
            snapshot,
            Some(0),
            None,
            self.object_compression,
        )
    }

    pub fn save_session_snapshot_as_writer(
        &self,
        snapshot: &SessionSnapshot,
    ) -> Result<SessionSaveReport> {
        let lease = self.current_process_writer_lease()?;
        let expected_revision = self
            .session_state()?
            .as_ref()
            .map_or(0, |state| state.revision);
        session_snapshot::save_session_snapshot(
            &self.conn,
            snapshot,
            Some(expected_revision),
            Some(&lease),
            self.object_compression,
        )
    }

    pub fn save_session_snapshot_and_transcript_descriptor_suffix_as_writer(
        &self,
        snapshot: &SessionSnapshot,
        start_descriptor_idx: usize,
        records: &[TranscriptDescriptorRecord],
    ) -> Result<SessionSaveReport> {
        let lease = self.current_process_writer_lease()?;
        let expected_revision = self
            .session_state()?
            .as_ref()
            .map_or(0, |state| state.revision);
        self.immediate_transaction(|conn| {
            let report = session_snapshot::save_session_snapshot_in_transaction(
                conn,
                snapshot,
                Some(expected_revision),
                Some(&lease),
                self.object_compression,
            )?;
            history::replace_transcript_descriptor_suffix_in_transaction(
                conn,
                start_descriptor_idx,
                records,
                self.object_compression,
            )?;
            Ok(report)
        })
    }

    pub fn save_history_suffix_and_transcript_descriptor_suffix_as_writer(
        &self,
        suffix: &SessionHistorySuffix,
        start_descriptor_idx: usize,
        records: &[TranscriptDescriptorRecord],
    ) -> Result<SessionSaveReport> {
        let lease = self.current_process_writer_lease()?;
        let expected_revision = self
            .session_state()?
            .as_ref()
            .map_or(0, |state| state.revision);
        self.immediate_transaction(|conn| {
            let report = session_snapshot::save_session_history_suffix_in_transaction(
                conn,
                suffix,
                Some(expected_revision),
                Some(&lease),
                self.object_compression,
            )?;
            history::replace_transcript_descriptor_suffix_in_transaction(
                conn,
                start_descriptor_idx,
                records,
                self.object_compression,
            )?;
            Ok(report)
        })
    }

    pub fn load_session_snapshot(&self) -> Result<Option<SessionSnapshot>> {
        session_snapshot::load_session_snapshot(&self.conn)
    }

    pub fn replace_transcript_descriptor_records(
        &self,
        records: &[TranscriptDescriptorRecord],
    ) -> Result<()> {
        history::replace_transcript_descriptor_records(&self.conn, records, self.object_compression)
    }

    pub fn replace_transcript_descriptor_suffix(
        &self,
        start_descriptor_idx: usize,
        records: &[TranscriptDescriptorRecord],
    ) -> Result<()> {
        history::replace_transcript_descriptor_suffix(
            &self.conn,
            start_descriptor_idx,
            records,
            self.object_compression,
        )
    }

    pub fn transcript_descriptor_count(&self) -> Result<usize> {
        history::transcript_descriptor_count(&self.conn)
    }

    /// Fast descriptor extent for dense transcript tables written by the current store.
    /// Use `transcript_descriptor_count` when sparse or synthetic block indices must be counted exactly.
    pub fn transcript_descriptor_dense_extent(&self) -> Result<usize> {
        history::transcript_descriptor_dense_extent(&self.conn)
    }

    pub fn read_transcript_descriptor_records(&self) -> Result<Vec<TranscriptDescriptorRecord>> {
        history::read_transcript_descriptor_records(&self.conn)
    }

    pub fn read_transcript_descriptor_slice(
        &self,
        range: TranscriptDescriptorRange,
    ) -> Result<TranscriptDescriptorSlice> {
        history::read_transcript_descriptor_slice(&self.conn, range)
    }

    pub fn read_transcript_descriptor_slice_with_total(
        &self,
        range: TranscriptDescriptorRange,
        total_count: usize,
    ) -> Result<TranscriptDescriptorSlice> {
        history::read_transcript_descriptor_slice_with_total(&self.conn, range, total_count)
    }

    pub fn read_transcript_descriptor_tail_slice(
        &self,
        count: usize,
    ) -> Result<TranscriptDescriptorSlice> {
        history::read_transcript_descriptor_tail_slice(&self.conn, count)
    }

    pub fn read_transcript_descriptor_tail_slice_with_total(
        &self,
        total_count: usize,
        count: usize,
    ) -> Result<TranscriptDescriptorSlice> {
        history::read_transcript_descriptor_tail_slice_with_total(&self.conn, total_count, count)
    }

    pub fn search_transcript_candidates(
        &self,
        query: &str,
    ) -> Result<Vec<TranscriptSearchCandidate>> {
        history::search_transcript_candidates(&self.conn, query)
    }

    pub fn search_transcript_candidate_page(
        &self,
        query: &str,
        origin_block_idx: Option<u64>,
        direction: crate::TranscriptSearchDirection,
        limit: usize,
    ) -> Result<Vec<TranscriptSearchCandidate>> {
        history::search_transcript_candidate_page(
            &self.conn,
            query,
            origin_block_idx,
            direction,
            limit,
        )
    }

    pub fn read_history_items_range(
        &self,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<protocol::HistoryItem>> {
        history::read_history_items_range(&self.conn, range)
    }

    pub fn history_text_bytes(&self) -> Result<u64> {
        session_snapshot::history_text_bytes(&self.conn)
    }

    pub fn search_blob(&self) -> Result<String> {
        session_snapshot::search_blob(&self.conn)
    }
    fn current_process_writer_lease(&self) -> Result<WriterLease> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let hostname = local_hostname();
        let pid = std::process::id();
        let owner_id = format!("{hostname}:{pid}");
        let started_at = self
            .writer_lease()?
            .filter(|lease| lease.owner_id == owner_id)
            .map_or(now, |lease| lease.started_at);
        Ok(WriterLease {
            owner_id,
            hostname,
            pid,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            started_at,
            heartbeat_at: now,
        })
    }
}

fn local_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

fn apply_pragmas(conn: &Connection, mode: OpenMode) -> Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;",
    )?;
    match mode {
        OpenMode::ReadWrite => conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?,
        OpenMode::ReadOnly => conn.execute_batch("PRAGMA query_only = ON;")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::session_snapshot::SessionSnapshotTableSuffixes;
    use crate::{
        benchmark_zstd_compression, ObjectCodec, RequestAuditOrder, DEFAULT_ZSTD_LEVEL,
        DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
    };

    #[test]
    fn creates_and_reopens_session_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");

        let db = SessionDb::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), schema::SCHEMA_VERSION);
        db.quick_check().unwrap();
        drop(db);

        let db = SessionDb::open_read_only(&path).unwrap();
        assert_eq!(db.mode(), OpenMode::ReadOnly);
        assert_eq!(db.schema_version().unwrap(), schema::SCHEMA_VERSION);
        db.quick_check().unwrap();
    }

    #[test]
    fn transcript_descriptor_suffix_preserves_prefix_and_replaces_tail() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial = vec![
            transcript_record(0, "zero", "old zero"),
            transcript_record(1, "one", "old one"),
            transcript_record(2, "two", "old two"),
        ];
        db.replace_transcript_descriptor_records(&initial).unwrap();

        let replacement = vec![
            transcript_record(1, "one-new", "updated one"),
            transcript_record(2, "two-new", "updated two"),
        ];
        db.replace_transcript_descriptor_suffix(1, &replacement)
            .unwrap();

        let records = db.read_transcript_descriptor_records().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0], initial[0]);
        assert_eq!(records[1], replacement[0]);
        assert_eq!(records[2], replacement[1]);
        assert_eq!(db.search_transcript_candidates("old two").unwrap(), vec![]);
        assert_eq!(
            db.search_transcript_candidates("updated two").unwrap(),
            vec![TranscriptSearchCandidate {
                block_idx: 2,
                history_idx: None,
            }]
        );

        db.replace_transcript_descriptor_suffix(2, &[]).unwrap();
        let records = db.read_transcript_descriptor_records().unwrap();
        assert_eq!(records, vec![initial[0].clone(), replacement[0].clone()]);
        assert_eq!(
            db.search_transcript_candidates("updated two").unwrap(),
            vec![]
        );
    }

    #[test]
    fn history_suffix_write_appends_history_and_descriptors_transactionally() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial_history = vec![
            protocol::HistoryItem::user(protocol::Content::text("old user")),
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text("old assistant")),
                None,
                Vec::new(),
            )),
        ];
        let initial_snapshot = SessionSnapshot {
            state: test_session_state("typed-suffix", initial_history.len()),
            meta_json: None,
            history_start_idx: 0,
            history_len: initial_history.len(),
            history: initial_history.clone(),
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            accounting_snapshots: Vec::new(),
        };
        db.save_session_snapshot_for_import(&initial_snapshot)
            .unwrap();
        let initial_descriptors = vec![
            transcript_record_with_history(0, 0, "old-user", "old user"),
            transcript_record_with_history(1, 1, "old-assistant", "old assistant"),
        ];
        db.replace_transcript_descriptor_records(&initial_descriptors)
            .unwrap();

        let appended = protocol::HistoryItem::user(protocol::Content::text("new user"));
        let suffix = SessionHistorySuffix {
            state: test_session_state("typed-suffix", 3),
            history_start_idx: 2,
            history_len: 3,
            history: vec![appended.clone()],
            snapshot_tables: None,
        };
        let appended_descriptor = transcript_record_with_history(2, 2, "new-user", "new user");
        let report = db
            .save_history_suffix_and_transcript_descriptor_suffix_as_writer(
                &suffix,
                2,
                std::slice::from_ref(&appended_descriptor),
            )
            .unwrap();

        assert_eq!(report.history_deleted, 0);
        assert_eq!(report.history_inserted, 1);
        assert_eq!(report.history_unchanged, 2);
        assert_eq!(
            db.read_history_items_range(0..3).unwrap(),
            vec![
                initial_history[0].clone(),
                initial_history[1].clone(),
                appended
            ]
        );
        assert_eq!(
            db.read_transcript_descriptor_records().unwrap(),
            vec![
                initial_descriptors[0].clone(),
                initial_descriptors[1].clone(),
                appended_descriptor,
            ]
        );
        assert_eq!(db.session_state().unwrap().unwrap().history_len, 3);
    }

    #[test]
    fn history_suffix_write_syncs_requested_snapshot_table_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial_history = vec![
            protocol::HistoryItem::user(protocol::Content::text("old user")),
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text("old assistant")),
                None,
                Vec::new(),
            )),
        ];
        let initial_metadata = serde_json::json!({"first_user_message":"old user"});
        let initial_snapshot = SessionSnapshot {
            state: test_session_state("typed-side-tables", initial_history.len()),
            meta_json: None,
            history_start_idx: 0,
            history_len: initial_history.len(),
            history: initial_history,
            turn_metas: Vec::new(),
            metadata_snapshots: vec![(1, initial_metadata.clone())],
            accounting_snapshots: Vec::new(),
        };
        db.save_session_snapshot_for_import(&initial_snapshot)
            .unwrap();

        let appended = protocol::HistoryItem::user(protocol::Content::text("new user"));
        let appended_metadata = serde_json::json!({"first_user_message":"new user"});
        let suffix = SessionHistorySuffix {
            state: test_session_state("typed-side-tables", 3),
            history_start_idx: 2,
            history_len: 3,
            history: vec![appended],
            snapshot_tables: Some(SessionSnapshotTableSuffixes {
                start_idx: 2,
                turn_metas: Vec::new(),
                metadata_snapshots: vec![(3, appended_metadata.clone())],
                accounting_snapshots: Vec::new(),
            }),
        };
        db.save_history_suffix_and_transcript_descriptor_suffix_as_writer(&suffix, 0, &[])
            .unwrap();

        let snapshot = db
            .load_session_snapshot()
            .unwrap()
            .expect("session snapshot");
        assert_eq!(
            snapshot.metadata_snapshots,
            vec![(1, initial_metadata), (3, appended_metadata)]
        );
    }

    #[test]
    fn transcript_search_uses_indexed_short_utf8_and_paged_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = vec![
            transcript_record(0, "zero", "alpha café"),
            transcript_record(1, "one", "beta needle"),
            transcript_record(2, "two", "gamma café needle"),
            transcript_record(3, "three", "delta"),
        ];
        db.replace_transcript_descriptor_records(&records).unwrap();

        assert_eq!(
            db.search_transcript_candidates("é").unwrap(),
            vec![
                TranscriptSearchCandidate {
                    block_idx: 0,
                    history_idx: None,
                },
                TranscriptSearchCandidate {
                    block_idx: 2,
                    history_idx: None,
                },
            ]
        );
        assert_eq!(
            db.search_transcript_candidate_page(
                "needle",
                Some(2),
                crate::TranscriptSearchDirection::Forward,
                8,
            )
            .unwrap(),
            vec![TranscriptSearchCandidate {
                block_idx: 2,
                history_idx: None,
            }]
        );
        assert_eq!(
            db.search_transcript_candidate_page(
                "needle",
                Some(2),
                crate::TranscriptSearchDirection::Backward,
                8,
            )
            .unwrap(),
            vec![
                TranscriptSearchCandidate {
                    block_idx: 1,
                    history_idx: None,
                },
                TranscriptSearchCandidate {
                    block_idx: 2,
                    history_idx: None,
                },
            ]
        );
    }

    #[test]
    fn transcript_search_pages_past_false_positive_postings() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let mut records = (0..80)
            .map(|idx| transcript_record(idx, &format!("false-{idx}"), "abc false bcd"))
            .collect::<Vec<_>>();
        records.push(transcript_record(80, "true", "contains abcd exactly"));
        db.replace_transcript_descriptor_records(&records).unwrap();

        assert_eq!(
            db.search_transcript_candidate_page(
                "abcd",
                None,
                crate::TranscriptSearchDirection::Forward,
                1,
            )
            .unwrap(),
            vec![TranscriptSearchCandidate {
                block_idx: 80,
                history_idx: None,
            }]
        );
    }

    #[test]
    fn transcript_descriptor_range_and_tail_read_bounded_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = (0..6)
            .map(|idx| transcript_record(idx * 10, &format!("block-{idx}"), &format!("text {idx}")))
            .collect::<Vec<_>>();
        db.replace_transcript_descriptor_records(&records).unwrap();

        let slice = db.read_transcript_descriptor_slice((2..5).into()).unwrap();
        assert_eq!(slice.start.get(), 2);
        assert_eq!(slice.end().get(), 5);
        assert_eq!(slice.total_count, 6);
        assert_eq!(slice.records, records[2..5].to_vec());
        assert_eq!(
            slice.hydration,
            crate::TranscriptDescriptorHydration::ObjectBacked
        );

        let empty = db.read_transcript_descriptor_slice((4..4).into()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.start.get(), 4);
        assert_eq!(empty.total_count, 6);

        let tail = db.read_transcript_descriptor_tail_slice(2).unwrap();
        assert_eq!(tail.start.get(), 4);
        assert_eq!(tail.end().get(), 6);
        assert_eq!(tail.records, records[4..6].to_vec());
        assert!(db
            .read_transcript_descriptor_tail_slice(0)
            .unwrap()
            .is_empty());
        assert_eq!(
            db.read_transcript_descriptor_tail_slice(99)
                .unwrap()
                .records,
            records
        );
    }

    #[test]
    fn sparse_transcript_descriptor_reads_keep_metadata_object_backed() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let mut record = transcript_record(0, "tool", "tool text");
        let metadata_payload = "x".repeat(crate::history::METADATA_OBJECT_MIN_BYTES + 128);
        record.descriptor_json = serde_json::json!({
            "kind": "tool",
            "metadata": { "payload": metadata_payload },
        })
        .to_string();
        db.replace_transcript_descriptor_records(&[record]).unwrap();

        let full: serde_json::Value = serde_json::from_str(
            &db.read_transcript_descriptor_records().unwrap()[0].descriptor_json,
        )
        .unwrap();
        assert_eq!(
            full["metadata"]["payload"].as_str(),
            Some(metadata_payload.as_str())
        );

        let tail: serde_json::Value = serde_json::from_str(
            &db.read_transcript_descriptor_tail_slice(1).unwrap().records[0].descriptor_json,
        )
        .unwrap();
        assert!(tail["metadata"]
            .get(crate::history::OBJECT_REF_KEY)
            .is_some());
    }

    #[test]
    fn stores_objects_by_raw_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();

        let object = db.put_object("request_body", b"hello sqlite").unwrap();
        assert_eq!(object.codec(), ObjectCodec::None);
        assert_eq!(object.raw_size(), 12);
        assert_eq!(object.stored_size(), 12);
        assert_eq!(object.bytes, b"hello sqlite");

        let duplicate = db.put_object("request_body", b"hello sqlite").unwrap();
        assert_eq!(duplicate.hash(), object.hash());
        assert_eq!(db.object(object.hash()).unwrap().unwrap(), object);
    }

    #[test]
    fn object_meta_does_not_materialize_payload() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let object = db
            .put_object_with_compression(
                "metadata",
                &representative_metadata_payload(),
                ObjectCompression::zstd(1, 128, 15),
            )
            .unwrap();

        let meta = db.object_meta(object.hash()).unwrap().unwrap();
        assert_eq!(meta.hash, object.hash());
        assert_eq!(meta.kind, "metadata");
        assert_eq!(meta.codec, ObjectCodec::Zstd);
        assert_eq!(db.object_bytes(&meta.hash).unwrap().unwrap(), object.bytes);
    }

    #[cfg(unix)]
    #[test]
    fn writer_lease_allows_dead_same_host_pid() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let existing = WriterLease {
            owner_id: "host:4294967295".into(),
            hostname: "host".into(),
            pid: u32::MAX,
            app_version: "test".into(),
            started_at: 100,
            heartbeat_at: 100,
        };
        db.set_writer_lease(&existing).unwrap();

        let replacement = WriterLease {
            owner_id: "host:1".into(),
            hostname: "host".into(),
            pid: 1,
            app_version: "test".into(),
            started_at: 200,
            heartbeat_at: 200,
        };

        crate::meta::acquire_writer_lease(db.connection(), &replacement, 30 * 60).unwrap();
        assert_eq!(db.writer_lease().unwrap().unwrap().owner_id, "host:1");
    }

    #[cfg(unix)]
    #[test]
    fn writer_lease_allows_dead_unknown_host_pid() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let existing = WriterLease {
            owner_id: "unknown-host:4294967295".into(),
            hostname: "unknown-host".into(),
            pid: u32::MAX,
            app_version: "test".into(),
            started_at: 100,
            heartbeat_at: 100,
        };
        db.set_writer_lease(&existing).unwrap();

        let replacement = WriterLease {
            owner_id: "host:1".into(),
            hostname: "host".into(),
            pid: 1,
            app_version: "test".into(),
            started_at: 200,
            heartbeat_at: 200,
        };

        crate::meta::acquire_writer_lease(db.connection(), &replacement, 30 * 60).unwrap();
        assert_eq!(db.writer_lease().unwrap().unwrap().owner_id, "host:1");
    }

    #[test]
    fn duplicate_object_write_skips_new_kind_and_keeps_original_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let bytes = representative_metadata_payload();

        let first = db
            .put_object_with_compression("metadata", &bytes, ObjectCompression::zstd(1, 128, 15))
            .unwrap();
        let second = db
            .put_object_with_compression("other", &bytes, ObjectCompression::none())
            .unwrap();

        assert_eq!(second.hash(), first.hash());
        assert_eq!(second.kind(), "metadata");
        assert_eq!(second.codec(), ObjectCodec::Zstd);
    }

    #[test]
    fn can_force_uncompressed_object_storage() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let bytes = representative_metadata_payload();

        let object = db.put_object_uncompressed("metadata", &bytes).unwrap();
        assert_eq!(object.codec(), ObjectCodec::None);
        assert_eq!(object.raw_size(), bytes.len() as u64);
        assert_eq!(object.stored_size(), bytes.len() as u64);
        assert_eq!(object.bytes, bytes);
    }

    #[test]
    fn compresses_large_objects_when_gate_passes() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let bytes = representative_metadata_payload();
        let compression = ObjectCompression::zstd(1, 128, 15);

        let object = db
            .put_object_with_compression("metadata", &bytes, compression)
            .unwrap();
        assert_eq!(object.codec(), ObjectCodec::Zstd);
        assert_eq!(object.raw_size(), bytes.len() as u64);
        assert!(object.stored_size() < object.raw_size());
        assert_eq!(object.bytes, bytes);
        assert_eq!(db.object(object.hash()).unwrap().unwrap(), object);
    }

    #[test]
    fn skips_compression_when_gate_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let bytes: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let compression = ObjectCompression::zstd(1, 128, 95);

        let object = db
            .put_object_with_compression("binary", &bytes, compression)
            .unwrap();
        assert_eq!(object.codec(), ObjectCodec::None);
        assert_eq!(object.raw_size(), bytes.len() as u64);
        assert_eq!(object.stored_size(), bytes.len() as u64);
        assert_eq!(object.bytes, bytes);
    }

    #[test]
    fn compression_benchmark_supports_default_gate_for_representative_payloads() {
        let metadata = representative_metadata_payload();
        let request = representative_request_payload();
        let report = benchmark_zstd_compression(
            [metadata.as_slice(), request.as_slice()],
            DEFAULT_ZSTD_LEVEL,
        )
        .unwrap();

        assert_eq!(report.samples.len(), 2);
        assert!(report.supports_policy(DEFAULT_ZSTD_MIN_SAVINGS_PERCENT));
        assert!(report.compression_ratio_percent() < 50);
    }

    #[test]
    fn session_snapshot_save_appends_only_changed_history_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = protocol::HistoryItem::user(protocol::Content::text("first"));
        let second = protocol::HistoryItem::user(protocol::Content::text("second"));
        let mut snapshot = SessionSnapshot {
            state: SessionState {
                id: "s1".into(),
                title: Some("title".into()),
                slug: Some("title".into()),
                first_user_message: None,
                cwd: Some("/tmp/project".into()),
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: Some("model-a".into()),
                parent_id: None,
                accounting_json: Some(serde_json::json!({"prompt_tokens": 1})),
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: 0,
                history_len: 1,
                created_at: 10,
                updated_at: 20,
            },
            meta_json: Some(serde_json::json!({"id": "s1", "schema_version": 2})),
            history_start_idx: 0,
            history_len: 1,
            history: vec![first.clone()],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            accounting_snapshots: Vec::new(),
        };

        let first_report = db.save_session_snapshot(&snapshot, None).unwrap();
        assert_eq!(first_report.history_inserted, 1);
        assert_eq!(first_report.history_deleted, 0);
        assert!(first_report.changed);
        let first_created_at: i64 = db
            .connection()
            .query_row(
                "SELECT created_at FROM history_items WHERE idx = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();

        let no_op = db.save_session_snapshot(&snapshot, None).unwrap();
        assert_eq!(no_op.history_inserted, 0);
        assert_eq!(no_op.history_deleted, 0);
        assert!(!no_op.changed);
        let second_created_at: i64 = db
            .connection()
            .query_row(
                "SELECT created_at FROM history_items WHERE idx = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_created_at, second_created_at);

        snapshot.history_start_idx = 1;
        snapshot.history = vec![second.clone()];
        snapshot.history_len = 2;
        snapshot.state.history_len = 2;
        snapshot.state.updated_at = 30;
        let append = db
            .save_session_snapshot(&snapshot, Some(no_op.revision))
            .unwrap();
        assert_eq!(append.history_unchanged, 1);
        assert_eq!(append.history_inserted, 1);
        assert_eq!(append.history_deleted, 0);
        assert_eq!(
            db.load_session_snapshot().unwrap().unwrap().history.len(),
            2
        );
        assert_eq!(db.search_blob().unwrap(), "first\nuser\nsecond\nuser\n");
        assert_eq!(db.read_history_items_range(1..2).unwrap(), vec![second]);
        assert!(db.read_history_items_range(2..2).unwrap().is_empty());
    }

    #[test]
    fn combined_snapshot_and_descriptor_save_rolls_back_together() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = protocol::HistoryItem::user(protocol::Content::text("first"));
        let second = protocol::HistoryItem::user(protocol::Content::text("second"));
        let mut snapshot = SessionSnapshot {
            state: SessionState {
                id: "s1".into(),
                title: Some("title".into()),
                slug: Some("title".into()),
                first_user_message: None,
                cwd: Some("/tmp/project".into()),
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: Some("model-a".into()),
                parent_id: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: 0,
                history_len: 1,
                created_at: 10,
                updated_at: 20,
            },
            meta_json: Some(serde_json::json!({"id": "s1", "schema_version": 2})),
            history_start_idx: 0,
            history_len: 1,
            history: vec![first.clone()],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            accounting_snapshots: Vec::new(),
        };

        db.save_session_snapshot_and_transcript_descriptor_suffix_as_writer(
            &snapshot,
            0,
            &[transcript_record_with_history(
                0,
                0,
                "first",
                "first descriptor",
            )],
        )
        .unwrap();
        assert_eq!(db.search_blob().unwrap(), "first descriptor\n");

        snapshot.history_start_idx = 1;
        snapshot.history = vec![second];
        snapshot.history_len = 2;
        snapshot.state.history_len = 2;
        snapshot.state.updated_at = 30;
        let mut invalid_record =
            transcript_record_with_history(1, 1, "second", "second descriptor");
        invalid_record.descriptor_json = "{invalid".into();

        db.save_session_snapshot_and_transcript_descriptor_suffix_as_writer(
            &snapshot,
            1,
            &[invalid_record],
        )
        .unwrap_err();

        let loaded = db.load_session_snapshot().unwrap().unwrap();
        assert_eq!(loaded.history, vec![first]);
        assert_eq!(loaded.state.history_len, 1);
        assert_eq!(db.search_blob().unwrap(), "first descriptor\n");
        assert_eq!(db.read_transcript_descriptor_records().unwrap().len(), 1);
    }

    #[test]
    fn session_snapshot_append_replaces_stale_descriptor_tail() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = protocol::HistoryItem::user(protocol::Content::text("first"));
        let second = protocol::HistoryItem::user(protocol::Content::text("second"));
        let mut snapshot = SessionSnapshot {
            state: SessionState {
                id: "s1".into(),
                title: Some("title".into()),
                slug: Some("title".into()),
                first_user_message: None,
                cwd: Some("/tmp/project".into()),
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: Some("model-a".into()),
                parent_id: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: 0,
                history_len: 1,
                created_at: 10,
                updated_at: 20,
            },
            meta_json: Some(serde_json::json!({"id": "s1", "schema_version": 2})),
            history_start_idx: 0,
            history_len: 1,
            history: vec![first],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            accounting_snapshots: Vec::new(),
        };

        let first_report = db.save_session_snapshot(&snapshot, None).unwrap();
        db.replace_transcript_descriptor_records(&[
            TranscriptDescriptorRecord {
                block_idx: 0,
                history_idx: Some(0),
                kind: "user".into(),
                tool_call_id: None,
                tool_name: None,
                content_hash: "11".into(),
                estimated_text_bytes: 14,
                preview_text: "first detailed".into(),
                search_text: "first detailed".into(),
                descriptor_json: serde_json::json!({"Text": {"content": "first detailed"}})
                    .to_string(),
                origin_json: None,
                tool_state_json: None,
            },
            TranscriptDescriptorRecord {
                block_idx: 1,
                history_idx: None,
                kind: "thinking".into(),
                tool_call_id: None,
                tool_name: None,
                content_hash: "12".into(),
                estimated_text_bytes: 14,
                preview_text: "synthetic tail".into(),
                search_text: "synthetic tail".into(),
                descriptor_json: serde_json::json!({"Text": {"content": "synthetic tail"}})
                    .to_string(),
                origin_json: None,
                tool_state_json: None,
            },
        ])
        .unwrap();
        assert_eq!(
            db.search_blob().unwrap(),
            "first detailed\nsynthetic tail\n"
        );

        snapshot.history_start_idx = 1;
        snapshot.history = vec![second.clone()];
        snapshot.history_len = 2;
        snapshot.state.history_len = 2;
        snapshot.state.updated_at = 30;
        let append = db
            .save_session_snapshot(&snapshot, Some(first_report.revision))
            .unwrap();

        assert_eq!(append.history_unchanged, 1);
        assert_eq!(append.history_inserted, 1);
        assert_eq!(db.search_blob().unwrap(), "first detailed\nsecond\nuser\n");
        assert_eq!(
            db.load_session_snapshot().unwrap().unwrap().history.len(),
            2
        );
    }

    #[test]
    fn session_snapshot_suffix_preserves_prior_multi_block_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = protocol::HistoryItem::user(protocol::Content::text("first"));
        let assistant = protocol::HistoryItem::user(protocol::Content::text("assistant history"));
        let old_request = protocol::HistoryItem::user(protocol::Content::text("old request"));
        let new_request = protocol::HistoryItem::user(protocol::Content::text("new request"));
        let mut snapshot = SessionSnapshot {
            state: SessionState {
                id: "s1".into(),
                title: Some("title".into()),
                slug: Some("title".into()),
                first_user_message: None,
                cwd: Some("/tmp/project".into()),
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: Some("model-a".into()),
                parent_id: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: 0,
                history_len: 3,
                created_at: 10,
                updated_at: 20,
            },
            meta_json: Some(serde_json::json!({"id": "s1", "schema_version": 2})),
            history_start_idx: 0,
            history_len: 3,
            history: vec![first, assistant, old_request],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            accounting_snapshots: Vec::new(),
        };

        let first_report = db.save_session_snapshot(&snapshot, None).unwrap();
        db.replace_transcript_descriptor_records(&[
            transcript_record_with_history(0, 0, "first", "first descriptor"),
            transcript_record_with_history(1, 1, "thinking", "assistant thinking"),
            transcript_record_with_history(2, 1, "answer", "assistant answer"),
            transcript_record_with_history(3, 2, "old-request", "old request descriptor"),
        ])
        .unwrap();

        snapshot.history_start_idx = 2;
        snapshot.history = vec![new_request];
        snapshot.history_len = 3;
        snapshot.state.history_len = 3;
        snapshot.state.updated_at = 30;
        db.save_session_snapshot(&snapshot, Some(first_report.revision))
            .unwrap();

        let search = db.search_blob().unwrap();
        assert!(search.contains("assistant answer\n"));
        assert!(search.contains("new request\n"));
        assert!(!search.contains("old request descriptor"));
    }

    #[test]
    fn transcript_descriptors_roundtrip_and_feed_search_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.replace_transcript_descriptor_records(&[
            TranscriptDescriptorRecord {
                block_idx: 0,
                history_idx: None,
                kind: "assistant".into(),
                tool_call_id: None,
                tool_name: None,
                content_hash: "11".into(),
                estimated_text_bytes: 5,
                preview_text: "alpha".into(),
                search_text: "alpha".into(),
                descriptor_json: serde_json::json!({"Text": {"content": "alpha"}}).to_string(),
                origin_json: None,
                tool_state_json: None,
            },
            TranscriptDescriptorRecord {
                block_idx: 1,
                history_idx: None,
                kind: "tool".into(),
                tool_call_id: Some("call-1".into()),
                tool_name: Some("tool_name".into()),
                content_hash: "12".into(),
                estimated_text_bytes: 13,
                preview_text: "needle output".into(),
                search_text: "needle output".into(),
                descriptor_json: serde_json::json!({
                    "ToolCall": {
                        "call_id": "call-1",
                        "name": "tool_name",
                        "summary": [],
                        "args": {}
                    }
                })
                .to_string(),
                origin_json: None,
                tool_state_json: None,
            },
        ])
        .unwrap();

        let rows = db.read_transcript_descriptor_records().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(db.search_blob().unwrap(), "alpha\nneedle output\n");
        assert_eq!(
            db.search_transcript_candidates("needle").unwrap(),
            vec![TranscriptSearchCandidate {
                block_idx: 1,
                history_idx: None,
            }]
        );
    }
    #[test]
    fn appends_request_audit_to_sqlite_objects_and_queries_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let body = serde_json::json!({"model": "model-a", "messages": [{"role": "user", "content": "hi"}]});
        let entry = protocol::request_log::RequestLogEntry {
            request_id: 9,
            kind: "turn".into(),
            turn_id: Some(9),
            ask_id: None,
            history_len: Some(4),
            timestamp_ms: 1000,
            provider_kind: "openai".into(),
            api_base: "https://api.example.test".into(),
            model: "model-a".into(),
            url: "https://api.example.test/v1/chat/completions".into(),
            http_status: Some(200),
            body: body.clone(),
            prompt_cache_key: Some("session-9".into()),
            stream: true,
            system_prompt: Some("duplicated prompt context is intentionally not stored".into()),
            messages: Some(vec![protocol::Message::user(protocol::Content::text("hi"))]),
            tools: None,
            response: Some(protocol::request_log::RequestResponse {
                content: Some("hello".into()),
                reasoning: None,
                tool_calls: None,
                raw: Some(serde_json::json!({"id": "resp-1"})),
            }),
            usage: Some(protocol::TokenUsage {
                context_tokens: Some(20),
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                cache_read_tokens: Some(2),
                cache_write_tokens: Some(1),
                reasoning_tokens: Some(3),
            }),
            cost_usd: Some(0.000123),
            tokens_per_sec: Some(42.0),
            elapsed_ms: Some(250),
            attempt: 1,
            error: None,
            background: false,
        };

        let id = db.append_request_attempt(&entry).unwrap();
        let attempts = db
            .query_request_attempts(&RequestAuditQuery {
                request_id: Some("9".into()),
                min_input_tokens: Some(10),
                min_cost_micros: Some(123),
                order: RequestAuditOrder::OldestFirst,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, id);
        assert_eq!(attempts[0].provider.as_deref(), Some("openai"));
        assert!(attempts[0].stream);
        assert_eq!(
            attempts[0].usage.as_ref().unwrap().cache_write_tokens,
            Some(1)
        );
        assert_eq!(attempts[0].cost_usd, Some(0.000123));

        let payloads = db.request_payloads(id).unwrap().unwrap();
        assert_eq!(payloads.body.unwrap(), body);
        assert_eq!(payloads.response.unwrap()["raw"]["id"], "resp-1");
        assert!(payloads.error.is_none());
        let object_count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(object_count, 2);
    }

    #[test]
    fn imports_split_session_and_exports_history_and_requests() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_dir = dir.path().join("legacy-session");
        fs::create_dir(&legacy_dir).unwrap();
        fs::write(
            legacy_dir.join("meta.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 2,
                "id": "s1",
                "title": "import me",
                "slug": "import-me",
                "created_at_ms": 10,
                "updated_at_ms": 20,
                "mode": "ask",
                "model": "model-a",
                "cwd": "/tmp/project",
                "session_usage": {"prompt_tokens": 1},
                "checkpoint": {"kind": "summary"}
            }))
            .unwrap(),
        )
        .unwrap();

        let metadata = serde_json::json!({
            "before": "a".repeat(5000),
            "after": "b".repeat(5000),
        });
        let history_item = serde_json::json!({
            "kind": "assistant",
            "invocations": [{
                "call_id": "call-1",
                "name": "edit_file",
                "arguments": "{}",
                "result": {
                    "content": "edited",
                    "is_error": false,
                    "metadata": metadata
                }
            }]
        });
        fs::write(
            legacy_dir.join("history.jsonl"),
            format!("{}\n", serde_json::to_string(&history_item).unwrap()),
        )
        .unwrap();

        let request = serde_json::json!({
            "request_id": 7,
            "kind": "turn",
            "turn_id": 7,
            "history_len": 1,
            "timestamp_ms": 30,
            "provider_kind": "openai",
            "model": "model-a",
            "url": "https://example.test/v1/chat/completions",
            "body": {"model": "model-a", "messages": [{"role": "user", "content": "hi"}]},
            "system_prompt": "legacy prompt duplicate",
            "messages": [{"role": "user", "content": "hi from top-level duplicate"}],
            "tools": [{"type": "function", "function": {"name": "echo"}}],
            "response": {"content": "hello", "raw": {"id": "resp"}},
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
            "elapsed_ms": 40,
            "attempt": 1,
            "background": false
        });
        fs::write(
            legacy_dir.join("requests.jsonl"),
            format!("{}\n", serde_json::to_string(&request).unwrap()),
        )
        .unwrap();

        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let report = db.import_legacy_session_dir(&legacy_dir).unwrap();
        assert_eq!(report.history_items, 1);
        assert_eq!(report.transcript_blocks, 1);
        assert_eq!(report.request_attempts, 1);
        assert!(report.objects >= 3);

        let state = db.session_state().unwrap().unwrap();
        assert_eq!(state.id, "s1");
        assert_eq!(state.history_len, 1);
        assert_eq!(state.model.as_deref(), Some("model-a"));

        let stored_json: String = db
            .connection()
            .query_row("SELECT json FROM history_items WHERE idx = 0", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(stored_json.contains("$smelt_object_ref"));
        assert!(!stored_json.contains(&"a".repeat(5000)));

        let block_count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM transcript_blocks", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(block_count, 1);

        let mut exported_history = Vec::new();
        db.export_history_jsonl(&mut exported_history).unwrap();
        let exported_history_value: serde_json::Value =
            serde_json::from_slice(exported_history.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(exported_history_value, history_item);

        let snapshot_count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM metadata_snapshots", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(snapshot_count, 0);

        let attempts = db.request_attempts().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].request_id.as_deref(), Some("7"));
        assert_eq!(attempts[0].provider.as_deref(), Some("openai"));
        assert!(attempts[0].raw_body_size > 0);

        let payloads = db.request_payloads(attempts[0].id).unwrap().unwrap();
        assert_eq!(payloads.body.as_ref().unwrap(), &request["body"]);
        assert!(payloads
            .body
            .as_ref()
            .unwrap()
            .get("system_prompt")
            .is_none());
        assert_eq!(payloads.response.as_ref().unwrap(), &request["response"]);
        assert!(payloads.error.is_none());
        let request_ref_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM request_object_refs WHERE request_attempt_id = ?1",
                [attempts[0].id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(request_ref_count, 2);
        let duplicate_request_object_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE kind IN ('request_messages', 'request_system_prompt', 'request_tools')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(duplicate_request_object_count, 0);

        let mut exported_requests = Vec::new();
        db.export_requests_jsonl(&mut exported_requests).unwrap();
        let exported_request_value: serde_json::Value =
            serde_json::from_slice(exported_requests.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(exported_request_value["body"], request["body"]);
        assert_eq!(exported_request_value["response"], request["response"]);
        assert_eq!(exported_request_value["kind"], request["kind"]);
        assert_eq!(
            exported_request_value["provider_kind"],
            request["provider_kind"]
        );
    }

    #[test]
    fn imports_concatenated_legacy_requests_jsonl_records() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_dir = dir.path().join("legacy-session");
        fs::create_dir(&legacy_dir).unwrap();
        let first = serde_json::json!({
            "request_id": 1,
            "kind": "turn",
            "timestamp_ms": 10,
            "provider_kind": "openai",
            "model": "model-a",
            "body": {"messages": [{"role": "user", "content": "one"}]},
        });
        let second = serde_json::json!({
            "request_id": 2,
            "kind": "turn",
            "timestamp_ms": 20,
            "provider_kind": "openai",
            "model": "model-a",
            "body": {"messages": [{"role": "user", "content": "two"}]},
        });
        fs::write(
            legacy_dir.join("requests.jsonl"),
            format!(
                "{}{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap()
            ),
        )
        .unwrap();

        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let imported = db.import_legacy_requests_jsonl(&legacy_dir).unwrap();

        assert_eq!(imported, 2);
        let attempts = db.request_attempts().unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].request_id.as_deref(), Some("1"));
        assert_eq!(attempts[1].request_id.as_deref(), Some("2"));
    }

    #[test]
    fn refuses_to_import_into_nonempty_database() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_dir = dir.path().join("legacy-session");
        fs::create_dir(&legacy_dir).unwrap();
        fs::write(
            legacy_dir.join("session.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "old1",
                "history": [{"kind": "user", "content": "hello"}]
            }))
            .unwrap(),
        )
        .unwrap();

        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.put_object_uncompressed("test", b"existing").unwrap();
        let err = db.import_legacy_session_dir(&legacy_dir).unwrap_err();
        assert!(err.to_string().contains("non-empty database"));
    }

    #[test]
    fn import_search_text_truncates_on_utf8_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_dir = dir.path().join("legacy-session");
        fs::create_dir(&legacy_dir).unwrap();
        fs::write(
            legacy_dir.join("session.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "unicode1",
                "history": [{"kind": "user", "content": "é".repeat(70_000)}]
            }))
            .unwrap(),
        )
        .unwrap();

        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.import_legacy_session_dir(&legacy_dir).unwrap();
        let search_text: String = db
            .connection()
            .query_row(
                "SELECT search_text FROM history_items WHERE idx = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(search_text.len() <= 64 * 1024);
        assert!(search_text.ends_with('é'));
    }

    #[test]
    fn imports_monolithic_session_json() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_dir = dir.path().join("legacy-session");
        fs::create_dir(&legacy_dir).unwrap();
        let first = serde_json::json!({"kind": "user", "content": "hello"});
        let second = serde_json::json!({"kind": "assistant", "content": "hi"});
        fs::write(
            legacy_dir.join("session.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 2,
                "id": "old1",
                "title": "old",
                "created_at_ms": 100,
                "updated_at_ms": 200,
                "history": [first, second]
            }))
            .unwrap(),
        )
        .unwrap();

        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let report = db.import_legacy_session_dir(&legacy_dir).unwrap();
        assert_eq!(report.history_items, 2);
        assert_eq!(db.session_state().unwrap().unwrap().id, "old1");

        let mut exported = Vec::new();
        db.export_history_jsonl(&mut exported).unwrap();
        assert_eq!(exported.iter().filter(|byte| **byte == b'\n').count(), 2);
    }

    #[test]
    fn imports_legacy_message_session_json_without_history() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_dir = dir.path().join("legacy-session");
        fs::create_dir(&legacy_dir).unwrap();
        fs::write(
            legacy_dir.join("session.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "messages1",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        )
        .unwrap();

        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let report = db.import_legacy_session_dir(&legacy_dir).unwrap();
        assert_eq!(report.history_items, 1);
        assert_eq!(db.session_state().unwrap().unwrap().id, "messages1");
    }

    #[test]
    fn writes_session_meta_sidecar_from_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let state = SessionState {
            id: "s1".into(),
            title: Some("title".into()),
            slug: Some("slug".into()),
            first_user_message: Some("hello".into()),
            cwd: Some("/tmp".into()),
            mode: Some("ask".into()),
            reasoning_effort: Some("low".into()),
            model: Some("model".into()),
            parent_id: Some("parent".into()),
            accounting_json: Some(serde_json::json!({"cost": 1})),
            checkpoint_json: None,
            context_tokens: Some(42),
            context_tokens_history_len: Some(3),
            display_context_tokens: Some(40),
            session_cost_usd: 1.25,
            revision: 7,
            history_len: 3,
            created_at: 10,
            updated_at: 20,
        };

        db.upsert_session_state(&state).unwrap();
        assert_eq!(db.session_state().unwrap(), Some(state));

        let meta_path = dir.path().join("meta.json");
        let meta = db.write_meta_sidecar(&meta_path).unwrap().unwrap();
        assert_eq!(meta.id, "s1");
        assert_eq!(meta.revision, 7);
        assert_eq!(meta.history_len, 3);
        assert_eq!(meta.schema_version, schema::SCHEMA_VERSION);

        let from_file: SessionMeta = serde_json::from_slice(&fs::read(meta_path).unwrap()).unwrap();
        assert_eq!(from_file, meta);
    }

    #[test]
    fn session_state_is_singleton() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let state = SessionState {
            id: "s1".into(),
            title: None,
            slug: None,
            first_user_message: None,
            cwd: None,
            mode: None,
            reasoning_effort: None,
            model: None,
            parent_id: None,
            accounting_json: None,
            checkpoint_json: None,
            context_tokens: None,
            context_tokens_history_len: None,
            display_context_tokens: None,
            session_cost_usd: 0.0,
            revision: 0,
            history_len: 0,
            created_at: 10,
            updated_at: 10,
        };
        let mut next = state.clone();
        next.id = "s2".into();
        next.revision = 1;

        db.upsert_session_state(&state).unwrap();
        db.upsert_session_state(&next).unwrap();

        assert_eq!(db.session_state().unwrap(), Some(next));
        let count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM session_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn roundtrips_writer_lease() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let lease = WriterLease {
            owner_id: "owner".into(),
            hostname: "host".into(),
            pid: 42,
            app_version: "test".into(),
            started_at: 10,
            heartbeat_at: 11,
        };

        assert_eq!(db.writer_lease().unwrap(), None);
        db.set_writer_lease(&lease).unwrap();
        assert_eq!(db.writer_lease().unwrap(), Some(lease));
        db.clear_writer_lease().unwrap();
        assert_eq!(db.writer_lease().unwrap(), None);
    }

    #[test]
    fn corrupt_database_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");
        fs::write(&path, b"not a sqlite database").unwrap();

        let err = SessionDb::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)));
    }

    fn test_session_state(id: &str, history_len: usize) -> SessionState {
        SessionState {
            id: id.to_string(),
            title: None,
            slug: None,
            first_user_message: None,
            cwd: None,
            mode: None,
            reasoning_effort: None,
            model: None,
            parent_id: None,
            accounting_json: None,
            checkpoint_json: None,
            context_tokens: None,
            context_tokens_history_len: None,
            display_context_tokens: None,
            session_cost_usd: 0.0,
            revision: 0,
            history_len: history_len as u64,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn transcript_record(
        block_idx: u64,
        label: &str,
        search_text: &str,
    ) -> TranscriptDescriptorRecord {
        TranscriptDescriptorRecord {
            block_idx,
            history_idx: None,
            kind: "assistant".to_string(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("hash-{label}"),
            estimated_text_bytes: search_text.len() as u64,
            preview_text: search_text.to_string(),
            search_text: search_text.to_string(),
            descriptor_json: serde_json::json!({
                "kind": "assistant",
                "label": label,
                "text": search_text,
            })
            .to_string(),
            origin_json: Some(
                serde_json::json!({
                    "History": block_idx,
                })
                .to_string(),
            ),
            tool_state_json: None,
        }
    }

    fn transcript_record_with_history(
        block_idx: u64,
        history_idx: u64,
        label: &str,
        search_text: &str,
    ) -> TranscriptDescriptorRecord {
        TranscriptDescriptorRecord {
            block_idx,
            history_idx: Some(history_idx),
            kind: "assistant".to_string(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("hash-{label}"),
            estimated_text_bytes: search_text.len() as u64,
            preview_text: search_text.to_string(),
            search_text: search_text.to_string(),
            descriptor_json: serde_json::json!({
                "kind": "assistant",
                "label": label,
                "text": search_text,
            })
            .to_string(),
            origin_json: Some(
                serde_json::json!({
                    "History": history_idx,
                })
                .to_string(),
            ),
            tool_state_json: None,
        }
    }

    fn representative_metadata_payload() -> Vec<u8> {
        let mut payload = Vec::new();
        for idx in 0..256 {
            payload.extend_from_slice(
                br#"{"tool":"edit_file","metadata":{"file_path":"/tmp/example.rs","old_string":"#,
            );
            payload.extend_from_slice(format!("line-{idx:04}-before\\n").as_bytes());
            payload.extend_from_slice(br#"","new_string":"#);
            payload.extend_from_slice(format!("line-{idx:04}-after\\n").as_bytes());
            payload.extend_from_slice(
                br#"","status":"ok"}}
"#,
            );
        }
        payload
    }

    fn representative_request_payload() -> Vec<u8> {
        let mut payload = Vec::from(
            br#"{"model":"test-model","messages":[{"role":"system","content":"you are smelt"}"#,
        );
        for idx in 0..128 {
            payload.extend_from_slice(
                br#",{"role":"user","content":"summarize this repeated context: "#,
            );
            payload.extend_from_slice(format!("chunk-{idx:04} ").repeat(8).as_bytes());
            payload.extend_from_slice(br#""}"#);
        }
        payload
            .extend_from_slice(br#"],"tools":[{"name":"edit_file","schema":{"type":"object"}}]}"#);
        payload
    }
}
