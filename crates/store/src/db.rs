use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{
    Connection, DropBehavior, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history::{
    self, TranscriptBlockMetadataRecord, TranscriptDescriptorIndex, TranscriptDescriptorRange,
    TranscriptDescriptorRecord, TranscriptDescriptorSlice, TranscriptSearchCandidate,
};
use crate::jsonl_export;
use crate::meta::{self, SessionIdentity, SessionMeta, SessionMetadata, WriterOwner};
use crate::object::{self, ObjectMeta, StoredObject};
use crate::request_audit::{
    self, RequestAuditPayloads, RequestAuditQuery, RequestAuditStats, RequestAuditSummary,
};
use crate::schema;
use crate::session_commit::{
    DescriptorIndex, DescriptorLen, HistoryIndex, HistoryIndexBound, HistoryLen, SaveReceipt,
    SessionCommit, SessionCommitFailure, StoreHead,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenMode {
    CreateOrMigrate,
    CurrentWriter,
    ReadOnly,
}

#[derive(Clone, Debug)]
pub(crate) struct OpenOptions {
    mode: OpenMode,
    app_version: String,
    object_compression: ObjectCompression,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            mode: OpenMode::CreateOrMigrate,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            object_compression: ObjectCompression::default(),
        }
    }
}

const LAST_SESSION_COMMIT_KEY: &str = "last_session_commit";
const JOURNAL_SIZE_LIMIT_BYTES: u64 = 32 * 1024 * 1024;
const WAL_AUTOCHECKPOINT_PAGES: u64 = 1_000;
const SESSION_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(serde::Deserialize, serde::Serialize)]
struct PersistedSessionCommit {
    fingerprint: String,
    receipt: SaveReceipt,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct StorageStats {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub history_rows: u64,
    pub descriptor_rows: u64,
    pub object_rows: u64,
    pub object_raw_bytes: u64,
    pub object_stored_bytes: u64,
    pub request_rows: u64,
}

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
    pub descriptors: Vec<TranscriptDescriptorRecord>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionResumeSnapshot {
    pub session: StoredSession,
    pub retained_history_len: usize,
    pub history_text_bytes: u64,
    pub missing_object_references: Vec<String>,
    pub descriptor_tail: TranscriptDescriptorSlice,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct DoctorReport {
    pub schema_version: i32,
    pub healthy: bool,
    pub issues: Vec<String>,
    pub stats: StorageStats,
}

#[derive(Debug)]
pub struct SessionDb {
    conn: Connection,
    path: PathBuf,
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

    pub(crate) fn open_current(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(
            path,
            OpenOptions {
                mode: OpenMode::CurrentWriter,
                ..OpenOptions::default()
            },
        )
    }

    pub(crate) fn open_with_options(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self> {
        let _perf = smelt_perf::perf::begin(match options.mode {
            OpenMode::CreateOrMigrate | OpenMode::CurrentWriter => "store:db:open_read_write",
            OpenMode::ReadOnly => "store:db:open_read_only",
        });
        let path = path.as_ref().to_path_buf();
        if matches!(
            options.mode,
            OpenMode::CreateOrMigrate | OpenMode::CurrentWriter
        ) {
            prepare_writable_path(&path)?;
        }

        let flags = match options.mode {
            OpenMode::CreateOrMigrate => {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            }
            OpenMode::CurrentWriter => OpenFlags::SQLITE_OPEN_READ_WRITE,
            OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        } | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let mut conn = Connection::open_with_flags(&path, flags)?;
        apply_pragmas(&conn, options.mode)?;

        match options.mode {
            OpenMode::CreateOrMigrate => {
                schema::migrate(&mut conn, &options.app_version)?;
                secure_sqlite_files(&path)?;
            }
            OpenMode::CurrentWriter => {
                schema::validate_read_only_schema(&conn)?;
                secure_sqlite_files(&path)?;
            }
            OpenMode::ReadOnly => schema::validate_read_only_schema(&conn)?,
        }

        Ok(Self {
            conn,
            path,
            object_compression: options.object_compression,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn immediate_transaction<T>(
        &mut self,
        operation: &'static str,
        f: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        self.run_immediate_transaction(operation, f, |err| err)
    }

    fn run_immediate_transaction<T, E>(
        &mut self,
        operation: &'static str,
        f: impl FnOnce(&Connection) -> std::result::Result<T, E>,
        map_store_error: impl Fn(StoreError) -> E,
    ) -> std::result::Result<T, E>
    where
        E: std::fmt::Debug,
    {
        let started = std::time::Instant::now();
        let mut tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| {
                if sqlite_error_is_locked(&err) {
                    map_store_error(StoreError::Busy {
                        operation,
                        attempts: 1,
                        waited_ms: started.elapsed().as_millis() as u64,
                    })
                } else {
                    map_store_error(StoreError::from(err))
                }
            })?;
        smelt_perf::perf::record_value("store:db:transaction_begin_attempts", 1);
        smelt_perf::perf::record_value(
            "store:db:transaction_begin_wait_ms",
            started.elapsed().as_millis() as u64,
        );
        match f(&tx) {
            Ok(value) => {
                let _perf = smelt_perf::perf::begin("store:db:transaction_commit");
                match tx.execute_batch("COMMIT") {
                    Ok(()) => {
                        tx.set_drop_behavior(DropBehavior::Ignore);
                        Ok(value)
                    }
                    Err(commit_err) => {
                        let rollback = rollback_after_commit_failure(tx);
                        let err = match rollback {
                            Ok(()) => StoreError::from(commit_err),
                            Err(rollback_err) => StoreError::TransactionCleanup {
                                operation,
                                message: format!(
                                    "commit failed ({commit_err}); rollback failed ({rollback_err})"
                                ),
                            },
                        };
                        Err(map_store_error(err))
                    }
                }
            }
            Err(body_err) => match tx.rollback() {
                Ok(()) => Err(body_err),
                Err(rollback_err) => Err(map_store_error(StoreError::TransactionCleanup {
                    operation,
                    message: format!(
                        "body failed ({body_err:?}); rollback failed ({rollback_err})"
                    ),
                })),
            },
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

    pub fn storage_stats(&self) -> Result<StorageStats> {
        let database_bytes = file_size(&self.path)?;
        let wal_bytes = file_size(&sqlite_companion_path(&self.path, "-wal"))?;
        let shm_bytes = file_size(&sqlite_companion_path(&self.path, "-shm"))?;
        let history_rows =
            self.conn
                .query_row("SELECT COUNT(*) FROM history_items", [], |row| {
                    row.get::<_, i64>(0)
                })?;
        let descriptor_rows = self.conn.query_row(
            "SELECT COUNT(*) FROM transcript_blocks WHERE descriptor_json IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let (object_rows, object_raw_bytes, object_stored_bytes) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(raw_size), 0), COALESCE(SUM(stored_size), 0)
             FROM objects",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        let request_rows =
            self.conn
                .query_row("SELECT COUNT(*) FROM request_attempts", [], |row| {
                    row.get::<_, i64>(0)
                })?;
        let history_rows = nonnegative_sql_value(history_rows, "history_rows")?;
        let descriptor_rows = nonnegative_sql_value(descriptor_rows, "descriptor_rows")?;
        let object_rows = nonnegative_sql_value(object_rows, "object_rows")?;
        let object_raw_bytes = nonnegative_sql_value(object_raw_bytes, "object_raw_bytes")?;
        let object_stored_bytes =
            nonnegative_sql_value(object_stored_bytes, "object_stored_bytes")?;
        let request_rows = nonnegative_sql_value(request_rows, "request_rows")?;
        Ok(StorageStats {
            database_bytes,
            wal_bytes,
            shm_bytes,
            history_rows,
            descriptor_rows,
            object_rows,
            object_raw_bytes,
            object_stored_bytes,
            request_rows,
        })
    }

    pub fn doctor_report(&self) -> Result<DoctorReport> {
        let mut issues = Vec::new();
        let mut quick_check = self.conn.prepare("PRAGMA quick_check")?;
        let quick_check = quick_check
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for result in quick_check {
            if result != "ok" {
                issues.push(format!("quick_check: {result}"));
            }
        }
        let foreign_key_violations =
            self.conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })?;
        let foreign_key_violations =
            nonnegative_sql_value(foreign_key_violations, "foreign_key_violations")?;
        if foreign_key_violations != 0 {
            issues.push(format!(
                "foreign_key_check found {foreign_key_violations} violation(s)"
            ));
        }
        let (history_count, history_min, history_max) = self.conn.query_row(
            "SELECT COUNT(*), MIN(idx), MAX(idx) FROM history_items",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )?;
        let history_count = nonnegative_sql_value(history_count, "history_count")?;
        if history_count != 0
            && (history_min != Some(0) || history_max != Some(history_count as i64 - 1))
        {
            issues.push("history indices are not dense from zero".into());
        }
        match meta::stored_session(&self.conn)? {
            Some(session) => {
                if session.history_len != history_count {
                    issues.push(format!(
                        "session metadata history_len {} does not match {history_count} history row(s)",
                        session.history_len
                    ));
                }
                if let Err(err) =
                    meta::validate_session_checkpoint(&session.metadata, session.history_len)
                {
                    issues.push(err.to_string());
                }
            }
            None if history_count != 0 => {
                issues.push("history rows exist without session metadata".into());
            }
            None => {}
        }
        let (descriptor_count, descriptor_min, descriptor_max) = self.conn.query_row(
            "SELECT COUNT(*), MIN(descriptor_idx), MAX(descriptor_idx)
             FROM transcript_blocks WHERE descriptor_json IS NOT NULL",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )?;
        let descriptor_count = nonnegative_sql_value(descriptor_count, "descriptor_count")?;
        if descriptor_count != 0
            && (descriptor_min != Some(0) || descriptor_max != Some(descriptor_count as i64 - 1))
        {
            issues.push("transcript descriptor indices are not dense from zero".into());
        }
        for reference in self.missing_object_references()? {
            issues.push(format!("missing object {reference}"));
        }
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM objects ORDER BY hash")?;
        let hashes = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for hash in hashes {
            if let Err(err) = object::object(&self.conn, &hash) {
                issues.push(format!("object {hash}: {err}"));
            }
        }
        let mut stmt = self.conn.prepare(
            "SELECT block_idx, indexed_text FROM transcript_search
             WHERE length(indexed_text) >= 3 ORDER BY block_idx",
        )?;
        let search_rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (block_idx, indexed_text) in search_rows {
            let probe = indexed_text.chars().take(3).collect::<String>();
            let query = history::fts5_phrase_query(&probe);
            match self.conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM transcript_search_fts
                    WHERE rowid = ?1 AND indexed_text MATCH ?2
                 )",
                rusqlite::params![block_idx, query],
                |row| row.get::<_, bool>(0),
            ) {
                Ok(true) => {}
                Ok(false) => issues.push(format!(
                    "transcript search index is missing block {block_idx}"
                )),
                Err(err) => {
                    issues.push(format!("transcript search index check failed: {err}"));
                    break;
                }
            }
        }
        let stats = self.storage_stats()?;
        Ok(DoctorReport {
            schema_version: self.schema_version()?,
            healthy: issues.is_empty(),
            issues,
            stats,
        })
    }

    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<()> {
        let destination = destination.as_ref();
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(destination)?;
        secure_file(destination)?;
        drop(file);

        let result = (|| {
            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
            let mut destination_db = Connection::open_with_flags(destination, flags)?;
            let backup = rusqlite::backup::Backup::new(&self.conn, &mut destination_db)?;
            backup.run_to_completion(128, std::time::Duration::from_millis(10), None)?;
            drop(backup);
            let check: String =
                destination_db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
            if check != "ok" {
                return Err(StoreError::Integrity(format!(
                    "backup quick_check failed: {check}"
                )));
            }
            drop(destination_db);
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(destination)?
                .sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(destination);
        }
        result
    }

    pub(crate) fn close_hygiene(&self) -> Result<bool> {
        let optimize = self.conn.execute_batch("PRAGMA optimize");
        let zero_timeout = self.conn.busy_timeout(std::time::Duration::ZERO);
        let checkpoint = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            });
        let restore = self.conn.busy_timeout(SESSION_BUSY_TIMEOUT);
        restore?;
        zero_timeout?;
        let (busy, log_frames, checkpointed_frames) = checkpoint?;
        let busy = nonnegative_sql_value(busy, "checkpoint_busy")?;
        let log_frames = nonnegative_sql_value(log_frames, "checkpoint_log_frames")?;
        let checkpointed_frames =
            nonnegative_sql_value(checkpointed_frames, "checkpointed_frames")?;
        smelt_perf::perf::record_value("store:wal:checkpoint_busy", busy);
        smelt_perf::perf::record_value("store:wal:log_frames", log_frames);
        smelt_perf::perf::record_value("store:wal:checkpointed_frames", checkpointed_frames);
        self.record_storage_telemetry()?;
        optimize?;
        Ok(busy == 0)
    }

    fn record_storage_telemetry(&self) -> Result<()> {
        let stats = self.storage_stats()?;
        smelt_perf::perf::record_value("store:size:database_bytes", stats.database_bytes);
        smelt_perf::perf::record_value("store:size:wal_bytes", stats.wal_bytes);
        smelt_perf::perf::record_value("store:size:shm_bytes", stats.shm_bytes);
        smelt_perf::perf::record_value("store:size:history_rows", stats.history_rows);
        smelt_perf::perf::record_value("store:size:descriptor_rows", stats.descriptor_rows);
        smelt_perf::perf::record_value("store:size:object_rows", stats.object_rows);
        smelt_perf::perf::record_value("store:size:object_raw_bytes", stats.object_raw_bytes);
        smelt_perf::perf::record_value("store:size:object_stored_bytes", stats.object_stored_bytes);
        smelt_perf::perf::record_value("store:size:request_rows", stats.request_rows);
        Ok(())
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        meta::set_meta(&self.conn, key, value)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        meta::meta(&self.conn, key)
    }

    pub fn writer_owner(&self) -> Result<Option<WriterOwner>> {
        meta::writer_owner(&self.conn)
    }

    pub(crate) fn last_session_commit_fingerprint(&self) -> Result<Option<String>> {
        persisted_session_commit_fingerprint(&self.conn)
    }

    pub(crate) fn last_session_commit(&self) -> Result<Option<(String, SaveReceipt)>> {
        persisted_session_commit(&self.conn)
            .map(|commit| commit.map(|commit| (commit.fingerprint, commit.receipt)))
    }

    pub(crate) fn verify_writer_owner(&self, token: &str) -> Result<()> {
        meta::verify_writer_owner(&self.conn, token)
    }

    pub(crate) fn claim_writer_owner(&mut self, token: &str, owner: &WriterOwner) -> Result<()> {
        self.immediate_transaction("claim writer ownership", |conn| {
            meta::claim_writer_owner(conn, token, owner)
        })
    }

    pub(crate) fn release_writer_owner(&mut self, token: &str) -> Result<()> {
        self.immediate_transaction("release writer ownership", |conn| {
            meta::clear_writer_owner(conn, token)
        })
    }

    pub fn stored_session(&self) -> Result<Option<StoredSession>> {
        let Some(session) = meta::stored_session(&self.conn)? else {
            return Ok(None);
        };
        let descriptor_len = u64::try_from(history::transcript_descriptor_count(&self.conn)?)
            .map_err(|_| StoreError::Integrity("descriptor length exceeds u64".into()))?;
        Ok(Some(StoredSession {
            identity: session.identity,
            metadata: session.metadata,
            head: StoreHead {
                revision: session.revision.into(),
                history_len: session.history_len.into(),
                descriptor_len: descriptor_len.into(),
            },
        }))
    }

    pub fn store_head(&self) -> Result<StoreHead> {
        Ok(self
            .stored_session()?
            .map_or_else(StoreHead::default, |session| session.head))
    }

    pub fn session_meta(&self) -> Result<Option<SessionMeta>> {
        meta::session_meta(&self.conn)
    }

    pub fn load_session_resume_snapshot(
        &self,
        descriptor_width: u16,
        descriptor_target_rows: u16,
    ) -> Result<Option<SessionResumeSnapshot>> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(stored) = meta::stored_session(&tx)? else {
            tx.commit()?;
            return Ok(None);
        };
        let retained_history_len = history::history_item_count(&tx)?;
        let history_text_bytes = history::history_text_bytes(&tx)?;
        let missing_object_references = missing_object_references(&tx)?;
        let descriptor_len = history::transcript_descriptor_count(&tx)?;
        let descriptor_tail = read_descriptor_tail_for_rows(
            &tx,
            descriptor_len,
            descriptor_width,
            descriptor_target_rows,
        )?;
        let descriptor_len = u64::try_from(descriptor_len)
            .map_err(|_| StoreError::Integrity("transcript descriptor count exceeds u64".into()))?;
        let session = StoredSession {
            identity: stored.identity,
            metadata: stored.metadata,
            head: StoreHead {
                revision: stored.revision.into(),
                history_len: stored.history_len.into(),
                descriptor_len: descriptor_len.into(),
            },
        };
        tx.commit()?;
        Ok(Some(SessionResumeSnapshot {
            session,
            retained_history_len,
            history_text_bytes,
            missing_object_references,
            descriptor_tail,
        }))
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn write_meta_sidecar(&self, path: impl AsRef<Path>) -> Result<Option<SessionMeta>> {
        meta::write_meta_sidecar(&self.conn, path)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn put_object(&self, bytes: &[u8]) -> Result<StoredObject> {
        object::put_object(&self.conn, bytes, self.object_compression)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn put_object_uncompressed(&self, bytes: &[u8]) -> Result<StoredObject> {
        object::put_object(&self.conn, bytes, ObjectCompression::none())
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn put_object_with_compression(
        &self,
        bytes: &[u8],
        compression: ObjectCompression,
    ) -> Result<StoredObject> {
        object::put_object(&self.conn, bytes, compression)
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

    pub fn missing_object_references(&self) -> Result<Vec<String>> {
        missing_object_references(&self.conn)
    }

    pub fn export_history_jsonl(&self, out: impl Write) -> Result<()> {
        jsonl_export::export_history_jsonl(&self.conn, out)
    }

    pub fn export_requests_jsonl(&self, out: impl Write) -> Result<()> {
        jsonl_export::export_requests_jsonl(&self.conn, out)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn append_request_attempt(
        &mut self,
        entry: &protocol::request_log::RequestLogEntry,
        payload_mode: request_audit::RequestAuditPayloadMode,
    ) -> Result<i64> {
        let compression = self.object_compression;
        self.immediate_transaction("append request audit", |conn| {
            request_audit::append_request_attempt(conn, entry, compression, payload_mode)
        })
    }

    pub(crate) fn append_request_attempt_owned(
        &mut self,
        token: &str,
        entry: &protocol::request_log::RequestLogEntry,
        payload_mode: request_audit::RequestAuditPayloadMode,
    ) -> Result<i64> {
        let compression = self.object_compression;
        self.immediate_transaction("append owned request audit", |conn| {
            meta::verify_writer_owner(conn, token)?;
            request_audit::append_request_attempt(conn, entry, compression, payload_mode)
        })
    }

    pub(crate) fn garbage_collect_objects_owned(&mut self, token: &str) -> Result<usize> {
        self.immediate_transaction("garbage collect objects", |conn| {
            meta::verify_writer_owner(conn, token)?;
            object::delete_unreachable_objects(conn)
        })
    }

    pub(crate) fn rebuild_search_index_owned(&mut self, token: &str) -> Result<()> {
        self.immediate_transaction("rebuild search index", |conn| {
            meta::verify_writer_owner(conn, token)?;
            conn.execute(
                "INSERT INTO transcript_search_fts(transcript_search_fts) VALUES('rebuild')",
                [],
            )?;
            Ok(())
        })
    }

    pub(crate) fn vacuum_owned(&self, token: &str) -> Result<()> {
        meta::verify_writer_owner(&self.conn, token)?;
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    pub fn query_request_attempts(
        &self,
        query: &RequestAuditQuery,
    ) -> Result<Vec<RequestAuditSummary>> {
        request_audit::request_attempts(&self.conn, query)
    }

    pub fn request_audit_stats(&self) -> Result<RequestAuditStats> {
        request_audit::request_stats(&self.conn)
    }

    pub fn request_payloads(
        &self,
        request_attempt_id: i64,
    ) -> Result<Option<RequestAuditPayloads>> {
        request_audit::request_payloads(&self.conn, request_attempt_id)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn apply_session_commit(
        &mut self,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        self.apply_session_commit_with_owner(command, None)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn apply_transcript_descriptor_fixture(
        &mut self,
        records: &[TranscriptDescriptorRecord],
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        self.apply_transcript_descriptor_suffix_fixture(0, records)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn apply_transcript_descriptor_suffix_fixture(
        &mut self,
        start: usize,
        records: &[TranscriptDescriptorRecord],
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        let full = self
            .load_full_session()
            .map_err(session_commit_failure_from_store_error)?;
        let (identity, metadata, head, side_tables) = if let Some(full) = full {
            let boundary = full.session.head.history_len.get();
            (
                full.session.identity,
                full.session.metadata,
                full.session.head,
                crate::SideTableSuffixes {
                    start: HistoryIndex::new(boundary),
                    turn_metas: full
                        .turn_metas
                        .into_iter()
                        .filter(|(index, _)| *index >= boundary)
                        .map(|(index, value)| (HistoryIndex::new(index), value))
                        .collect(),
                    metadata_snapshots: full
                        .metadata_snapshots
                        .into_iter()
                        .filter(|(index, _)| *index >= boundary)
                        .map(|(index, value)| (HistoryIndex::new(index), value))
                        .collect(),
                    context_snapshots: full
                        .context_snapshots
                        .into_iter()
                        .filter(|(index, _)| *index >= boundary)
                        .map(|(index, value)| (HistoryIndex::new(index), value))
                        .collect(),
                },
            )
        } else {
            (
                SessionIdentity {
                    id: "test-fixture".into(),
                    created_at: 0,
                    parent_id: None,
                },
                SessionMetadata {
                    title: None,
                    slug: None,
                    first_user_message: None,
                    cwd: None,
                    mode: None,
                    reasoning_effort: None,
                    model: None,
                    fast_mode: None,
                    accounting_json: None,
                    checkpoint_json: None,
                    context_tokens: None,
                    context_tokens_history_len: None,
                    display_context_tokens: None,
                    session_cost_usd: crate::SessionCostUsd::new(0.0)
                        .map_err(session_commit_failure_from_store_error)?,
                    updated_at: 0,
                },
                StoreHead::default(),
                crate::SideTableSuffixes::default(),
            )
        };
        let start =
            DescriptorIndex::try_from(start).map_err(|_| SessionCommitFailure::Integrity {
                message: "descriptor fixture start exceeds u64".into(),
            })?;
        let command = SessionCommit {
            session_id: identity.id.clone(),
            save_id: crate::SaveId::new(head.revision.get().saturating_add(1)),
            expected: head,
            identity,
            metadata,
            history: crate::HistorySuffix {
                start: HistoryIndex::new(head.history_len.get()),
                final_len: head.history_len,
                items: Vec::new(),
            },
            side_tables,
            descriptors: Some(crate::TranscriptDescriptorSuffix {
                start,
                records: records.to_vec(),
            }),
        };
        self.apply_session_commit(&command)
    }

    pub(crate) fn apply_session_commit_owned(
        &mut self,
        token: &str,
        command: &SessionCommit,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        self.apply_session_commit_with_owner(command, Some(token))
    }

    fn apply_session_commit_with_owner(
        &mut self,
        command: &SessionCommit,
        owner_token: Option<&str>,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        let prepared = prepare_session_commit(command)?;
        let compression = self.object_compression;
        self.run_immediate_transaction(
            "apply session commit",
            |conn| {
                apply_session_commit_in_transaction(
                    conn,
                    command,
                    &prepared,
                    owner_token,
                    compression,
                )
            },
            session_commit_failure_from_store_error,
        )
    }

    pub fn load_full_session(&self) -> Result<Option<FullSession>> {
        let tx = self.conn.unchecked_transaction()?;
        let full = load_full_session_from(&tx)?;
        tx.commit()?;
        Ok(full)
    }

    pub(crate) fn load_full_session_prefix(
        &self,
        history_len: usize,
    ) -> Result<Option<FullSession>> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(mut full) = load_full_session_from(&tx)? else {
            tx.commit()?;
            return Ok(None);
        };
        if history_len > full.history.len() {
            return Err(StoreError::Integrity(format!(
                "fork history length {history_len} exceeds source length {}",
                full.history.len()
            )));
        }
        let history_len_u64 = u64::try_from(history_len)
            .map_err(|_| StoreError::Integrity("fork history length exceeds u64".into()))?;
        let descriptor_block_end = tx.query_row(
            "SELECT COALESCE(
                 MIN(block_idx),
                 (SELECT COALESCE(MAX(block_idx) + 1, 0) FROM transcript_blocks)
             )
             FROM transcript_blocks
             WHERE history_idx >= ?1",
            [checked_sql_coordinate(
                history_len_u64,
                "fork history length",
            )?],
            |row| row.get::<_, i64>(0),
        )?;
        let descriptor_block_end = u64::try_from(descriptor_block_end)
            .map_err(|_| StoreError::Integrity("negative transcript block index".into()))?;
        full.history.truncate(history_len);
        full.turn_metas
            .retain(|(index, _)| *index <= history_len_u64);
        full.metadata_snapshots
            .retain(|(index, _)| *index <= history_len_u64);
        full.context_snapshots
            .retain(|(index, _)| *index <= history_len_u64);
        full.descriptors.retain(|record| {
            record.block_idx < descriptor_block_end
                && record
                    .history_idx
                    .is_none_or(|index| index < history_len_u64)
        });
        full.session.head.history_len = history_len_u64.into();
        full.session.head.descriptor_len = u64::try_from(full.descriptors.len())
            .map_err(|_| StoreError::Integrity("descriptor length exceeds u64".into()))?
            .into();
        tx.commit()?;
        Ok(Some(full))
    }

    pub(crate) fn repaired_checkpoint_metadata(
        &self,
    ) -> Result<Option<(StoredSession, SessionMetadata)>> {
        let Some((stored, metadata)) = meta::repaired_checkpoint_metadata(&self.conn)? else {
            return Ok(None);
        };
        let descriptor_len = u64::try_from(history::transcript_descriptor_count(&self.conn)?)
            .map_err(|_| StoreError::Integrity("descriptor length exceeds u64".into()))?;
        Ok(Some((
            StoredSession {
                identity: stored.identity,
                metadata: stored.metadata,
                head: StoreHead {
                    revision: stored.revision.into(),
                    history_len: stored.history_len.into(),
                    descriptor_len: descriptor_len.into(),
                },
            },
            metadata,
        )))
    }

    pub fn transcript_descriptor_count(&self) -> Result<usize> {
        history::transcript_descriptor_count(&self.conn)
    }

    /// Fast descriptor extent using the stored descriptor ordinal.
    /// This is equivalent to the descriptor count for current stores.
    pub fn transcript_descriptor_dense_extent(&self) -> Result<usize> {
        history::transcript_descriptor_dense_extent(&self.conn)
    }

    pub fn transcript_descriptor_index_for_block_idx(
        &self,
        block_idx: u64,
    ) -> Result<Option<TranscriptDescriptorIndex>> {
        history::transcript_descriptor_index_for_block_idx(&self.conn, block_idx)
    }

    pub fn transcript_descriptor_estimated_rows(
        &self,
        range: TranscriptDescriptorRange,
        width: u16,
    ) -> Result<u64> {
        history::transcript_descriptor_estimated_rows(&self.conn, range, width)
    }

    pub fn read_all_transcript_descriptor_records(
        &self,
    ) -> Result<Vec<TranscriptDescriptorRecord>> {
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

    pub fn read_transcript_descriptor_tail_for_rows(
        &self,
        width: u16,
        target_rows: u16,
    ) -> Result<TranscriptDescriptorSlice> {
        let tx = self.conn.unchecked_transaction()?;
        let total_count = history::transcript_descriptor_count(&tx)?;
        let slice = read_descriptor_tail_for_rows(&tx, total_count, width, target_rows)?;
        tx.commit()?;
        Ok(slice)
    }

    pub fn read_transcript_descriptor_centered_slice(
        &self,
        center_descriptor_idx: u64,
        before: usize,
        after: usize,
    ) -> Result<TranscriptDescriptorSlice> {
        history::read_transcript_descriptor_centered_slice(
            &self.conn,
            center_descriptor_idx,
            before,
            after,
        )
    }

    pub fn read_transcript_descriptor_before_kind_at_index(
        &self,
        kind: &str,
        before_or_at_descriptor_index: u64,
    ) -> Result<Option<TranscriptDescriptorRecord>> {
        history::read_transcript_descriptor_before_kind_at_index(
            &self.conn,
            kind,
            before_or_at_descriptor_index,
        )
    }

    pub fn read_transcript_descriptor_after_kind_at_index(
        &self,
        kind: &str,
        after_or_at_descriptor_index: u64,
    ) -> Result<Option<TranscriptDescriptorRecord>> {
        history::read_transcript_descriptor_after_kind_at_index(
            &self.conn,
            kind,
            after_or_at_descriptor_index,
        )
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

    pub fn legacy_attachment_references(&self, history_end: usize) -> Result<Vec<String>> {
        history::legacy_attachment_references(&self.conn, history_end)
    }

    pub fn history_item_count(&self) -> Result<usize> {
        history::history_item_count(&self.conn)
    }

    pub fn transcript_block_count(&self) -> Result<usize> {
        history::transcript_block_count(&self.conn)
    }

    pub fn transcript_missing_descriptor_count(&self) -> Result<usize> {
        history::transcript_missing_descriptor_count(&self.conn)
    }

    pub fn transcript_descriptor_max_history_idx(&self) -> Result<Option<usize>> {
        history::transcript_descriptor_max_history_idx(&self.conn)
    }

    pub fn transcript_max_block_idx(&self) -> Result<Option<u64>> {
        history::transcript_max_block_idx(&self.conn)
    }

    pub fn read_transcript_block_metadata_range(
        &self,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<TranscriptBlockMetadataRecord>> {
        history::read_transcript_block_metadata_range(&self.conn, range)
    }

    pub fn read_transcript_block_metadata_tail(
        &self,
        count: usize,
    ) -> Result<Vec<TranscriptBlockMetadataRecord>> {
        history::read_transcript_block_metadata_tail(&self.conn, count)
    }

    pub fn history_text_bytes(&self) -> Result<u64> {
        history::history_text_bytes(&self.conn)
    }

    pub fn search_blob(&self) -> Result<String> {
        history::search_blob(&self.conn)
    }
}

fn load_full_session_from(conn: &Connection) -> Result<Option<FullSession>> {
    let Some(stored) = meta::stored_session(conn)? else {
        return Ok(None);
    };
    let descriptor_len = history::transcript_descriptor_count(conn)?;
    let descriptor_len = u64::try_from(descriptor_len)
        .map_err(|_| StoreError::Integrity("descriptor length exceeds u64".into()))?;
    Ok(Some(FullSession {
        session: StoredSession {
            identity: stored.identity,
            metadata: stored.metadata,
            head: StoreHead {
                revision: stored.revision.into(),
                history_len: stored.history_len.into(),
                descriptor_len: descriptor_len.into(),
            },
        },
        history: history::read_history_items(conn)?,
        turn_metas: read_side_table_rows(conn, SideTable::TurnMetas)?,
        metadata_snapshots: read_side_table_rows(conn, SideTable::MetadataSnapshots)?,
        context_snapshots: read_side_table_rows(conn, SideTable::ContextSnapshots)?,
        descriptors: history::read_transcript_descriptor_records(conn)?,
    }))
}

#[derive(Clone, Copy, Debug)]
enum SideTable {
    TurnMetas,
    MetadataSnapshots,
    ContextSnapshots,
}

impl SideTable {
    const ALL: [Self; 3] = [
        Self::TurnMetas,
        Self::MetadataSnapshots,
        Self::ContextSnapshots,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::TurnMetas => "turn_metas",
            Self::MetadataSnapshots => "metadata_snapshots",
            Self::ContextSnapshots => "accounting_snapshots",
        }
    }

    const fn index_column(self) -> &'static str {
        match self {
            Self::TurnMetas => "turn_idx",
            Self::MetadataSnapshots | Self::ContextSnapshots => "history_idx",
        }
    }

    const fn value_column(self) -> &'static str {
        match self {
            Self::TurnMetas => "meta_json",
            Self::MetadataSnapshots => "metadata_json",
            Self::ContextSnapshots => "accounting_json",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SideTableChanges {
    turn_metas: bool,
    metadata_snapshots: bool,
    context_snapshots: bool,
}

impl SideTableChanges {
    const fn any(self) -> bool {
        self.turn_metas || self.metadata_snapshots || self.context_snapshots
    }

    const fn get(self, table: SideTable) -> bool {
        match table {
            SideTable::TurnMetas => self.turn_metas,
            SideTable::MetadataSnapshots => self.metadata_snapshots,
            SideTable::ContextSnapshots => self.context_snapshots,
        }
    }
}

#[derive(Debug)]
struct PreparedSideTables {
    start: u64,
    turn_metas: Vec<(u64, serde_json::Value)>,
    metadata_snapshots: Vec<(u64, serde_json::Value)>,
    context_snapshots: Vec<(u64, serde_json::Value)>,
}

impl PreparedSideTables {
    fn new(side_tables: &crate::SideTableSuffixes) -> Result<Self> {
        Ok(Self {
            start: side_tables.start.get(),
            turn_metas: prepare_side_table_rows(&side_tables.turn_metas, side_tables.start.get())?,
            metadata_snapshots: prepare_side_table_rows(
                &side_tables.metadata_snapshots,
                side_tables.start.get(),
            )?,
            context_snapshots: prepare_side_table_rows(
                &side_tables.context_snapshots,
                side_tables.start.get(),
            )?,
        })
    }

    fn rows(&self, table: SideTable) -> &[(u64, serde_json::Value)] {
        match table {
            SideTable::TurnMetas => &self.turn_metas,
            SideTable::MetadataSnapshots => &self.metadata_snapshots,
            SideTable::ContextSnapshots => &self.context_snapshots,
        }
    }

    fn changes(&self, conn: &Connection) -> Result<SideTableChanges> {
        Ok(SideTableChanges {
            turn_metas: read_side_table_rows_from(conn, SideTable::TurnMetas, self.start)?
                != self.turn_metas,
            metadata_snapshots: read_side_table_rows_from(
                conn,
                SideTable::MetadataSnapshots,
                self.start,
            )? != self.metadata_snapshots,
            context_snapshots: read_side_table_rows_from(
                conn,
                SideTable::ContextSnapshots,
                self.start,
            )? != self.context_snapshots,
        })
    }

    fn apply_changes(&self, conn: &Connection, changes: SideTableChanges) -> Result<()> {
        for table in SideTable::ALL {
            if changes.get(table) {
                replace_side_table_suffix(conn, table, self.start, self.rows(table))?;
            }
        }
        Ok(())
    }
}

fn prepare_side_table_rows(
    rows: &[(HistoryIndex, serde_json::Value)],
    start: u64,
) -> Result<Vec<(u64, serde_json::Value)>> {
    rows.iter()
        .map(|(index, value)| {
            checked_sql_coordinate(index.get(), "side-table row index")?;
            Ok((index.get(), value.clone()))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()
        .map(|rows| {
            rows.into_iter()
                .filter(|(index, _)| *index >= start)
                .collect()
        })
}

fn read_side_table_rows(
    conn: &Connection,
    table: SideTable,
) -> Result<Vec<(u64, serde_json::Value)>> {
    read_side_table_rows_from(conn, table, 0)
}

fn read_side_table_rows_from(
    conn: &Connection,
    table: SideTable,
    start: u64,
) -> Result<Vec<(u64, serde_json::Value)>> {
    let sql = format!(
        "SELECT {}, {} FROM {} WHERE {} >= ?1 ORDER BY {}",
        table.index_column(),
        table.value_column(),
        table.name(),
        table.index_column(),
        table.index_column(),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        [checked_sql_coordinate(start, table.index_column())?],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )?;
    rows.map(|row| {
        let (index, json) = row?;
        let index = u64::try_from(index)
            .map_err(|_| StoreError::Integrity(format!("negative {}", table.index_column())))?;
        Ok((index, serde_json::from_str(&json)?))
    })
    .collect()
}

fn replace_side_table_suffix(
    conn: &Connection,
    table: SideTable,
    start: u64,
    rows: &[(u64, serde_json::Value)],
) -> Result<()> {
    let delete_sql = format!(
        "DELETE FROM {} WHERE {} >= ?1",
        table.name(),
        table.index_column()
    );
    let deleted = conn.execute(
        &delete_sql,
        [checked_sql_coordinate(start, table.index_column())?],
    )?;
    let insert_sql = format!(
        "INSERT INTO {} ({}, {}) VALUES (?1, ?2)",
        table.name(),
        table.index_column(),
        table.value_column()
    );
    for (index, value) in rows {
        conn.execute(
            &insert_sql,
            rusqlite::params![
                checked_sql_coordinate(*index, table.index_column())?,
                serde_json::to_string(value)?
            ],
        )?;
    }
    smelt_perf::perf::record_value("store:session:side_table_rows_deleted", deleted as u64);
    smelt_perf::perf::record_value("store:session:side_table_rows_inserted", rows.len() as u64);
    Ok(())
}

fn missing_object_references(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT refs.object_hash
         FROM (
             SELECT object_hash FROM history_object_refs
             UNION ALL
             SELECT object_hash FROM request_object_refs
         ) refs
         LEFT JOIN objects ON objects.hash = refs.object_hash
         WHERE objects.hash IS NULL
         ORDER BY refs.object_hash",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn read_descriptor_tail_for_rows(
    conn: &Connection,
    total_count: usize,
    width: u16,
    target_rows: u16,
) -> Result<TranscriptDescriptorSlice> {
    if total_count == 0 {
        return history::read_transcript_descriptor_tail_slice_with_total(conn, 0, 0);
    }

    let target_rows = u64::from(target_rows.max(1));
    let mut count = target_rows
        .saturating_add(1)
        .saturating_div(2)
        .min(total_count as u64) as usize;
    let mut probes = 0u64;
    loop {
        probes = probes.saturating_add(1);
        smelt_perf::perf::record_value("transcript:resume_tail:tail_probe_count", count as u64);
        let slice =
            history::read_transcript_descriptor_tail_slice_with_total(conn, total_count, count)?;
        if estimated_descriptor_rows(&slice.records, width) >= target_rows || count == total_count {
            smelt_perf::perf::record_value("transcript:resume_tail:tail_probes", probes);
            return Ok(slice);
        }
        count = count.saturating_mul(2).min(total_count);
    }
}

fn estimated_descriptor_rows(records: &[TranscriptDescriptorRecord], width: u16) -> u64 {
    let width = u64::from(width.max(1));
    records
        .iter()
        .map(|record| {
            let text_rows = record.estimated_text_bytes.saturating_add(width - 1) / width;
            text_rows.max(1).saturating_add(1)
        })
        .sum()
}

pub(crate) fn session_commit_failure_from_store_error(err: StoreError) -> SessionCommitFailure {
    let disposition = err.session_persistence_disposition();
    match err {
        StoreError::OwnershipLost => SessionCommitFailure::OwnershipLost,
        StoreError::Busy {
            operation,
            attempts,
            waited_ms,
        } => SessionCommitFailure::Busy {
            operation: operation.to_string(),
            attempts,
            waited_ms,
        },
        StoreError::UnsupportedSchema { found, expected } => {
            SessionCommitFailure::UnsupportedSchema { found, expected }
        }
        StoreError::Json(err) => SessionCommitFailure::InvalidCommand {
            message: err.to_string(),
        },
        StoreError::ObjectTooLarge { size, max } => SessionCommitFailure::InvalidCommand {
            message: format!("session object is too large: {size} bytes exceeds {max}"),
        },
        StoreError::Integrity(message) | StoreError::MissingObject { reference: message } => {
            SessionCommitFailure::Integrity { message }
        }
        StoreError::Io(err) => SessionCommitFailure::Io {
            message: err.to_string(),
            disposition,
        },
        StoreError::Sqlite(err) => SessionCommitFailure::Sqlite {
            message: err.to_string(),
            disposition,
        },
        StoreError::TransactionCleanup { operation, message } => SessionCommitFailure::Sqlite {
            message: format!("transaction cleanup failed during {operation}: {message}"),
            disposition,
        },
        StoreError::OperationCleanup {
            operation,
            primary,
            cleanup,
        } => SessionCommitFailure::Sqlite {
            message: format!(
                "{operation} failed: {primary}; cleanup also failed: {}",
                cleanup
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            disposition,
        },
        StoreError::OwnershipConflict { .. } => SessionCommitFailure::OwnershipLost,
    }
}

#[derive(Debug)]
struct PreparedSessionCommit {
    fingerprint: String,
    history_start: usize,
    history_final_len: usize,
    history_hashes: Vec<String>,
    #[cfg(debug_assertions)]
    history_object_hashes: Vec<Vec<String>>,
    descriptor_start: Option<usize>,
    #[cfg(debug_assertions)]
    descriptor_object_hashes: Vec<String>,
    side_tables: PreparedSideTables,
}

fn prepare_session_commit(
    command: &SessionCommit,
) -> std::result::Result<PreparedSessionCommit, SessionCommitFailure> {
    if command.identity.id != command.session_id {
        return Err(SessionCommitFailure::SessionMismatch {
            expected: command.session_id.clone(),
            actual: Some(command.identity.id.clone()),
        });
    }
    validate_sql_coordinate(command.expected.revision.get(), "expected revision")?;
    validate_sql_coordinate(
        command.expected.history_len.get(),
        "expected history length",
    )?;
    validate_sql_coordinate(
        command.expected.descriptor_len.get(),
        "expected descriptor length",
    )?;
    validate_sql_coordinate(command.history.start.get(), "history start")?;
    validate_sql_coordinate(command.history.final_len.get(), "history final length")?;
    validate_metadata_coordinates(&command.metadata)?;
    meta::validate_session_checkpoint(&command.metadata, command.history.final_len.get())
        .map_err(session_commit_failure_from_store_error)?;

    let history_start = history_index_usize(command.history.start)?;
    let history_final_len = history_len_usize(command.history.final_len)?;
    if history_start.checked_add(command.history.items.len()) != Some(history_final_len) {
        return Err(SessionCommitFailure::InvalidHistorySuffix {
            start: command.history.start,
            final_len: command.history.final_len,
            item_count: u64::try_from(command.history.items.len()).unwrap_or(u64::MAX),
        });
    }
    let history_hashes = command
        .history
        .items
        .iter()
        .map(history::item_hash)
        .collect::<Result<Vec<_>>>()
        .map_err(session_commit_failure_from_store_error)?;

    validate_side_table_suffixes(command)?;
    let side_tables = PreparedSideTables::new(&command.side_tables)
        .map_err(session_commit_failure_from_store_error)?;
    let descriptor_start = command
        .descriptors
        .as_ref()
        .map(|suffix| descriptor_index_usize(suffix.start))
        .transpose()?;
    if let Some(descriptors) = &command.descriptors {
        validate_sql_coordinate(descriptors.start.get(), "descriptor start")?;
        let final_len = descriptors
            .start
            .get()
            .checked_add(u64::try_from(descriptors.records.len()).map_err(|_| {
                SessionCommitFailure::Integrity {
                    message: "descriptor item count exceeds u64".into(),
                }
            })?)
            .ok_or_else(|| SessionCommitFailure::Integrity {
                message: "descriptor suffix length overflows u64".into(),
            })?;
        validate_sql_coordinate(final_len, "descriptor final length")?;
        usize::try_from(final_len).map_err(|_| SessionCommitFailure::Integrity {
            message: "descriptor final length exceeds platform limits".into(),
        })?;
        for record in &descriptors.records {
            validate_sql_coordinate(record.block_idx, "descriptor block index")?;
            if let Some(history_idx) = record.history_idx {
                validate_sql_coordinate(history_idx, "descriptor history index")?;
            }
            validate_sql_coordinate(
                record.estimated_text_bytes,
                "descriptor estimated text bytes",
            )?;
            serde_json::from_str::<serde_json::Value>(&record.descriptor_json)
                .map_err(StoreError::from)
                .map_err(session_commit_failure_from_store_error)?;
            if let Some(tool_state_json) = &record.tool_state_json {
                serde_json::from_str::<serde_json::Value>(tool_state_json)
                    .map_err(StoreError::from)
                    .map_err(session_commit_failure_from_store_error)?;
            }
        }
    }
    #[cfg(debug_assertions)]
    let history_object_hashes = command
        .history
        .items
        .iter()
        .map(|item| history::incoming_object_hashes(std::slice::from_ref(item), None))
        .collect::<Result<Vec<_>>>()
        .map_err(session_commit_failure_from_store_error)?;
    #[cfg(debug_assertions)]
    let descriptor_object_hashes = history::incoming_object_hashes(
        &[],
        command
            .descriptors
            .as_ref()
            .map(|suffix| suffix.records.as_slice()),
    )
    .map_err(session_commit_failure_from_store_error)?;
    let fingerprint = canonical_session_commit_fingerprint(command, &side_tables)
        .map_err(session_commit_failure_from_store_error)?;
    Ok(PreparedSessionCommit {
        fingerprint,
        history_start,
        history_final_len,
        history_hashes,
        #[cfg(debug_assertions)]
        history_object_hashes,
        descriptor_start,
        #[cfg(debug_assertions)]
        descriptor_object_hashes,
        side_tables,
    })
}

fn apply_session_commit_in_transaction(
    conn: &Connection,
    command: &SessionCommit,
    prepared: &PreparedSessionCommit,
    owner_token: Option<&str>,
    compression: ObjectCompression,
) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
    if let Some(token) = owner_token {
        meta::verify_writer_owner(conn, token).map_err(session_commit_failure_from_store_error)?;
    }
    if let Some(mut receipt) = idempotent_session_commit_receipt(conn, &prepared.fingerprint)
        .map_err(session_commit_failure_from_store_error)?
    {
        let current = current_store_head(conn).map_err(session_commit_failure_from_store_error)?;
        if receipt.previous != command.expected || receipt.current != current {
            return Err(SessionCommitFailure::Integrity {
                message: "persisted commit receipt does not match the command or store head".into(),
            });
        }
        receipt.save_id = command.save_id;
        return Ok(receipt);
    }

    let current_session =
        meta::stored_session(conn).map_err(session_commit_failure_from_store_error)?;
    let current = current_store_head_from(conn, current_session.as_ref())
        .map_err(session_commit_failure_from_store_error)?;
    if command.expected != current {
        return Err(SessionCommitFailure::StaleBase {
            expected: command.expected,
            current,
        });
    }
    if let Some(stored) = &current_session {
        if stored.identity != command.identity {
            return Err(SessionCommitFailure::IdentityMismatch {
                stored: stored.identity.clone(),
                attempted: command.identity.clone(),
            });
        }
    }
    if prepared.history_start
        > current
            .history_len
            .as_usize()
            .ok_or_else(|| SessionCommitFailure::Integrity {
                message: "current history length exceeds platform limits".into(),
            })?
    {
        return Err(SessionCommitFailure::InvalidHistorySuffix {
            start: command.history.start,
            final_len: command.history.final_len,
            item_count: u64::try_from(command.history.items.len()).unwrap_or(u64::MAX),
        });
    }
    if let Some(start) = prepared.descriptor_start {
        if start
            > current
                .descriptor_len
                .as_usize()
                .expect("SQLite descriptor length fits usize")
        {
            return Err(SessionCommitFailure::InvalidDescriptorSuffix {
                start: command.descriptors.as_ref().expect("prepared suffix").start,
                current_len: current.descriptor_len,
            });
        }
    }
    if let Some(descriptors) = &command.descriptors {
        validate_descriptor_suffix_history_links(conn, &command.history, descriptors)
            .map_err(session_commit_failure_from_store_error)?;
    }

    let current_hashes = history::history_hashes_from(conn, prepared.history_start)
        .map_err(session_commit_failure_from_store_error)?;
    let common_suffix_len = current_hashes
        .iter()
        .zip(&prepared.history_hashes)
        .take_while(|(current, next)| current.hash == **next)
        .count();
    let history_changed = common_suffix_len != current_hashes.len()
        || common_suffix_len != prepared.history_hashes.len();
    let history_replace_from = prepared.history_start + common_suffix_len;
    let incoming_offset = common_suffix_len;

    let metadata_changed = current_session
        .as_ref()
        .is_none_or(|stored| stored.metadata != command.metadata);
    let side_table_changes = prepared
        .side_tables
        .changes(conn)
        .map_err(session_commit_failure_from_store_error)?;
    let descriptors_changed = match (&command.descriptors, prepared.descriptor_start) {
        (Some(descriptors), Some(start)) => !history::transcript_descriptor_suffix_matches(
            conn,
            start,
            &descriptors.records,
            compression,
        )
        .map_err(session_commit_failure_from_store_error)?,
        _ => false,
    };
    let changed = current_session.is_none()
        || metadata_changed
        || history_changed
        || side_table_changes.any()
        || descriptors_changed;
    let revision = if changed {
        current
            .revision
            .checked_add(1)
            .filter(|revision| revision.get() <= i64::MAX as u64)
            .ok_or_else(|| SessionCommitFailure::Integrity {
                message: "session revision exceeds SQLite integer range".into(),
            })?
    } else {
        current.revision
    };
    #[cfg(debug_assertions)]
    let changed_object_hashes = {
        let mut hashes = std::collections::BTreeSet::new();
        if history_changed {
            hashes.extend(
                prepared.history_object_hashes[incoming_offset..]
                    .iter()
                    .flatten()
                    .cloned(),
            );
        }
        if descriptors_changed {
            hashes.extend(prepared.descriptor_object_hashes.iter().cloned());
        }
        hashes
    };

    if history_changed {
        history::replace_history_suffix(
            conn,
            history_replace_from,
            &command.history.items[incoming_offset..],
            compression,
        )
        .map_err(session_commit_failure_from_store_error)?;
    }
    prepared
        .side_tables
        .apply_changes(conn, side_table_changes)
        .map_err(session_commit_failure_from_store_error)?;
    if descriptors_changed {
        let descriptors = command.descriptors.as_ref().expect("changed suffix");
        history::replace_transcript_descriptor_suffix_in_transaction(
            conn,
            prepared.descriptor_start.expect("changed suffix start"),
            &descriptors.records,
            compression,
        )
        .map_err(session_commit_failure_from_store_error)?;
    }
    meta::write_session(
        conn,
        &command.identity,
        &command.metadata,
        revision.get(),
        command.history.final_len.get(),
    )
    .map_err(session_commit_failure_from_store_error)?;

    validate_session_commit_invariants(conn).map_err(session_commit_failure_from_store_error)?;
    #[cfg(debug_assertions)]
    validate_object_payload_hashes(conn, &changed_object_hashes)
        .map_err(session_commit_failure_from_store_error)?;

    let descriptor_len = history::transcript_descriptor_count(conn)
        .map_err(session_commit_failure_from_store_error)?;
    let descriptor_len =
        u64::try_from(descriptor_len).map_err(|_| SessionCommitFailure::Integrity {
            message: "descriptor length exceeds u64".into(),
        })?;
    let current = StoreHead {
        revision,
        history_len: command.history.final_len,
        descriptor_len: DescriptorLen::new(descriptor_len),
    };
    let receipt = SaveReceipt {
        session_id: command.session_id.clone(),
        save_id: command.save_id,
        previous: command.expected,
        current,
    };
    let persisted = PersistedSessionCommit {
        fingerprint: prepared.fingerprint.clone(),
        receipt: receipt.clone(),
    };
    let persisted = serde_json::to_string(&persisted)
        .map_err(StoreError::from)
        .map_err(session_commit_failure_from_store_error)?;
    meta::set_meta(conn, LAST_SESSION_COMMIT_KEY, &persisted)
        .map_err(session_commit_failure_from_store_error)?;
    record_session_commit_metrics(
        history_replace_from,
        prepared.history_final_len,
        command.expected.history_len.get(),
        changed,
    );
    Ok(receipt)
}

#[cfg(test)]
pub(crate) fn session_commit_fingerprint(command: &SessionCommit) -> Result<String> {
    prepare_session_commit(command)
        .map(|prepared| prepared.fingerprint)
        .map_err(|failure| StoreError::Integrity(format!("invalid session commit: {failure:?}")))
}

fn canonical_session_commit_fingerprint(
    command: &SessionCommit,
    side_tables: &PreparedSideTables,
) -> Result<String> {
    let mut encoder = CanonicalEncoder::new(b"smelt-session-commit-v1\0");
    encoder.string(&command.session_id);
    encode_store_head(&mut encoder, command.expected);
    encoder.string(&command.identity.id);
    encoder.i64(command.identity.created_at);
    encoder.optional_string(command.identity.parent_id.as_deref());
    encode_session_metadata(&mut encoder, &command.metadata)?;
    encoder.u64(command.history.start.get());
    encoder.u64(command.history.final_len.get());
    encoder.u64(command.history.items.len() as u64);
    for item in &command.history.items {
        encoder.json(item)?;
    }
    encoder.u64(side_tables.start);
    encode_side_table_rows(&mut encoder, &side_tables.turn_metas)?;
    encode_side_table_rows(&mut encoder, &side_tables.metadata_snapshots)?;
    encode_side_table_rows(&mut encoder, &side_tables.context_snapshots)?;
    match &command.descriptors {
        Some(suffix) => {
            encoder.bool(true);
            encoder.u64(suffix.start.get());
            encoder.u64(suffix.records.len() as u64);
            for record in &suffix.records {
                encoder.u64(record.block_idx);
                encoder.optional_u64(record.history_idx);
                encoder.string(&record.kind);
                encoder.optional_string(record.tool_call_id.as_deref());
                encoder.optional_string(record.tool_name.as_deref());
                encoder.string(&record.content_hash);
                encoder.u64(record.estimated_text_bytes);
                encoder.string(&record.preview_text);
                encoder.string(&record.indexed_text);
                encoder.json_text(&record.descriptor_json)?;
                encoder.optional_json_text(record.origin_json.as_deref())?;
                encoder.optional_json_text(record.tool_state_json.as_deref())?;
            }
        }
        None => encoder.bool(false),
    }
    Ok(crate::object::sha256_hex(&encoder.finish()))
}

fn encode_store_head(encoder: &mut CanonicalEncoder, head: StoreHead) {
    encoder.u64(head.revision.get());
    encoder.u64(head.history_len.get());
    encoder.u64(head.descriptor_len.get());
}

fn encode_session_metadata(
    encoder: &mut CanonicalEncoder,
    metadata: &SessionMetadata,
) -> Result<()> {
    encoder.optional_string(metadata.title.as_deref());
    encoder.optional_string(metadata.slug.as_deref());
    encoder.optional_string(metadata.first_user_message.as_deref());
    encoder.optional_string(metadata.cwd.as_deref());
    encoder.optional_string(metadata.mode.as_deref());
    encoder.optional_string(metadata.reasoning_effort.as_deref());
    encoder.optional_string(metadata.model.as_deref());
    match metadata.fast_mode {
        Some(value) => {
            encoder.bool(true);
            encoder.bool(value);
        }
        None => encoder.bool(false),
    }
    encoder.optional_json(metadata.accounting_json.as_ref())?;
    encoder.optional_json(metadata.checkpoint_json.as_ref())?;
    encoder.optional_u64(metadata.context_tokens);
    encoder.optional_u64(metadata.context_tokens_history_len);
    encoder.optional_u64(metadata.display_context_tokens);
    encoder.u64(metadata.session_cost_usd.normalized_bits());
    encoder.i64(metadata.updated_at);
    Ok(())
}

fn encode_side_table_rows(
    encoder: &mut CanonicalEncoder,
    rows: &[(u64, serde_json::Value)],
) -> Result<()> {
    encoder.u64(rows.len() as u64);
    for (index, value) in rows {
        encoder.u64(*index);
        encoder.json(value)?;
    }
    Ok(())
}

struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    fn new(version: &[u8]) -> Self {
        Self {
            bytes: version.to_vec(),
        }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn string(&mut self, value: &str) {
        self.u64(value.len() as u64);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    fn optional_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.bool(true);
                self.string(value);
            }
            None => self.bool(false),
        }
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.bool(true);
                self.u64(value);
            }
            None => self.bool(false),
        }
    }

    fn json(&mut self, value: &impl serde::Serialize) -> Result<()> {
        let value = serde_json::to_value(value)?;
        let mut bytes = Vec::new();
        write_canonical_json(&value, &mut bytes)?;
        self.u64(bytes.len() as u64);
        self.bytes.extend_from_slice(&bytes);
        Ok(())
    }

    fn optional_json(&mut self, value: Option<&serde_json::Value>) -> Result<()> {
        match value {
            Some(value) => {
                self.bool(true);
                self.json(value)
            }
            None => {
                self.bool(false);
                Ok(())
            }
        }
    }

    fn json_text(&mut self, value: &str) -> Result<()> {
        let value = serde_json::from_str::<serde_json::Value>(value)?;
        self.json(&value)
    }

    fn optional_json_text(&mut self, value: Option<&str>) -> Result<()> {
        match value {
            Some(value) => {
                self.bool(true);
                self.json_text(value)
            }
            None => {
                self.bool(false);
                Ok(())
            }
        }
    }
}

fn write_canonical_json(value: &serde_json::Value, out: &mut Vec<u8>) -> Result<()> {
    use std::io::Write;

    match value {
        serde_json::Value::Null => out.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => {
            out.extend_from_slice(if *value { b"true" } else { b"false" })
        }
        serde_json::Value::Number(value) => write!(out, "{value}")?,
        serde_json::Value::String(value) => serde_json::to_writer(out, value)?,
        serde_json::Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(b']');
        }
        serde_json::Value::Object(values) => {
            out.push(b'{');
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write_canonical_json(value, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn validate_metadata_coordinates(
    metadata: &SessionMetadata,
) -> std::result::Result<(), SessionCommitFailure> {
    for (value, field) in [
        (metadata.context_tokens, "context_tokens"),
        (
            metadata.context_tokens_history_len,
            "context_tokens_history_len",
        ),
        (metadata.display_context_tokens, "display_context_tokens"),
    ] {
        if let Some(value) = value {
            validate_sql_coordinate(value, field)?;
        }
    }
    Ok(())
}

fn validate_sql_coordinate(
    value: u64,
    field: &str,
) -> std::result::Result<(), SessionCommitFailure> {
    checked_sql_coordinate(value, field).map_err(session_commit_failure_from_store_error)?;
    usize::try_from(value).map_err(|_| SessionCommitFailure::Integrity {
        message: format!("{field} exceeds platform limits"),
    })?;
    Ok(())
}

fn checked_sql_coordinate(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StoreError::Integrity(format!("{field} exceeds SQLite integer range")))
}

fn current_store_head(conn: &Connection) -> Result<StoreHead> {
    let session = meta::stored_session(conn)?;
    current_store_head_from(conn, session.as_ref())
}

fn current_store_head_from(
    conn: &Connection,
    session: Option<&meta::PersistedSession>,
) -> Result<StoreHead> {
    let descriptor_len = u64::try_from(history::transcript_descriptor_count(conn)?)
        .map_err(|_| StoreError::Integrity("descriptor length exceeds u64".into()))?;
    Ok(StoreHead {
        revision: session.map_or(0, |session| session.revision).into(),
        history_len: session.map_or(0, |session| session.history_len).into(),
        descriptor_len: descriptor_len.into(),
    })
}

fn record_session_commit_metrics(
    unchanged: usize,
    final_len: usize,
    previous_len: u64,
    changed: bool,
) {
    smelt_perf::perf::record_value("store:session:history_rows_unchanged", unchanged as u64);
    smelt_perf::perf::record_value(
        "store:session:history_rows_deleted",
        previous_len.saturating_sub(unchanged as u64),
    );
    smelt_perf::perf::record_value(
        "store:session:history_rows_inserted",
        (final_len.saturating_sub(unchanged)) as u64,
    );
    smelt_perf::perf::record_value(
        "store:session:db_writes_changed",
        if changed { 1 } else { 0 },
    );
}

fn persisted_session_commit_value(conn: &Connection) -> Result<Option<serde_json::Value>> {
    meta::meta(conn, LAST_SESSION_COMMIT_KEY)?
        .map(|persisted| {
            serde_json::from_str(&persisted).map_err(|err| {
                StoreError::Integrity(format!("invalid persisted session commit: {err}"))
            })
        })
        .transpose()
}

fn persisted_session_commit_fingerprint(conn: &Connection) -> Result<Option<String>> {
    persisted_session_commit_value(conn)?
        .map(|value| {
            value
                .get("fingerprint")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    StoreError::Integrity(
                        "persisted session commit has no string fingerprint".into(),
                    )
                })
        })
        .transpose()
}

fn persisted_session_commit(conn: &Connection) -> Result<Option<PersistedSessionCommit>> {
    persisted_session_commit_value(conn)?
        .map(|value| {
            serde_json::from_value(value).map_err(|err| {
                StoreError::Integrity(format!("invalid persisted session commit: {err}"))
            })
        })
        .transpose()
}

fn idempotent_session_commit_receipt(
    conn: &Connection,
    fingerprint: &str,
) -> Result<Option<SaveReceipt>> {
    let Some(value) = persisted_session_commit_value(conn)? else {
        return Ok(None);
    };
    if value.get("fingerprint").and_then(serde_json::Value::as_str) != Some(fingerprint) {
        return Ok(None);
    }
    let receipt = value
        .get("receipt")
        .cloned()
        .ok_or_else(|| StoreError::Integrity("persisted session commit has no receipt".into()))?;
    serde_json::from_value(receipt)
        .map(Some)
        .map_err(|err| StoreError::Integrity(format!("invalid persisted commit receipt: {err}")))
}

fn validate_side_table_suffixes(
    command: &SessionCommit,
) -> std::result::Result<(), SessionCommitFailure> {
    history_index_usize(command.side_tables.start)?;
    if command.side_tables.start.get() > command.history.final_len.get() {
        return Err(SessionCommitFailure::InvalidSideTableSuffix {
            start: command.side_tables.start,
            final_len: command.history.final_len,
        });
    }
    validate_side_table_rows(
        "turn_metas",
        &command.side_tables.turn_metas,
        command.history.final_len,
        true,
    )?;
    validate_side_table_rows(
        "metadata_snapshots",
        &command.side_tables.metadata_snapshots,
        command.history.final_len,
        true,
    )?;
    validate_side_table_rows(
        "accounting_snapshots",
        &command.side_tables.context_snapshots,
        command.history.final_len,
        true,
    )
}

fn validate_side_table_rows(
    table: &str,
    rows: &[(HistoryIndex, serde_json::Value)],
    final_len: HistoryLen,
    include_boundary: bool,
) -> std::result::Result<(), SessionCommitFailure> {
    for (idx, _) in rows {
        let within_bounds = if include_boundary {
            idx.get() <= final_len.get()
        } else {
            idx.get() < final_len.get()
        };
        if !within_bounds {
            return Err(SessionCommitFailure::InvalidSideTableRow {
                table: table.to_string(),
                index: *idx,
                final_len,
                bound: if include_boundary {
                    HistoryIndexBound::AtOrBeforeFinalLen
                } else {
                    HistoryIndexBound::BeforeFinalLen
                },
            });
        }
    }
    Ok(())
}

fn history_index_usize(value: HistoryIndex) -> std::result::Result<usize, SessionCommitFailure> {
    value
        .as_usize()
        .ok_or_else(|| SessionCommitFailure::Integrity {
            message: format!("history index {} does not fit usize", value.get()),
        })
}

fn history_len_usize(value: HistoryLen) -> std::result::Result<usize, SessionCommitFailure> {
    value
        .as_usize()
        .ok_or_else(|| SessionCommitFailure::Integrity {
            message: format!("history length {} does not fit usize", value.get()),
        })
}

fn descriptor_index_usize(
    value: DescriptorIndex,
) -> std::result::Result<usize, SessionCommitFailure> {
    value
        .as_usize()
        .ok_or_else(|| SessionCommitFailure::Integrity {
            message: format!("descriptor index {} does not fit usize", value.get()),
        })
}

fn validate_session_commit_invariants(conn: &Connection) -> Result<()> {
    let Some(session) = meta::stored_session(conn)? else {
        return Ok(());
    };
    let history_count = history::history_item_count(conn)? as u64;
    if session.history_len != history_count {
        return Err(StoreError::Integrity(format!(
            "session metadata history_len {} does not match history item count {}",
            session.history_len, history_count
        )));
    }
    meta::validate_session_checkpoint(&session.metadata, history_count)?;
    validate_history_indices_dense(conn, history_count)?;
    validate_transcript_descriptor_indices_dense(conn)?;
    validate_transcript_descriptor_history_bounds(conn, history_count)?;
    validate_side_table_history_bounds(conn, history_count)?;
    validate_history_object_refs(conn, history_count)?;
    Ok(())
}

fn validate_history_indices_dense(conn: &Connection, history_count: u64) -> Result<()> {
    let max_idx = conn
        .query_row("SELECT MAX(idx) FROM history_items", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .map_err(StoreError::from)?;
    let expected_max = history_count.checked_sub(1).map(|idx| idx as i64);
    if max_idx != expected_max {
        return Err(StoreError::Integrity(format!(
            "history item indices are not dense: count {history_count}, max_idx {max_idx:?}"
        )));
    }
    Ok(())
}

fn validate_transcript_descriptor_indices_dense(conn: &Connection) -> Result<()> {
    let (count, max_idx): (i64, Option<i64>) = conn
        .query_row(
            "SELECT COUNT(*), MAX(descriptor_idx)
             FROM transcript_blocks
             WHERE descriptor_json IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(StoreError::from)?;
    let expected_max = (count > 0).then_some(count - 1);
    if max_idx != expected_max {
        return Err(StoreError::Integrity(format!(
            "transcript descriptor indices are not dense: count {count}, max_idx {max_idx:?}"
        )));
    }
    Ok(())
}

fn validate_transcript_descriptor_history_bounds(
    conn: &Connection,
    history_count: u64,
) -> Result<()> {
    let invalid: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM transcript_blocks
             WHERE descriptor_json IS NOT NULL
               AND history_idx IS NOT NULL
               AND history_idx >= ?1",
            [history_count as i64],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    if invalid != 0 {
        return Err(StoreError::Integrity(format!(
            "transcript descriptors point past history length: invalid {invalid}, history_len {history_count}"
        )));
    }
    Ok(())
}

fn validate_side_table_history_bounds(conn: &Connection, history_count: u64) -> Result<()> {
    let turn_meta_invalid: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM turn_metas WHERE turn_idx > ?1",
            [history_count as i64],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    if turn_meta_invalid != 0 {
        return Err(StoreError::Integrity(format!(
            "turn_metas contains rows past history length: invalid {turn_meta_invalid}, history_len {history_count}"
        )));
    }

    for (table, column) in [
        ("metadata_snapshots", "history_idx"),
        ("accounting_snapshots", "history_idx"),
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} > ?1");
        let invalid: i64 = conn
            .query_row(&sql, [history_count as i64], |row| row.get(0))
            .map_err(StoreError::from)?;
        if invalid != 0 {
            return Err(StoreError::Integrity(format!(
                "{table} contains rows past history length: invalid {invalid}, history_len {history_count}"
            )));
        }
    }
    Ok(())
}

fn validate_history_object_refs(conn: &Connection, history_count: u64) -> Result<()> {
    let missing_history: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM history_object_refs refs
             LEFT JOIN history_items history ON history.idx = refs.history_idx
             WHERE history.idx IS NULL OR refs.history_idx >= ?1",
            [history_count as i64],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    if missing_history != 0 {
        return Err(StoreError::Integrity(format!(
            "history object refs point outside history rows: invalid {missing_history}, history_len {history_count}"
        )));
    }

    let missing_objects: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM history_object_refs refs
             LEFT JOIN objects objects ON objects.hash = refs.object_hash
             WHERE objects.hash IS NULL",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    if missing_objects != 0 {
        return Err(StoreError::Integrity(format!(
            "history object refs point to missing objects: invalid {missing_objects}"
        )));
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn validate_object_payload_hashes(
    conn: &Connection,
    hashes: &std::collections::BTreeSet<String>,
) -> Result<()> {
    for hash in hashes {
        object::object_bytes_by_hash(conn, hash)?.ok_or_else(|| {
            StoreError::Integrity(format!("object {hash} disappeared during validation"))
        })?;
    }
    Ok(())
}

fn validate_descriptor_suffix_history_links(
    conn: &Connection,
    history: &crate::HistorySuffix,
    descriptors: &crate::TranscriptDescriptorSuffix,
) -> Result<()> {
    let start = history.start.get();
    let end = history.final_len.get();
    for record in &descriptors.records {
        let Some(history_idx) = record.history_idx else {
            continue;
        };
        if history_idx >= end {
            return Err(StoreError::Integrity(format!(
                "transcript descriptor history link past saved history: history_idx {history_idx}, history_len {end}"
            )));
        }
        let matches_history = if history_idx < start {
            let history_kind = persisted_history_kind(conn, history_idx)?;
            let Some(history_kind) = history_kind else {
                return Err(StoreError::Integrity(format!(
                    "transcript descriptor history link missing from stored prefix: history_idx {history_idx}, suffix {start}..{end}"
                )));
            };
            descriptor_kind_matches_history_kind(&record.kind, &history_kind)
        } else {
            let suffix_offset = (history_idx - start) as usize;
            let Some(item) = history.items.get(suffix_offset) else {
                return Err(StoreError::Integrity(format!(
                    "transcript descriptor history link missing from saved suffix: history_idx {history_idx}, suffix {start}..{end}"
                )));
            };
            descriptor_kind_matches_history_item(&record.kind, item)
        };
        if !matches_history {
            return Err(StoreError::Integrity(format!(
                "transcript descriptor history link kind mismatch: descriptor kind {}, history_idx {history_idx}",
                record.kind
            )));
        }
    }
    Ok(())
}

fn persisted_history_kind(conn: &Connection, history_idx: u64) -> Result<Option<String>> {
    let history_idx = crate::object::checked_i64(history_idx, "history_idx")?;
    conn.query_row(
        "SELECT kind FROM history_items WHERE idx = ?1",
        [history_idx],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn descriptor_kind_matches_history_kind(kind: &str, history_kind: &str) -> bool {
    matches!(
        (kind, history_kind),
        ("user", "user")
            | (
                "assistant" | "thinking" | "tool" | "exec" | "code",
                "assistant"
            )
    )
}

fn descriptor_kind_matches_history_item(kind: &str, item: &protocol::HistoryItem) -> bool {
    matches!(
        (kind, item),
        ("user", protocol::HistoryItem::User { .. })
            | (
                "assistant" | "thinking" | "tool" | "exec" | "code",
                protocol::HistoryItem::Assistant(_),
            )
    )
}

fn prepare_writable_path(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(StoreError::Integrity(
            "session database path has no parent".into(),
        ));
    };
    reject_symlink(parent)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)?;
    reject_symlink(parent)?;
    reject_symlink(path)?;
    create_private_file(path)?;
    secure_directory(parent)?;
    secure_sqlite_files(path)?;
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing symlinked storage path {}", path.display()),
            )))
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn create_private_file(path: &Path) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => reject_symlink(path),
        Err(err) => Err(err.into()),
    }
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<()> {
    Ok(())
}

fn nonnegative_sql_value(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is negative: {value}")))
}

fn file_size(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err.into()),
    }
}

fn sqlite_companion_path(path: &Path, suffix: &str) -> PathBuf {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return path.with_extension(suffix.trim_start_matches('-'));
    };
    path.with_file_name(format!("{name}{suffix}"))
}

fn secure_sqlite_files(path: &Path) -> Result<()> {
    reject_symlink(path)?;
    secure_file(path)?;
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    for suffix in ["-wal", "-shm"] {
        let companion = path.with_file_name(format!("{name}{suffix}"));
        reject_symlink(&companion)?;
        secure_file(&companion)?;
    }
    Ok(())
}

fn rollback_after_commit_failure(mut tx: Transaction<'_>) -> rusqlite::Result<()> {
    if tx.is_autocommit() {
        tx.set_drop_behavior(DropBehavior::Ignore);
        Ok(())
    } else {
        tx.rollback()
    }
}

fn sqlite_error_is_locked(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(err, _)
            if matches!(
                err.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn apply_pragmas(conn: &Connection, mode: OpenMode) -> Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    conn.busy_timeout(SESSION_BUSY_TIMEOUT)?;
    match mode {
        OpenMode::CreateOrMigrate | OpenMode::CurrentWriter => conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT_BYTES};
             PRAGMA wal_autocheckpoint = {WAL_AUTOCHECKPOINT_PAGES};"
        ))?,
        OpenMode::ReadOnly => conn.execute_batch("PRAGMA query_only = ON;")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::{
        benchmark_zstd_compression, HistorySuffix, ObjectCodec, RequestAuditOrder,
        RequestAuditPayloadMode, Revision, SaveId, SideTableSuffixes, TranscriptDescriptorSuffix,
        DEFAULT_ZSTD_LEVEL, DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
    };

    #[derive(Clone, Debug, PartialEq)]
    struct TestSessionModel {
        id: String,
        title: Option<String>,
        slug: Option<String>,
        first_user_message: Option<String>,
        cwd: Option<String>,
        mode: Option<String>,
        reasoning_effort: Option<String>,
        model: Option<String>,
        fast_mode: Option<bool>,
        parent_id: Option<String>,
        accounting_json: Option<serde_json::Value>,
        checkpoint_json: Option<serde_json::Value>,
        context_tokens: Option<u64>,
        context_tokens_history_len: Option<u64>,
        display_context_tokens: Option<u64>,
        session_cost_usd: f64,
        revision: u64,
        history_len: u64,
        created_at: i64,
        updated_at: i64,
    }

    struct TestSessionFixture {
        state: TestSessionModel,
        history_start_idx: usize,
        history_len: usize,
        history: Vec<protocol::HistoryItem>,
        turn_metas: Vec<(u64, serde_json::Value)>,
        metadata_snapshots: Vec<(u64, serde_json::Value)>,
        context_snapshots: Vec<(u64, serde_json::Value)>,
    }

    trait SessionDbTestExt {
        fn apply_test_fixture(
            &mut self,
            fixture: &TestSessionFixture,
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure>;
        fn apply_test_fixture_with_descriptors(
            &mut self,
            fixture: &TestSessionFixture,
            start: usize,
            records: &[TranscriptDescriptorRecord],
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure>;
        fn apply_test_state(
            &mut self,
            state: &TestSessionModel,
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure>;
        fn test_session_model(&self) -> Result<Option<TestSessionModel>>;
        fn apply_test_descriptors(
            &mut self,
            records: &[TranscriptDescriptorRecord],
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure>;
        fn apply_test_descriptor_suffix(
            &mut self,
            start: usize,
            records: &[TranscriptDescriptorRecord],
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure>;
        fn repair_test_transcript_history_links(
            &mut self,
        ) -> std::result::Result<usize, SessionCommitFailure>;
        fn repair_test_checkpoint(&mut self) -> std::result::Result<usize, SessionCommitFailure>;
        fn apply_test_prefix_to(
            &self,
            destination: impl AsRef<Path>,
            state: &TestSessionModel,
            history_len: usize,
        ) -> Result<()>;
    }

    impl SessionDbTestExt for SessionDb {
        fn apply_test_fixture(
            &mut self,
            fixture: &TestSessionFixture,
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
            let expected = self
                .store_head()
                .map_err(session_commit_failure_from_store_error)?;
            let command = test_fixture_command(expected, fixture, None)?;
            self.apply_session_commit(&command)
        }

        fn apply_test_fixture_with_descriptors(
            &mut self,
            fixture: &TestSessionFixture,
            start: usize,
            records: &[TranscriptDescriptorRecord],
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
            let expected = self
                .store_head()
                .map_err(session_commit_failure_from_store_error)?;
            let descriptors = TranscriptDescriptorSuffix {
                start: DescriptorIndex::try_from(start).map_err(|_| {
                    SessionCommitFailure::Integrity {
                        message: "descriptor start exceeds u64".into(),
                    }
                })?,
                records: records.to_vec(),
            };
            let command = test_fixture_command(expected, fixture, Some(descriptors))?;
            self.apply_session_commit(&command)
        }

        fn apply_test_state(
            &mut self,
            state: &TestSessionModel,
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
            let expected = self
                .store_head()
                .map_err(session_commit_failure_from_store_error)?;
            let history_len =
                expected
                    .history_len
                    .as_usize()
                    .ok_or_else(|| SessionCommitFailure::Integrity {
                        message: "history length exceeds usize".into(),
                    })?;
            let fixture = TestSessionFixture {
                state: state.clone(),
                history_start_idx: history_len,
                history_len,
                history: Vec::new(),
                turn_metas: Vec::new(),
                metadata_snapshots: Vec::new(),
                context_snapshots: Vec::new(),
            };
            let command = test_fixture_command(expected, &fixture, None)?;
            self.apply_session_commit(&command)
        }

        fn test_session_model(&self) -> Result<Option<TestSessionModel>> {
            self.stored_session()?
                .map(test_model_from_stored)
                .transpose()
        }

        fn apply_test_descriptors(
            &mut self,
            records: &[TranscriptDescriptorRecord],
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
            self.apply_test_descriptor_suffix(0, records)
        }

        fn apply_test_descriptor_suffix(
            &mut self,
            start: usize,
            records: &[TranscriptDescriptorRecord],
        ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
            self.apply_transcript_descriptor_suffix_fixture(start, records)
        }

        fn repair_test_transcript_history_links(
            &mut self,
        ) -> std::result::Result<usize, SessionCommitFailure> {
            let mut full = self
                .load_full_session()
                .map_err(session_commit_failure_from_store_error)?
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "session metadata is missing".into(),
                })?;
            let mut repaired = 0;
            for record in &mut full.descriptors {
                let matches = record
                    .history_idx
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| full.history.get(index))
                    .is_some_and(|item| {
                        matches!(
                            (record.kind.as_str(), item),
                            ("user", protocol::HistoryItem::User { .. })
                                | (
                                    "assistant" | "thinking" | "tool" | "exec" | "code",
                                    protocol::HistoryItem::Assistant(_),
                                )
                        )
                    });
                if record.history_idx.is_some() && !matches {
                    record.history_idx = None;
                    record.origin_json = None;
                    repaired += 1;
                }
            }
            if repaired != 0 {
                let command = test_full_session_command(
                    &full,
                    Some(TranscriptDescriptorSuffix {
                        start: DescriptorIndex::ZERO,
                        records: full.descriptors.clone(),
                    }),
                )?;
                self.apply_session_commit(&command)?;
            }
            Ok(repaired)
        }

        fn repair_test_checkpoint(&mut self) -> std::result::Result<usize, SessionCommitFailure> {
            let Some((stored, metadata)) = self
                .repaired_checkpoint_metadata()
                .map_err(session_commit_failure_from_store_error)?
            else {
                return Ok(0);
            };
            let full = self
                .load_full_session()
                .map_err(session_commit_failure_from_store_error)?
                .ok_or_else(|| SessionCommitFailure::Integrity {
                    message: "session metadata is missing".into(),
                })?;
            let command =
                test_full_replacement_command(stored.head, stored.identity, metadata, &full)?;
            self.apply_session_commit(&command)?;
            Ok(1)
        }

        fn apply_test_prefix_to(
            &self,
            destination: impl AsRef<Path>,
            state: &TestSessionModel,
            history_len: usize,
        ) -> Result<()> {
            let full = self.load_full_session_prefix(history_len)?.ok_or_else(|| {
                StoreError::Integrity("source session metadata is missing".into())
            })?;
            let mut destination = SessionDb::open(destination)?;
            let command = test_full_replacement_command(
                StoreHead::default(),
                test_identity_from_model(state),
                test_metadata_from_model(state)?,
                &full,
            )
            .map_err(|failure| {
                StoreError::Integrity(format!("invalid prefix fixture: {failure:?}"))
            })?;
            destination
                .apply_session_commit(&command)
                .map_err(|failure| {
                    StoreError::Integrity(format!("prefix commit failed: {failure:?}"))
                })?;
            Ok(())
        }
    }

    fn test_fixture_command(
        expected: StoreHead,
        fixture: &TestSessionFixture,
        descriptors: Option<TranscriptDescriptorSuffix>,
    ) -> std::result::Result<SessionCommit, SessionCommitFailure> {
        let start = u64::try_from(fixture.history_start_idx).map_err(|_| {
            SessionCommitFailure::Integrity {
                message: "history start exceeds u64".into(),
            }
        })?;
        let final_len =
            u64::try_from(fixture.history_len).map_err(|_| SessionCommitFailure::Integrity {
                message: "history length exceeds u64".into(),
            })?;
        Ok(SessionCommit {
            session_id: fixture.state.id.clone(),
            save_id: SaveId::new(expected.revision.get().saturating_add(1)),
            expected,
            identity: test_identity_from_model(&fixture.state),
            metadata: test_metadata_from_model(&fixture.state)
                .map_err(session_commit_failure_from_store_error)?,
            history: HistorySuffix {
                start: HistoryIndex::new(start),
                final_len: HistoryLen::new(final_len),
                items: fixture.history.clone(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(start),
                turn_metas: test_side_table_rows(&fixture.turn_metas),
                metadata_snapshots: test_side_table_rows(&fixture.metadata_snapshots),
                context_snapshots: test_side_table_rows(&fixture.context_snapshots),
            },
            descriptors,
        })
    }

    fn test_full_session_command(
        full: &FullSession,
        descriptors: Option<TranscriptDescriptorSuffix>,
    ) -> std::result::Result<SessionCommit, SessionCommitFailure> {
        let boundary = full.session.head.history_len.get();
        Ok(SessionCommit {
            session_id: full.session.identity.id.clone(),
            save_id: SaveId::new(full.session.head.revision.get().saturating_add(1)),
            expected: full.session.head,
            identity: full.session.identity.clone(),
            metadata: full.session.metadata.clone(),
            history: HistorySuffix {
                start: HistoryIndex::new(boundary),
                final_len: full.session.head.history_len,
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(boundary),
                turn_metas: test_side_table_rows_from(&full.turn_metas, boundary),
                metadata_snapshots: test_side_table_rows_from(&full.metadata_snapshots, boundary),
                context_snapshots: test_side_table_rows_from(&full.context_snapshots, boundary),
            },
            descriptors,
        })
    }

    fn test_full_replacement_command(
        expected: StoreHead,
        identity: SessionIdentity,
        metadata: SessionMetadata,
        full: &FullSession,
    ) -> std::result::Result<SessionCommit, SessionCommitFailure> {
        let final_len =
            u64::try_from(full.history.len()).map_err(|_| SessionCommitFailure::Integrity {
                message: "history length exceeds u64".into(),
            })?;
        Ok(SessionCommit {
            session_id: identity.id.clone(),
            save_id: SaveId::new(expected.revision.get().saturating_add(1)),
            expected,
            identity,
            metadata,
            history: HistorySuffix {
                start: HistoryIndex::ZERO,
                final_len: HistoryLen::new(final_len),
                items: full.history.clone(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::ZERO,
                turn_metas: test_side_table_rows(&full.turn_metas),
                metadata_snapshots: test_side_table_rows(&full.metadata_snapshots),
                context_snapshots: test_side_table_rows(&full.context_snapshots),
            },
            descriptors: Some(TranscriptDescriptorSuffix {
                start: DescriptorIndex::ZERO,
                records: full.descriptors.clone(),
            }),
        })
    }

    fn test_side_table_rows(
        rows: &[(u64, serde_json::Value)],
    ) -> Vec<(HistoryIndex, serde_json::Value)> {
        test_side_table_rows_from(rows, 0)
    }

    fn test_side_table_rows_from(
        rows: &[(u64, serde_json::Value)],
        start: u64,
    ) -> Vec<(HistoryIndex, serde_json::Value)> {
        rows.iter()
            .filter(|(index, _)| *index >= start)
            .map(|(index, value)| (HistoryIndex::new(*index), value.clone()))
            .collect()
    }

    fn test_identity_from_model(state: &TestSessionModel) -> SessionIdentity {
        SessionIdentity {
            id: state.id.clone(),
            created_at: state.created_at,
            parent_id: state.parent_id.clone(),
        }
    }

    fn test_metadata_from_model(state: &TestSessionModel) -> Result<SessionMetadata> {
        Ok(SessionMetadata {
            title: state.title.clone(),
            slug: state.slug.clone(),
            first_user_message: state.first_user_message.clone(),
            cwd: state.cwd.clone(),
            mode: state.mode.clone(),
            reasoning_effort: state.reasoning_effort.clone(),
            model: state.model.clone(),
            fast_mode: state.fast_mode,
            accounting_json: state.accounting_json.clone(),
            checkpoint_json: state.checkpoint_json.clone(),
            context_tokens: state.context_tokens,
            context_tokens_history_len: state.context_tokens_history_len,
            display_context_tokens: state.display_context_tokens,
            session_cost_usd: crate::SessionCostUsd::new(state.session_cost_usd)?,
            updated_at: state.updated_at,
        })
    }

    fn test_model_from_stored(session: StoredSession) -> Result<TestSessionModel> {
        Ok(TestSessionModel {
            id: session.identity.id,
            title: session.metadata.title,
            slug: session.metadata.slug,
            first_user_message: session.metadata.first_user_message,
            cwd: session.metadata.cwd,
            mode: session.metadata.mode,
            reasoning_effort: session.metadata.reasoning_effort,
            model: session.metadata.model,
            fast_mode: session.metadata.fast_mode,
            parent_id: session.identity.parent_id,
            accounting_json: session.metadata.accounting_json,
            checkpoint_json: session.metadata.checkpoint_json,
            context_tokens: session.metadata.context_tokens,
            context_tokens_history_len: session.metadata.context_tokens_history_len,
            display_context_tokens: session.metadata.display_context_tokens,
            session_cost_usd: session.metadata.session_cost_usd.get(),
            revision: session.head.revision.get(),
            history_len: session.head.history_len.get(),
            created_at: session.identity.created_at,
            updated_at: session.metadata.updated_at,
        })
    }

    fn test_identity(id: &str) -> SessionIdentity {
        test_identity_from_model(&test_session_state(id, 0))
    }

    fn test_metadata() -> SessionMetadata {
        test_metadata_from_model(&test_session_state("unused", 0)).unwrap()
    }

    fn test_store_head(revision: u64, history_len: u64, descriptor_len: u64) -> StoreHead {
        StoreHead {
            revision: revision.into(),
            history_len: history_len.into(),
            descriptor_len: descriptor_len.into(),
        }
    }

    fn test_empty_commit(id: &str, expected: StoreHead) -> SessionCommit {
        SessionCommit {
            session_id: id.into(),
            save_id: crate::SaveId::new(expected.revision.get().saturating_add(1)),
            expected,
            identity: test_identity(id),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(expected.history_len.get()),
                final_len: expected.history_len,
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(expected.history_len.get()),
                ..SideTableSuffixes::default()
            },
            descriptors: None,
        }
    }

    #[test]
    fn creates_and_reopens_session_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");

        let db = SessionDb::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), schema::SCHEMA_VERSION);
        db.quick_check().unwrap();
        drop(db);

        let db = SessionDb::open_read_only(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), schema::SCHEMA_VERSION);
        db.quick_check().unwrap();
    }

    #[test]
    fn writable_open_configures_durable_bounded_wal_policy() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();

        assert_eq!(
            db.connection()
                .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.connection()
                .query_row("PRAGMA journal_size_limit", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            JOURNAL_SIZE_LIMIT_BYTES as i64
        );
        assert_eq!(
            db.connection()
                .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            WAL_AUTOCHECKPOINT_PAGES as i64
        );
    }

    #[test]
    fn immediate_transaction_uses_one_bounded_sqlite_busy_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");
        let mut db = SessionDb::open(&path).unwrap();
        let lock = Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let err = db
            .immediate_transaction("test lock acquisition", |_| Ok(()))
            .unwrap_err();
        lock.execute_batch("ROLLBACK").unwrap();

        assert!(matches!(
            err,
            StoreError::Busy {
                operation: "test lock acquisition",
                attempts: 1,
                ..
            }
        ));
        assert!(started.elapsed() >= std::time::Duration::from_millis(50));
        assert!(started.elapsed() < std::time::Duration::from_millis(300));
    }

    #[test]
    fn immediate_transaction_restores_bounded_ordinary_busy_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");
        let mut db = SessionDb::open(&path).unwrap();
        db.immediate_transaction("install begin retry handler", |_| Ok(()))
            .unwrap();
        let lock = Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let err = db
            .connection()
            .execute(
                "INSERT INTO store_meta (key, value) VALUES ('ordinary_busy', 'blocked')",
                [],
            )
            .unwrap_err();
        let waited = started.elapsed();
        lock.execute_batch("ROLLBACK").unwrap();

        assert!(sqlite_error_is_locked(&err));
        assert!(waited >= std::time::Duration::from_millis(10));
        assert!(waited < std::time::Duration::from_millis(250));
    }

    #[test]
    fn immediate_transaction_reports_commit_failure_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.connection()
            .execute_batch(
                "CREATE TABLE commit_parent(id INTEGER PRIMARY KEY);
                 CREATE TABLE commit_child(
                    parent_id INTEGER REFERENCES commit_parent(id) DEFERRABLE INITIALLY DEFERRED
                 );",
            )
            .unwrap();

        let err = db
            .immediate_transaction("test commit failure", |conn| {
                conn.execute("INSERT INTO commit_child(parent_id) VALUES (1)", [])?;
                Ok(())
            })
            .unwrap_err();

        assert!(matches!(err, StoreError::Sqlite(_)));
        assert_eq!(
            db.connection()
                .query_row("SELECT COUNT(*) FROM commit_child", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn immediate_transaction_surfaces_rollback_cleanup_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();

        let err = db
            .immediate_transaction("test rollback failure", |conn| {
                conn.execute_batch("ROLLBACK")?;
                Err::<(), _>(StoreError::Integrity("injected body failure".into()))
            })
            .unwrap_err();

        assert!(matches!(
            err,
            StoreError::TransactionCleanup {
                operation: "test rollback failure",
                message,
            } if message.contains("rollback failed")
        ));
    }

    #[test]
    fn resume_snapshot_reads_one_complete_store_head_and_descriptor_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let history = (0..4)
            .map(|idx| protocol::HistoryItem::user(protocol::Content::text(format!("item {idx}"))))
            .collect::<Vec<_>>();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("resume-snapshot", history.len()),
            history_start_idx: 0,
            history_len: history.len(),
            history,
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let descriptors = (0..4)
            .map(|idx| transcript_record(idx, "text", &format!("descriptor {idx}")))
            .collect::<Vec<_>>();
        db.apply_test_descriptors(&descriptors).unwrap();
        drop(db);
        let db = SessionDb::open_read_only(dir.path().join("session.db")).unwrap();

        let snapshot = db
            .load_session_resume_snapshot(80, 3)
            .unwrap()
            .expect("resume snapshot");

        assert_eq!(snapshot.session.identity.id, "resume-snapshot");
        assert_eq!(snapshot.session.head.revision, crate::Revision::new(2));
        assert_eq!(snapshot.session.head.history_len, HistoryLen::new(4));
        assert_eq!(
            snapshot.session.head.descriptor_len,
            crate::DescriptorLen::new(4)
        );
        assert_eq!(snapshot.retained_history_len, 4);
        assert!(snapshot.history_text_bytes > 0);
        assert!(snapshot.missing_object_references.is_empty());
        assert_eq!(snapshot.descriptor_tail.total_count, 4);
        assert_eq!(snapshot.descriptor_tail.start.get(), 2);
        assert_eq!(
            snapshot.descriptor_tail.records,
            descriptors[2..]
                .iter()
                .cloned()
                .map(without_indexed_text)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn online_backup_is_consistent_private_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("session.db");
        let backup_path = dir.path().join("backup.db");
        let mut db = SessionDb::open(&source_path).unwrap();
        db.apply_test_state(&test_session_state("backup", 0))
            .unwrap();

        db.backup_to(&backup_path).unwrap();
        assert!(db.backup_to(&backup_path).is_err());
        let backup = SessionDb::open_read_only(&backup_path).unwrap();
        assert_eq!(backup.test_session_model().unwrap().unwrap().id, "backup");
        assert!(backup.doctor_report().unwrap().healthy);
        assert!(backup.storage_stats().unwrap().database_bytes > 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&backup_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn doctor_reports_session_state_history_length_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_state(&test_session_state("doctor", 0))
            .unwrap();
        db.connection()
            .execute("UPDATE session_state SET history_len = 1", [])
            .unwrap();

        let report = db.doctor_report().unwrap();

        assert!(!report.healthy);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.contains("history_len 1 does not match 0 history row")));
    }

    #[test]
    fn doctor_detects_missing_fts_postings_and_rebuild_restores_them() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.connection()
            .execute_batch(
                "INSERT INTO transcript_blocks (
                    block_idx, descriptor_idx, kind, content_hash, estimated_text_bytes,
                    descriptor_json
                 ) VALUES (0, 0, 'user', 'content', 5, '{}');
                 INSERT INTO transcript_search (block_idx, indexed_text) VALUES (0, 'hello');
                 INSERT INTO transcript_search_fts(
                    transcript_search_fts, rowid, indexed_text
                 ) VALUES ('delete', 0, 'hello');",
            )
            .unwrap();

        let report = db.doctor_report().unwrap();
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.contains("search index is missing block 0")));

        db.connection()
            .execute(
                "INSERT INTO transcript_search_fts(transcript_search_fts) VALUES('rebuild')",
                [],
            )
            .unwrap();
        assert!(db.doctor_report().unwrap().healthy);
    }

    #[test]
    fn close_checkpoint_is_best_effort_while_a_reader_holds_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");
        let writer = SessionDb::open(&path).unwrap();
        writer
            .connection()
            .execute(
                "INSERT INTO store_meta (key, value) VALUES ('first', '1')",
                [],
            )
            .unwrap();
        let reader = SessionDb::open_read_only(&path).unwrap();
        reader.connection().execute_batch("BEGIN").unwrap();
        reader
            .connection()
            .query_row("SELECT COUNT(*) FROM store_meta", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        writer
            .connection()
            .execute(
                "INSERT INTO store_meta (key, value) VALUES ('second', '2')",
                [],
            )
            .unwrap();

        assert!(!writer.close_hygiene().unwrap());
        reader.connection().execute_batch("COMMIT").unwrap();
        assert!(writer.close_hygiene().unwrap());
        assert_eq!(file_size(&sqlite_companion_path(&path, "-wal")).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn writable_open_uses_private_directory_and_database_modes() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        let path = session_dir.join("session.db");
        let db = SessionDb::open(&path).unwrap();
        db.connection()
            .execute(
                "INSERT INTO store_meta (key, value) VALUES ('mode', 'check')",
                [],
            )
            .unwrap();
        drop(db);

        assert_eq!(
            fs::metadata(&session_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for suffix in ["-wal", "-shm"] {
            let companion = path.with_file_name(format!("session.db{suffix}"));
            if companion.exists() {
                assert_eq!(
                    fs::metadata(companion).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn writable_open_rejects_symlinked_session_paths() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let linked_dir = root.path().join("linked");
        symlink(&target, &linked_dir).unwrap();
        assert!(SessionDb::open(linked_dir.join("session.db")).is_err());

        let real_dir = root.path().join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let target_db = target.join("target.db");
        fs::write(&target_db, []).unwrap();
        symlink(&target_db, real_dir.join("session.db")).unwrap();
        assert!(SessionDb::open(real_dir.join("session.db")).is_err());

        let companion_dir = root.path().join("companion");
        fs::create_dir_all(&companion_dir).unwrap();
        let companion_target = target.join("target-wal");
        fs::write(&companion_target, []).unwrap();
        symlink(&companion_target, companion_dir.join("session.db-wal")).unwrap();
        assert!(SessionDb::open(companion_dir.join("session.db")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn read_only_open_rejects_symlinked_database() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.db");
        drop(SessionDb::open(&target).unwrap());
        let linked = root.path().join("session.db");
        symlink(&target, &linked).unwrap();

        assert!(SessionDb::open_read_only(linked).is_err());
    }

    #[test]
    fn transcript_descriptor_suffix_preserves_prefix_and_replaces_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial = vec![
            transcript_record(0, "zero", "old zero"),
            transcript_record(1, "one", "old one"),
            transcript_record(2, "two", "old two"),
        ];
        db.apply_test_descriptors(&initial).unwrap();

        let replacement = vec![
            transcript_record(1, "one-new", "updated one"),
            transcript_record(2, "two-new", "updated two"),
        ];
        db.apply_test_descriptor_suffix(1, &replacement).unwrap();

        let records = db.read_all_transcript_descriptor_records().unwrap();
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

        db.apply_test_descriptor_suffix(2, &[]).unwrap();
        let records = db.read_all_transcript_descriptor_records().unwrap();
        assert_eq!(records, vec![initial[0].clone(), replacement[0].clone()]);
        assert_eq!(
            db.search_transcript_candidates("updated two").unwrap(),
            vec![]
        );
    }

    #[test]
    fn transcript_descriptor_suffix_compacts_sparse_existing_descriptor_indices() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = transcript_record(0, "zero", "zero");
        let sparse = transcript_record(302, "sparse", "sparse");
        db.apply_test_descriptors(&[first.clone(), sparse.clone()])
            .unwrap();
        db.connection()
            .execute(
                "UPDATE transcript_blocks SET descriptor_idx = 302 WHERE block_idx = 302",
                [],
            )
            .unwrap();
        assert_eq!(db.transcript_descriptor_count().unwrap(), 2);
        assert_eq!(db.transcript_descriptor_dense_extent().unwrap(), 303);

        let appended = transcript_record(303, "appended", "appended");
        db.apply_test_descriptor_suffix(2, std::slice::from_ref(&appended))
            .unwrap();

        assert_eq!(db.transcript_descriptor_dense_extent().unwrap(), 3);
        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap(),
            vec![first, sparse, appended]
        );
    }

    #[test]
    fn transcript_descriptor_suffix_rejects_start_past_dense_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_descriptors(&[transcript_record(0, "zero", "zero")])
            .unwrap();

        let err = db
            .apply_test_descriptor_suffix(2, &[transcript_record(2, "stale", "stale")])
            .unwrap_err();
        assert!(matches!(
            err,
            SessionCommitFailure::InvalidDescriptorSuffix { .. }
        ));
        assert_eq!(db.transcript_descriptor_count().unwrap(), 1);
        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap(),
            vec![transcript_record(0, "zero", "zero")]
        );
    }

    #[test]
    fn corrupt_persisted_commit_is_classified_as_integrity_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.set_meta(LAST_SESSION_COMMIT_KEY, "not-json").unwrap();

        let err = db.last_session_commit_fingerprint().unwrap_err();

        assert!(matches!(
            session_commit_failure_from_store_error(err),
            SessionCommitFailure::Integrity { message }
                if message.contains("invalid persisted session commit")
        ));
    }

    #[test]
    fn commit_session_is_idempotent_for_the_same_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");
        let mut db = SessionDb::open(&path).unwrap();
        let history = protocol::HistoryItem::user(protocol::Content::text("hello"));
        let command = SessionCommit {
            session_id: "typed-commit".into(),
            save_id: crate::SaveId::new(1),
            expected: StoreHead::default(),
            identity: test_identity("typed-commit"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::ZERO,
                final_len: HistoryLen::new(1),
                items: vec![history],
            },
            side_tables: SideTableSuffixes::default(),
            descriptors: Some(crate::TranscriptDescriptorSuffix {
                start: DescriptorIndex::ZERO,
                records: vec![transcript_user_record_with_history(0, 0, "user", "hello")],
            }),
        };

        let receipt = db.apply_session_commit(&command).unwrap();
        drop(db);
        let mut db = SessionDb::open(&path).unwrap();
        let repeated = db.apply_session_commit(&command).unwrap();

        assert_eq!(repeated, receipt);
        assert_eq!(receipt.previous.revision, crate::Revision::ZERO);
        assert_eq!(receipt.current.revision, crate::Revision::new(1));
        assert_eq!(receipt.current.history_len, HistoryLen::new(1));
        assert_eq!(receipt.current.descriptor_len, crate::DescriptorLen::new(1));
        assert_eq!(db.history_item_count().unwrap(), 1);
        assert_eq!(db.transcript_descriptor_count().unwrap(), 1);
    }

    #[test]
    fn commit_fingerprint_ignores_save_id_but_replay_uses_incoming_correlation() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let command = test_empty_commit("fingerprint-save-id", StoreHead::default());
        let mut replay = command.clone();
        replay.save_id = crate::SaveId::new(99);

        assert_eq!(
            session_commit_fingerprint(&command).unwrap(),
            session_commit_fingerprint(&replay).unwrap()
        );
        let receipt = db.apply_session_commit(&command).unwrap();
        let replayed = db.apply_session_commit(&replay).unwrap();

        assert_eq!(replayed.save_id, replay.save_id);
        assert_eq!(replayed.previous, receipt.previous);
        assert_eq!(replayed.current, receipt.current);
        assert_eq!(replayed.current.revision, Revision::new(1));
    }

    #[test]
    fn canonical_fingerprint_sorts_json_keys_and_normalizes_negative_zero() {
        let mut left = test_empty_commit("canonical-fingerprint", StoreHead::default());
        let mut right = left.clone();
        let mut left_map = serde_json::Map::new();
        left_map.insert("z".into(), serde_json::json!({"b": 2, "a": 1}));
        left_map.insert("a".into(), serde_json::json!([3, 2, 1]));
        let mut right_map = serde_json::Map::new();
        right_map.insert("a".into(), serde_json::json!([3, 2, 1]));
        right_map.insert("z".into(), serde_json::json!({"a": 1, "b": 2}));
        left.metadata.accounting_json = Some(left_map.into());
        right.metadata.accounting_json = Some(right_map.into());
        left.metadata.session_cost_usd = crate::SessionCostUsd::new(-0.0).unwrap();
        right.metadata.session_cost_usd = crate::SessionCostUsd::new(0.0).unwrap();

        assert_eq!(
            session_commit_fingerprint(&left).unwrap(),
            session_commit_fingerprint(&right).unwrap()
        );
    }

    #[test]
    fn metadata_only_and_fast_mode_only_changes_advance_revision_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial = db
            .apply_session_commit(&test_empty_commit(
                "metadata-revisions",
                StoreHead::default(),
            ))
            .unwrap();

        let mut fast_mode = test_empty_commit("metadata-revisions", initial.current);
        fast_mode.metadata.fast_mode = Some(true);
        let fast_mode = db.apply_session_commit(&fast_mode).unwrap();
        assert_eq!(fast_mode.current.revision, Revision::new(2));

        let mut title = test_empty_commit("metadata-revisions", fast_mode.current);
        title.metadata.fast_mode = Some(true);
        title.metadata.title = Some("metadata only".into());
        let title = db.apply_session_commit(&title).unwrap();
        assert_eq!(title.current.revision, Revision::new(3));

        let mut no_op = test_empty_commit("metadata-revisions", title.current);
        no_op.metadata.fast_mode = Some(true);
        no_op.metadata.title = Some("metadata only".into());
        let no_op = db.apply_session_commit(&no_op).unwrap();
        assert_eq!(no_op.previous, no_op.current);
        assert_eq!(no_op.current.revision, Revision::new(3));
    }

    #[test]
    fn history_append_replacement_and_truncation_each_advance_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let user = |text| protocol::HistoryItem::user(protocol::Content::text(text));
        let mut initial = test_empty_commit("history-revisions", StoreHead::default());
        initial.history.start = HistoryIndex::ZERO;
        initial.history.final_len = HistoryLen::new(2);
        initial.history.items = vec![user("a"), user("b")];
        initial.side_tables.start = HistoryIndex::ZERO;
        let initial = db.apply_session_commit(&initial).unwrap();
        assert_eq!(initial.current.revision, Revision::new(1));

        let mut append = test_empty_commit("history-revisions", initial.current);
        append.history.start = HistoryIndex::new(2);
        append.history.final_len = HistoryLen::new(3);
        append.history.items = vec![user("c")];
        append.side_tables.start = HistoryIndex::new(2);
        let append = db.apply_session_commit(&append).unwrap();
        assert_eq!(append.current.revision, Revision::new(2));

        let mut replacement = test_empty_commit("history-revisions", append.current);
        replacement.history.start = HistoryIndex::new(1);
        replacement.history.final_len = HistoryLen::new(3);
        replacement.history.items = vec![user("b replacement"), user("c")];
        replacement.side_tables.start = HistoryIndex::new(1);
        let replacement = db.apply_session_commit(&replacement).unwrap();
        assert_eq!(replacement.current.revision, Revision::new(3));

        let mut truncation = test_empty_commit("history-revisions", replacement.current);
        truncation.history.start = HistoryIndex::new(2);
        truncation.history.final_len = HistoryLen::new(2);
        truncation.side_tables.start = HistoryIndex::new(2);
        let truncation = db.apply_session_commit(&truncation).unwrap();
        assert_eq!(truncation.current.revision, Revision::new(4));
        assert_eq!(db.history_item_count().unwrap(), 2);
    }

    #[test]
    fn descriptor_append_replacement_and_truncation_each_advance_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial = db
            .apply_session_commit(&test_empty_commit(
                "descriptor-revisions",
                StoreHead::default(),
            ))
            .unwrap();

        let mut append = test_empty_commit("descriptor-revisions", initial.current);
        append.descriptors = Some(crate::TranscriptDescriptorSuffix {
            start: DescriptorIndex::ZERO,
            records: vec![transcript_record(0, "first", "first")],
        });
        let append = db.apply_session_commit(&append).unwrap();
        assert_eq!(append.current.revision, Revision::new(2));
        assert_eq!(append.current.descriptor_len, crate::DescriptorLen::new(1));

        let mut replacement = test_empty_commit("descriptor-revisions", append.current);
        replacement.descriptors = Some(crate::TranscriptDescriptorSuffix {
            start: DescriptorIndex::ZERO,
            records: vec![transcript_record(0, "replacement", "replacement")],
        });
        let replacement = db.apply_session_commit(&replacement).unwrap();
        assert_eq!(replacement.current.revision, Revision::new(3));
        assert_eq!(
            replacement.current.descriptor_len,
            crate::DescriptorLen::new(1)
        );

        let mut truncation = test_empty_commit("descriptor-revisions", replacement.current);
        truncation.descriptors = Some(crate::TranscriptDescriptorSuffix {
            start: DescriptorIndex::ZERO,
            records: Vec::new(),
        });
        let truncation = db.apply_session_commit(&truncation).unwrap();
        assert_eq!(truncation.current.revision, Revision::new(4));
        assert_eq!(
            truncation.current.descriptor_len,
            crate::DescriptorLen::ZERO
        );
    }

    #[test]
    fn revision_overflow_fails_without_partial_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_session_commit(&test_empty_commit(
            "revision-overflow",
            StoreHead::default(),
        ))
        .unwrap();
        db.connection()
            .execute(
                "UPDATE session_state SET revision = ?1 WHERE singleton = 1",
                [i64::MAX],
            )
            .unwrap();
        let head = db.store_head().unwrap();
        let mut command = test_empty_commit("revision-overflow", head);
        command.metadata.title = Some("must roll back".into());

        assert!(matches!(
            db.apply_session_commit(&command).unwrap_err(),
            SessionCommitFailure::Integrity { message }
                if message.contains("revision exceeds SQLite integer range")
        ));
        let stored = db.stored_session().unwrap().unwrap();
        assert_eq!(stored.head.revision.get(), i64::MAX as u64);
        assert_eq!(stored.metadata.title, None);
    }

    #[test]
    fn immutable_creation_time_and_parent_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let receipt = db
            .apply_session_commit(&test_empty_commit(
                "immutable-identity",
                StoreHead::default(),
            ))
            .unwrap();

        let mut created_at = test_empty_commit("immutable-identity", receipt.current);
        created_at.identity.created_at += 1;
        assert!(matches!(
            db.apply_session_commit(&created_at).unwrap_err(),
            SessionCommitFailure::IdentityMismatch { .. }
        ));

        let mut parent = test_empty_commit("immutable-identity", receipt.current);
        parent.identity.parent_id = Some("other-parent".into());
        assert!(matches!(
            db.apply_session_commit(&parent).unwrap_err(),
            SessionCommitFailure::IdentityMismatch { .. }
        ));
        assert_eq!(db.store_head().unwrap(), receipt.current);
    }

    #[test]
    fn oversized_commit_coordinates_fail_before_writing() {
        let oversized = i64::MAX as u64 + 1;
        let baseline = test_empty_commit("oversized-coordinates", StoreHead::default());
        let mut commands = Vec::new();

        let mut command = baseline.clone();
        command.expected.revision = Revision::new(oversized);
        commands.push(command);
        let mut command = baseline.clone();
        command.expected.history_len = HistoryLen::new(oversized);
        commands.push(command);
        let mut command = baseline.clone();
        command.expected.descriptor_len = crate::DescriptorLen::new(oversized);
        commands.push(command);
        let mut command = baseline.clone();
        command.history.start = HistoryIndex::new(oversized);
        commands.push(command);
        let mut command = baseline.clone();
        command.history.final_len = HistoryLen::new(oversized);
        commands.push(command);
        for field in 0..3 {
            let mut command = baseline.clone();
            match field {
                0 => command.metadata.context_tokens = Some(oversized),
                1 => command.metadata.context_tokens_history_len = Some(oversized),
                _ => command.metadata.display_context_tokens = Some(oversized),
            }
            commands.push(command);
        }
        let mut command = baseline.clone();
        command.side_tables.start = HistoryIndex::new(oversized);
        commands.push(command);
        for table in 0..3 {
            let mut command = baseline.clone();
            let row = (
                HistoryIndex::new(oversized),
                serde_json::json!({"overflow": true}),
            );
            match table {
                0 => command.side_tables.turn_metas.push(row),
                1 => command.side_tables.metadata_snapshots.push(row),
                _ => command.side_tables.context_snapshots.push(row),
            }
            commands.push(command);
        }
        let mut command = baseline.clone();
        command.descriptors = Some(crate::TranscriptDescriptorSuffix {
            start: DescriptorIndex::new(oversized),
            records: Vec::new(),
        });
        commands.push(command);
        for field in 0..3 {
            let mut record = transcript_record(0, "overflow", "overflow");
            match field {
                0 => record.block_idx = oversized,
                1 => record.history_idx = Some(oversized),
                _ => record.estimated_text_bytes = oversized,
            }
            let mut command = baseline.clone();
            command.descriptors = Some(crate::TranscriptDescriptorSuffix {
                start: DescriptorIndex::ZERO,
                records: vec![record],
            });
            commands.push(command);
        }

        for command in commands {
            let dir = tempfile::tempdir().unwrap();
            let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
            assert!(db.apply_session_commit(&command).is_err());
            assert!(db.stored_session().unwrap().is_none());
            assert_eq!(db.history_item_count().unwrap(), 0);
            assert_eq!(db.transcript_descriptor_count().unwrap(), 0);
        }
    }

    #[test]
    fn identical_descriptor_replacement_is_an_exact_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let descriptor = transcript_user_record_with_history(0, 0, "user", "hello");
        let first = SessionCommit {
            session_id: "descriptor-no-op".into(),
            save_id: crate::SaveId::new(1),
            expected: StoreHead::default(),
            identity: test_identity("descriptor-no-op"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::ZERO,
                final_len: HistoryLen::new(1),
                items: vec![protocol::HistoryItem::user(protocol::Content::text(
                    "hello",
                ))],
            },
            side_tables: SideTableSuffixes::default(),
            descriptors: Some(crate::TranscriptDescriptorSuffix {
                start: DescriptorIndex::ZERO,
                records: vec![descriptor.clone()],
            }),
        };
        let first_receipt = db.apply_session_commit(&first).unwrap();
        let no_op = SessionCommit {
            save_id: crate::SaveId::new(2),
            expected: first_receipt.current,
            history: crate::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::new(1),
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                ..SideTableSuffixes::default()
            },
            descriptors: Some(crate::TranscriptDescriptorSuffix {
                start: DescriptorIndex::ZERO,
                records: vec![descriptor],
            }),
            ..first
        };

        let receipt = db.apply_session_commit(&no_op).unwrap();

        assert_eq!(receipt.previous.revision, crate::Revision::new(1));
        assert_eq!(receipt.current.revision, crate::Revision::new(1));
        assert_eq!(db.transcript_descriptor_count().unwrap(), 1);
    }

    #[test]
    fn commit_session_rejects_stale_descriptor_base_before_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("stale-descriptor", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::user(protocol::Content::text(
                "hello",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        db.apply_test_descriptors(&[transcript_user_record_with_history(0, 0, "user", "hello")])
            .unwrap();
        let current_revision = db.test_session_model().unwrap().unwrap().revision;
        let command = SessionCommit {
            session_id: "stale-descriptor".into(),
            save_id: crate::SaveId::new(2),
            expected: test_store_head(current_revision, 1, 303),
            identity: test_identity("stale-descriptor"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::new(1),
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                ..SideTableSuffixes::default()
            },
            descriptors: Some(crate::TranscriptDescriptorSuffix {
                start: DescriptorIndex::new(303),
                records: Vec::new(),
            }),
        };

        let err = db.apply_session_commit(&command).unwrap_err();

        assert_eq!(
            err,
            SessionCommitFailure::StaleBase {
                expected: StoreHead {
                    revision: current_revision.into(),
                    history_len: HistoryLen::new(1),
                    descriptor_len: crate::DescriptorLen::new(303),
                },
                current: StoreHead {
                    revision: current_revision.into(),
                    history_len: HistoryLen::new(1),
                    descriptor_len: crate::DescriptorLen::new(1),
                },
            }
        );
        assert_eq!(db.transcript_descriptor_count().unwrap(), 1);
    }

    #[test]
    fn commit_session_rejects_stale_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("stale-revision", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::user(protocol::Content::text(
                "hello",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let command = SessionCommit {
            session_id: "stale-revision".into(),
            save_id: crate::SaveId::new(3),
            expected: test_store_head(0, 1, 0),
            identity: test_identity("stale-revision"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::new(1),
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                ..SideTableSuffixes::default()
            },
            descriptors: None,
        };

        assert!(matches!(
            db.apply_session_commit(&command).unwrap_err(),
            SessionCommitFailure::StaleBase { .. }
        ));
    }

    #[test]
    fn commit_session_rejects_stale_history_base() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("stale-history", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::user(protocol::Content::text(
                "hello",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let current_revision = db.test_session_model().unwrap().unwrap().revision;
        let command = SessionCommit {
            session_id: "stale-history".into(),
            save_id: crate::SaveId::new(4),
            expected: test_store_head(current_revision, 2, 0),
            identity: test_identity("stale-history"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(2),
                final_len: HistoryLen::new(2),
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(2),
                ..SideTableSuffixes::default()
            },
            descriptors: None,
        };

        assert_eq!(
            db.apply_session_commit(&command).unwrap_err(),
            SessionCommitFailure::StaleBase {
                expected: StoreHead {
                    revision: current_revision.into(),
                    history_len: HistoryLen::new(2),
                    descriptor_len: crate::DescriptorLen::ZERO,
                },
                current: StoreHead {
                    revision: current_revision.into(),
                    history_len: HistoryLen::new(1),
                    descriptor_len: crate::DescriptorLen::ZERO,
                },
            }
        );
        assert_eq!(db.history_item_count().unwrap(), 1);
    }

    #[test]
    fn commit_session_rejects_history_start_past_final_len() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let command = SessionCommit {
            session_id: "bad-history-suffix".into(),
            save_id: crate::SaveId::new(5),
            expected: StoreHead::default(),
            identity: test_identity("bad-history-suffix"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::ZERO,
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes::default(),
            descriptors: None,
        };

        assert_eq!(
            db.apply_session_commit(&command).unwrap_err(),
            SessionCommitFailure::InvalidHistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::ZERO,
                item_count: 0,
            }
        );
        assert!(db.test_session_model().unwrap().is_none());
    }

    #[test]
    fn commit_session_truncates_history_with_empty_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("truncate-history", 3),
            history_start_idx: 0,
            history_len: 3,
            history: (0..3)
                .map(|idx| {
                    protocol::HistoryItem::user(protocol::Content::text(format!("item {idx}")))
                })
                .collect(),
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let current_revision = db.test_session_model().unwrap().unwrap().revision;
        let command = SessionCommit {
            session_id: "truncate-history".into(),
            save_id: crate::SaveId::new(5),
            expected: test_store_head(current_revision, 3, 0),
            identity: test_identity("truncate-history"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(2),
                final_len: HistoryLen::new(2),
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(2),
                ..SideTableSuffixes::default()
            },
            descriptors: None,
        };

        let receipt = db.apply_session_commit(&command).unwrap();

        assert_eq!(receipt.current.history_len, HistoryLen::new(2));
        assert_eq!(db.history_item_count().unwrap(), 2);
        assert_eq!(db.test_session_model().unwrap().unwrap().history_len, 2);
    }

    #[test]
    fn commit_session_applies_side_table_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("side-table-commit", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::user(protocol::Content::text(
                "before",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let current_revision = db.test_session_model().unwrap().unwrap().revision;
        let turn_meta = serde_json::json!({"turn": 1});
        let metadata = serde_json::json!({"model": "test"});
        let command = SessionCommit {
            session_id: "side-table-commit".into(),
            save_id: crate::SaveId::new(5),
            expected: test_store_head(current_revision, 1, 0),
            identity: test_identity("side-table-commit"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::new(2),
                items: vec![protocol::HistoryItem::user(protocol::Content::text(
                    "after",
                ))],
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                turn_metas: vec![(HistoryIndex::new(1), turn_meta.clone())],
                metadata_snapshots: vec![(HistoryIndex::new(1), metadata.clone())],
                context_snapshots: Vec::new(),
            },
            descriptors: None,
        };

        let receipt = db.apply_session_commit(&command).unwrap();
        let snapshot = db.load_full_session().unwrap().unwrap();

        assert_eq!(receipt.current.history_len, HistoryLen::new(2));
        assert_eq!(snapshot.turn_metas, vec![(1, turn_meta)]);
        assert_eq!(snapshot.metadata_snapshots, vec![(1, metadata)]);
    }

    #[test]
    fn commit_session_rejects_side_table_rows_past_history_len() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let command = SessionCommit {
            session_id: "bad-side-table".into(),
            save_id: crate::SaveId::new(6),
            expected: StoreHead::default(),
            identity: test_identity("bad-side-table"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::ZERO,
                final_len: HistoryLen::new(1),
                items: vec![protocol::HistoryItem::user(protocol::Content::text(
                    "hello",
                ))],
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::ZERO,
                turn_metas: vec![(HistoryIndex::new(2), serde_json::json!({"turn": 2}))],
                ..SideTableSuffixes::default()
            },
            descriptors: None,
        };

        let err = db.apply_session_commit(&command).unwrap_err();

        assert_eq!(
            err,
            SessionCommitFailure::InvalidSideTableRow {
                table: "turn_metas".into(),
                index: HistoryIndex::new(2),
                final_len: HistoryLen::new(1),
                bound: HistoryIndexBound::AtOrBeforeFinalLen,
            }
        );
        assert!(db.test_session_model().unwrap().is_none());
        assert_eq!(db.history_item_count().unwrap(), 0);
    }

    #[test]
    fn commit_session_rejects_side_table_start_past_history_len() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let command = SessionCommit {
            session_id: "bad-side-table-suffix".into(),
            save_id: crate::SaveId::new(7),
            expected: StoreHead::default(),
            identity: test_identity("bad-side-table-suffix"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::ZERO,
                final_len: HistoryLen::ZERO,
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                ..SideTableSuffixes::default()
            },
            descriptors: None,
        };

        assert_eq!(
            db.apply_session_commit(&command).unwrap_err(),
            SessionCommitFailure::InvalidSideTableSuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::ZERO,
            }
        );
        assert!(db.test_session_model().unwrap().is_none());
    }

    #[test]
    fn commit_session_accepts_side_tables_at_history_len_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("bad-turn-meta", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::user(protocol::Content::text(
                "before",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let current_revision = db.test_session_model().unwrap().unwrap().revision;
        let command = SessionCommit {
            session_id: "bad-turn-meta".into(),
            save_id: crate::SaveId::new(6),
            expected: test_store_head(current_revision, 1, 0),
            identity: test_identity("bad-turn-meta"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::new(1),
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                turn_metas: vec![(HistoryIndex::new(1), serde_json::json!({"turn": 1}))],
                metadata_snapshots: vec![(HistoryIndex::new(1), serde_json::json!({"ok": true}))],
                context_snapshots: vec![(HistoryIndex::new(1), serde_json::json!({"tokens": 7}))],
            },
            descriptors: None,
        };

        let receipt = db.apply_session_commit(&command).unwrap();
        drop(db);
        let db = SessionDb::open_read_only(dir.path().join("session.db")).unwrap();
        let snapshot = db.load_full_session().unwrap().unwrap();

        assert_eq!(receipt.current.history_len, HistoryLen::new(1));
        assert_eq!(
            snapshot.turn_metas,
            vec![(1, serde_json::json!({"turn": 1}))]
        );
        assert_eq!(
            snapshot.metadata_snapshots,
            vec![(1, serde_json::json!({"ok": true}))]
        );
        assert_eq!(
            snapshot.context_snapshots,
            vec![(1, serde_json::json!({"tokens": 7}))]
        );
        assert_eq!(
            db.test_session_model().unwrap().unwrap().revision,
            current_revision + 1
        );
        assert!(db.doctor_report().unwrap().healthy);
    }

    #[test]
    fn commit_session_rejects_missing_history_object_refs() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("missing-object-ref", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::user(protocol::Content::text(
                "before",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        db.connection()
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO history_object_refs (history_idx, object_hash, role)
                 VALUES (0, 'missing-object', 'metadata');
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
        let current_revision = db.test_session_model().unwrap().unwrap().revision;
        let command = SessionCommit {
            session_id: "missing-object-ref".into(),
            save_id: crate::SaveId::new(7),
            expected: test_store_head(current_revision, 1, 0),
            identity: test_identity("missing-object-ref"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::new(1),
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                ..SideTableSuffixes::default()
            },
            descriptors: None,
        };

        let err = db.apply_session_commit(&command).unwrap_err();

        assert!(matches!(
            err,
            SessionCommitFailure::Integrity { message }
                if message.contains("history object refs point to missing objects")
        ));
        assert_eq!(
            db.test_session_model().unwrap().unwrap().revision,
            current_revision
        );
    }

    #[test]
    fn commit_session_rolls_back_history_when_descriptor_validation_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("rollback-descriptor", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::user(protocol::Content::text(
                "before",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let current_revision = db.test_session_model().unwrap().unwrap().revision;
        let command = SessionCommit {
            session_id: "rollback-descriptor".into(),
            save_id: crate::SaveId::new(6),
            expected: test_store_head(current_revision, 1, 0),
            identity: test_identity("rollback-descriptor"),
            metadata: test_metadata(),
            history: crate::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: HistoryLen::new(2),
                items: vec![protocol::HistoryItem::user(protocol::Content::text(
                    "after",
                ))],
            },
            side_tables: SideTableSuffixes::default(),
            descriptors: Some(crate::TranscriptDescriptorSuffix {
                start: DescriptorIndex::ZERO,
                records: vec![transcript_record_with_history(0, 1, "assistant", "after")],
            }),
        };

        let err = db.apply_session_commit(&command).unwrap_err();

        assert!(matches!(
            err,
            SessionCommitFailure::Integrity { message }
                if message.contains("history link kind mismatch")
        ));
        assert_eq!(db.history_item_count().unwrap(), 1);
        assert_eq!(db.test_session_model().unwrap().unwrap().history_len, 1);
        assert_eq!(
            db.test_session_model().unwrap().unwrap().revision,
            current_revision
        );
        assert_eq!(db.transcript_descriptor_count().unwrap(), 0);
    }

    #[test]
    fn commit_session_appends_after_sparse_descriptors_and_nondescriptor_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let history = (0..12)
            .map(|idx| protocol::HistoryItem::user(protocol::Content::text(format!("item {idx}"))))
            .collect::<Vec<_>>();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("sparse-follow-up", history.len()),
            history_start_idx: 0,
            history_len: history.len(),
            history,
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        db.connection()
            .execute("DELETE FROM transcript_search", [])
            .unwrap();
        db.connection()
            .execute("DELETE FROM transcript_blocks", [])
            .unwrap();

        let first = transcript_user_record_with_history(0, 1, "first", "first");
        let sparse = transcript_user_record_with_history(302, 11, "sparse", "sparse");
        db.apply_test_descriptors(&[first.clone(), sparse.clone()])
            .unwrap();
        db.connection()
            .execute(
                "UPDATE transcript_blocks SET descriptor_idx = 302 WHERE block_idx = 302",
                [],
            )
            .unwrap();
        for (block_idx, history_idx, kind) in [(1_i64, 10_i64, "assistant"), (2, 11, "user")] {
            db.connection()
                .execute(
                    "INSERT INTO transcript_blocks (block_idx, history_idx, kind)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![block_idx, history_idx, kind],
                )
                .unwrap();
        }

        let appended_history = protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("new assistant")),
            None,
            Vec::new(),
        ));
        let appended_descriptor =
            transcript_record_with_history(303, 12, "appended", "new assistant");
        commit_current_suffix(
            &mut db,
            test_session_state("sparse-follow-up", 13),
            12,
            vec![appended_history],
            None,
            Some((2, vec![appended_descriptor.clone()])),
        )
        .unwrap();

        assert_eq!(db.transcript_descriptor_dense_extent().unwrap(), 3);
        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap(),
            vec![first, sparse, appended_descriptor]
        );
        let old_nondescriptor_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM transcript_blocks
                 WHERE block_idx = 2 AND history_idx = 11 AND descriptor_json IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_nondescriptor_count, 1);
    }

    #[test]
    fn commit_session_keeps_transcript_descriptors_independent_from_history_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let history = (0..3)
            .map(|idx| protocol::HistoryItem::user(protocol::Content::text(format!("item {idx}"))))
            .collect::<Vec<_>>();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("independent-transcript", history.len()),
            history_start_idx: 0,
            history_len: history.len(),
            history,
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        db.connection()
            .execute("DELETE FROM transcript_search", [])
            .unwrap();
        db.connection()
            .execute("DELETE FROM transcript_blocks", [])
            .unwrap();

        let initial_descriptors = vec![
            transcript_user_record_with_history(0, 0, "first", "first"),
            transcript_record(1, "assistant-a", "assistant a"),
            transcript_record(2, "assistant-b", "assistant b"),
        ];
        db.apply_test_descriptors(&initial_descriptors).unwrap();

        let appended_history = protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("new assistant")),
            None,
            Vec::new(),
        ));
        let appended_descriptor = transcript_record_with_history(3, 3, "appended", "new assistant");
        commit_current_suffix(
            &mut db,
            test_session_state("independent-transcript", 4),
            3,
            vec![appended_history],
            None,
            Some((3, vec![appended_descriptor.clone()])),
        )
        .unwrap();

        assert_eq!(db.transcript_descriptor_count().unwrap(), 4);
        assert_eq!(db.transcript_descriptor_dense_extent().unwrap(), 4);
        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap(),
            vec![
                initial_descriptors[0].clone(),
                initial_descriptors[1].clone(),
                initial_descriptors[2].clone(),
                appended_descriptor,
            ]
        );
    }

    #[test]
    fn commit_session_rejects_descriptor_history_kind_mismatch_in_saved_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let err = commit_current_suffix(
            &mut db,
            test_session_state("mismatched-descriptor-origin", 1),
            0,
            vec![protocol::HistoryItem::note(protocol::HistoryNote::context(
                "cwd changed",
            ))],
            None,
            Some((
                0,
                vec![transcript_record_with_history(0, 0, "user", "follow up")],
            )),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            SessionCommitFailure::Integrity { message } if message.contains("kind mismatch")
        ));
        assert!(db.load_full_session().unwrap().is_none());
    }

    #[test]
    fn repair_mismatched_transcript_descriptor_history_links_detaches_bad_links() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("repair-mismatched-links", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::note(protocol::HistoryNote::context(
                "cwd changed",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let mut bad = transcript_user_record_with_history(0, 0, "bad-user-link", "continue");
        bad.history_idx = None;
        db.apply_test_descriptors(std::slice::from_ref(&bad))
            .unwrap();
        db.connection()
            .execute(
                "UPDATE transcript_blocks SET history_idx = 0 WHERE block_idx = 0",
                [],
            )
            .unwrap();
        db.connection()
            .execute(
                "UPDATE transcript_search SET history_idx = 0 WHERE block_idx = 0",
                [],
            )
            .unwrap();

        assert_eq!(db.repair_test_transcript_history_links().unwrap(), 1);
        let rows = db.read_all_transcript_descriptor_records().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].preview_text, "continue");
        assert_eq!(rows[0].history_idx, None);
        assert_eq!(rows[0].origin_json, None);
        let search_history_idx: Option<i64> = db
            .connection()
            .query_row(
                "SELECT history_idx FROM transcript_search WHERE block_idx = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(search_history_idx, None);
    }

    #[test]
    fn repair_checkpoint_first_live_index_past_history_replays_retained_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let history = vec![
            protocol::HistoryItem::user(protocol::Content::text("old prompt")),
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text("recent reply")),
                None,
                Vec::new(),
            )),
        ];
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("repair-checkpoint-live-index", history.len()),
            history_start_idx: 0,
            history_len: history.len(),
            history,
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let checkpoint = serde_json::json!({
            "kind": "compaction",
            "summary": "retained summary",
            "first_live_index": 177,
            "created_at_ms": 1,
        });
        db.connection()
            .execute(
                "UPDATE session_state SET checkpoint_json = ?1 WHERE singleton = 1",
                [checkpoint.to_string()],
            )
            .unwrap();

        assert_eq!(db.repair_test_checkpoint().unwrap(), 1);
        let repaired = db
            .test_session_model()
            .unwrap()
            .unwrap()
            .checkpoint_json
            .unwrap();
        assert_eq!(repaired["summary"].as_str(), Some("retained summary"));
        assert_eq!(repaired["first_live_index"].as_u64(), Some(0));
        assert_eq!(db.repair_test_checkpoint().unwrap(), 0);
    }

    #[test]
    fn repair_checkpoint_first_live_index_past_actual_history_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("repair-checkpoint-actual-rows", 2),
            history_start_idx: 0,
            history_len: 2,
            history: vec![
                protocol::HistoryItem::user(protocol::Content::text("old prompt")),
                protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                    Some(protocol::Content::text("recent reply")),
                    None,
                    Vec::new(),
                )),
            ],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let checkpoint = serde_json::json!({
            "kind": "compaction",
            "summary": "retained summary",
            "first_live_index": 25,
            "created_at_ms": 1,
        });
        db.connection()
            .execute(
                "UPDATE session_state SET history_len = 177, checkpoint_json = ?1 WHERE singleton = 1",
                [checkpoint.to_string()],
            )
            .unwrap();

        assert_eq!(db.repair_test_checkpoint().unwrap(), 1);
        let repaired = db
            .test_session_model()
            .unwrap()
            .unwrap()
            .checkpoint_json
            .unwrap();
        assert_eq!(repaired["first_live_index"].as_u64(), Some(0));
    }

    #[test]
    fn session_state_rejects_checkpoint_first_live_index_past_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let mut state = test_session_state("reject-bad-checkpoint", 1);
        state.checkpoint_json = Some(serde_json::json!({
            "kind": "compaction",
            "summary": "bad summary",
            "first_live_index": 2,
            "created_at_ms": 1,
        }));

        let err = db.apply_test_state(&state).unwrap_err();
        assert!(matches!(
            err,
            SessionCommitFailure::Integrity { message }
                if message.contains("checkpoint first_live_index 2 exceeds history_len 0")
        ));
        assert!(db.test_session_model().unwrap().is_none());
    }

    #[test]
    fn commit_session_allows_descriptor_links_to_persisted_history_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("prefix-descriptor-origin", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::user(protocol::Content::text(
                "old user",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let updated_descriptor = transcript_user_record_with_history(0, 0, "updated", "old user");

        commit_current_suffix(
            &mut db,
            test_session_state("prefix-descriptor-origin", 1),
            1,
            Vec::new(),
            None,
            Some((0, vec![updated_descriptor.clone()])),
        )
        .unwrap();

        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap(),
            vec![updated_descriptor]
        );
    }

    #[test]
    fn commit_session_rejects_descriptor_history_kind_mismatch_in_persisted_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("prefix-descriptor-mismatch", 1),
            history_start_idx: 0,
            history_len: 1,
            history: vec![protocol::HistoryItem::note(protocol::HistoryNote::context(
                "cwd changed",
            ))],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        })
        .unwrap();

        let err = commit_current_suffix(
            &mut db,
            test_session_state("prefix-descriptor-mismatch", 1),
            1,
            Vec::new(),
            None,
            Some((
                0,
                vec![transcript_user_record_with_history(
                    0,
                    0,
                    "bad-prefix-link",
                    "follow up",
                )],
            )),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            SessionCommitFailure::Integrity { message } if message.contains("kind mismatch")
        ));
        assert!(db
            .read_all_transcript_descriptor_records()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn transcript_descriptor_estimated_rows_sums_dense_descriptor_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let mut records = vec![
            transcript_record(10, "zero", ""),
            transcript_record(20, "one", "a"),
            transcript_record(30, "two", "abcdefghi"),
            transcript_record(40, "three", "abcdefghij"),
        ];
        records[0].estimated_text_bytes = 0;
        records[1].estimated_text_bytes = 1;
        records[2].estimated_text_bytes = 9;
        records[3].estimated_text_bytes = 10;
        db.apply_test_descriptors(&records).unwrap();

        assert_eq!(
            db.transcript_descriptor_estimated_rows((0..0).into(), 5)
                .unwrap(),
            0
        );
        let inverted_start = 3;
        let inverted_end = 1;
        assert_eq!(
            db.transcript_descriptor_estimated_rows((inverted_start..inverted_end).into(), 5)
                .unwrap(),
            0
        );
        assert_eq!(
            db.transcript_descriptor_estimated_rows((0..4).into(), 5)
                .unwrap(),
            10
        );
        assert_eq!(
            db.transcript_descriptor_estimated_rows((1..3).into(), 5)
                .unwrap(),
            5
        );
    }

    #[test]
    fn transcript_descriptor_estimated_rows_uses_compact_tool_preview() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let mut tool = transcript_record(0, "tool", &"x".repeat(10_000));
        tool.kind = "tool".into();
        tool.tool_name = Some("edit_file".into());
        tool.preview_text = "edited file".into();
        let assistant = transcript_record(1, "assistant", "abcdefghij");
        db.apply_test_descriptors(&[tool, assistant]).unwrap();

        assert_eq!(
            db.transcript_descriptor_estimated_rows((0..1).into(), 10)
                .unwrap(),
            3
        );
        assert_eq!(
            db.transcript_descriptor_estimated_rows((0..2).into(), 10)
                .unwrap(),
            5
        );
    }

    #[test]
    fn descriptor_slices_omit_indexed_text_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let huge_indexed_text = format!("needle {}", "x".repeat(10_000));
        let record = transcript_record(0, "huge", &huge_indexed_text);
        db.apply_test_descriptors(std::slice::from_ref(&record))
            .unwrap();

        let slice = db.read_transcript_descriptor_slice((0..1).into()).unwrap();
        assert_eq!(slice.records.len(), 1);
        assert_eq!(slice.records[0].indexed_text, "");

        let tail = db.read_transcript_descriptor_tail_slice(1).unwrap();
        assert_eq!(tail.records.len(), 1);
        assert_eq!(tail.records[0].indexed_text, "");

        let full = db.read_all_transcript_descriptor_records().unwrap();
        assert_eq!(full[0].indexed_text, huge_indexed_text);
        assert!(!db
            .search_transcript_candidates("needle")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn transcript_search_filters_exact_substring_in_sql() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_descriptors(&[
            transcript_record(0, "false-positive", "aba gap bab"),
            transcript_record(1, "exact", "xx abab yy"),
        ])
        .unwrap();

        assert_eq!(
            db.search_transcript_candidates("abab").unwrap(),
            vec![TranscriptSearchCandidate {
                block_idx: 1,
                history_idx: None,
            }]
        );
    }

    #[test]
    fn transcript_search_treats_fts_query_punctuation_literally() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_descriptors(&[
            transcript_record(0, "underscore", "xx foo_bar yy"),
            transcript_record(1, "wildcard", "xx fooXbar yy"),
            transcript_record(2, "percent", "xx foo%bar yy"),
            transcript_record(3, "quote", "say \"hi\" now"),
        ])
        .unwrap();

        assert_eq!(
            db.search_transcript_candidates("foo_bar").unwrap(),
            vec![TranscriptSearchCandidate {
                block_idx: 0,
                history_idx: None,
            }]
        );
        assert_eq!(
            db.search_transcript_candidates("foo%bar").unwrap(),
            vec![TranscriptSearchCandidate {
                block_idx: 2,
                history_idx: None,
            }]
        );
        assert_eq!(
            db.search_transcript_candidates("\"hi\"").unwrap(),
            vec![TranscriptSearchCandidate {
                block_idx: 3,
                history_idx: None,
            }]
        );
    }

    #[test]
    fn commit_session_appends_history_and_descriptors_transactionally() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial_history = vec![
            protocol::HistoryItem::user(protocol::Content::text("old user")),
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text("old assistant")),
                None,
                Vec::new(),
            )),
        ];
        let initial_snapshot = TestSessionFixture {
            state: test_session_state("typed-suffix", initial_history.len()),
            history_start_idx: 0,
            history_len: initial_history.len(),
            history: initial_history.clone(),
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };
        db.apply_test_fixture(&initial_snapshot).unwrap();
        let initial_descriptors = vec![
            transcript_user_record_with_history(0, 0, "old-user", "old user"),
            transcript_record_with_history(1, 1, "old-assistant", "old assistant"),
        ];
        db.apply_test_descriptors(&initial_descriptors).unwrap();

        let appended = protocol::HistoryItem::user(protocol::Content::text("new user"));
        let appended_descriptor = transcript_user_record_with_history(2, 2, "new-user", "new user");
        let receipt = commit_current_suffix(
            &mut db,
            test_session_state("typed-suffix", 3),
            2,
            vec![appended.clone()],
            None,
            Some((2, vec![appended_descriptor.clone()])),
        )
        .unwrap();

        assert_eq!(receipt.current.history_len, HistoryLen::new(3));
        assert_eq!(receipt.current.descriptor_len, crate::DescriptorLen::new(3));
        assert_eq!(
            db.read_history_items_range(0..3).unwrap(),
            vec![
                initial_history[0].clone(),
                initial_history[1].clone(),
                appended
            ]
        );
        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap(),
            vec![
                initial_descriptors[0].clone(),
                initial_descriptors[1].clone(),
                appended_descriptor,
            ]
        );
        assert_eq!(db.test_session_model().unwrap().unwrap().history_len, 3);
    }

    #[test]
    fn copy_prefix_to_forks_store_without_copied_tail() {
        let source_dir = tempfile::tempdir().unwrap();
        let dest_dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(source_dir.path().join("session.db")).unwrap();
        let history = vec![
            protocol::HistoryItem::user(protocol::Content::text("one")),
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text("two")),
                None,
                Vec::new(),
            )),
            protocol::HistoryItem::user(protocol::Content::text("three")),
        ];
        db.apply_test_fixture(&TestSessionFixture {
            state: test_session_state("source", history.len()),
            history_start_idx: 0,
            history_len: history.len(),
            history: history.clone(),
            turn_metas: vec![
                (0, serde_json::json!({"turn":"first"})),
                (2, serde_json::json!({"turn":"fork-boundary"})),
            ],
            metadata_snapshots: vec![(2, serde_json::json!({"slug":"prefix"}))],
            context_snapshots: Vec::new(),
        })
        .unwrap();
        let descriptors = vec![
            transcript_user_record_with_history(0, 0, "one", "one"),
            transcript_record_with_history(1, 1, "two", "two"),
            transcript_user_record_with_history(2, 2, "three", "three"),
        ];
        db.apply_test_descriptors(&descriptors).unwrap();

        let mut fork_state = test_session_state("fork", 2);
        fork_state.parent_id = Some("source".into());
        db.apply_test_prefix_to(dest_dir.path().join("session.db"), &fork_state, 2)
            .unwrap();

        let fork = SessionDb::open_read_only(dest_dir.path().join("session.db")).unwrap();
        assert_eq!(fork.test_session_model().unwrap().unwrap().id, "fork");
        assert_eq!(fork.history_item_count().unwrap(), 2);
        assert_eq!(
            fork.read_history_items_range(0..3).unwrap(),
            history[..2].to_vec()
        );
        assert_eq!(fork.transcript_descriptor_count().unwrap(), 2);
        assert_eq!(
            fork.load_full_session().unwrap().unwrap().turn_metas,
            vec![
                (0, serde_json::json!({"turn":"first"})),
                (2, serde_json::json!({"turn":"fork-boundary"})),
            ]
        );
        assert_eq!(
            fork.search_transcript_candidates("three").unwrap(),
            Vec::<TranscriptSearchCandidate>::new()
        );
        assert_eq!(
            fork.read_all_transcript_descriptor_records().unwrap(),
            descriptors[..2].to_vec()
        );
    }

    #[test]
    fn commit_session_without_descriptors_preserves_transcript_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial_history = vec![
            protocol::HistoryItem::user(protocol::Content::text("old user")),
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text("old assistant")),
                None,
                Vec::new(),
            )),
        ];
        let initial_snapshot = TestSessionFixture {
            state: test_session_state("delta-history-only", initial_history.len()),
            history_start_idx: 0,
            history_len: initial_history.len(),
            history: initial_history.clone(),
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };
        db.apply_test_fixture(&initial_snapshot).unwrap();
        let initial_descriptors = vec![
            transcript_user_record_with_history(0, 0, "old-user", "old user"),
            transcript_record_with_history(1, 1, "old-assistant", "old assistant"),
        ];
        db.apply_test_descriptors(&initial_descriptors).unwrap();

        let appended = protocol::HistoryItem::user(protocol::Content::text("new user"));
        commit_current_suffix(
            &mut db,
            test_session_state("delta-history-only", 3),
            2,
            vec![appended],
            None,
            None,
        )
        .unwrap();

        assert_eq!(db.history_item_count().unwrap(), 3);
        assert_eq!(db.transcript_descriptor_count().unwrap(), 2);
        assert_eq!(db.transcript_block_count().unwrap(), 3);
        assert_eq!(db.transcript_missing_descriptor_count().unwrap(), 1);
        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap(),
            initial_descriptors
        );
    }

    #[test]
    fn commit_session_descriptor_suffix_replaces_only_requested_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial_history = vec![protocol::HistoryItem::user(protocol::Content::text("user"))];
        let initial_snapshot = TestSessionFixture {
            state: test_session_state("delta-descriptors", initial_history.len()),
            history_start_idx: 0,
            history_len: initial_history.len(),
            history: initial_history,
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };
        db.apply_test_fixture(&initial_snapshot).unwrap();
        let initial_descriptors = vec![
            transcript_record(0, "zero", "old zero"),
            transcript_record(1, "one", "old one"),
            transcript_record(2, "two", "old two"),
        ];
        db.apply_test_descriptors(&initial_descriptors).unwrap();

        let replacement = transcript_record(1, "one-new", "updated one");
        commit_current_suffix(
            &mut db,
            test_session_state("delta-descriptors", 1),
            1,
            Vec::new(),
            None,
            Some((1, vec![replacement.clone()])),
        )
        .unwrap();

        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap(),
            vec![initial_descriptors[0].clone(), replacement]
        );
        assert_eq!(
            db.search_transcript_candidates("old two").unwrap(),
            Vec::<TranscriptSearchCandidate>::new()
        );
    }

    #[test]
    fn commit_session_syncs_requested_side_table_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let initial_history = vec![
            protocol::HistoryItem::user(protocol::Content::text("old user")),
            protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text("old assistant")),
                None,
                Vec::new(),
            )),
        ];
        let initial_metadata = serde_json::json!({"first_user_message":"old user"});
        let initial_snapshot = TestSessionFixture {
            state: test_session_state("typed-side-tables", initial_history.len()),
            history_start_idx: 0,
            history_len: initial_history.len(),
            history: initial_history,
            turn_metas: Vec::new(),
            metadata_snapshots: vec![(1, initial_metadata.clone())],
            context_snapshots: Vec::new(),
        };
        db.apply_test_fixture(&initial_snapshot).unwrap();

        let appended = protocol::HistoryItem::user(protocol::Content::text("new user"));
        let appended_metadata = serde_json::json!({"first_user_message":"new user"});
        commit_current_suffix(
            &mut db,
            test_session_state("typed-side-tables", 3),
            2,
            vec![appended],
            Some(SideTableSuffixes {
                start: HistoryIndex::new(2),
                turn_metas: Vec::new(),
                metadata_snapshots: vec![(HistoryIndex::new(3), appended_metadata.clone())],
                context_snapshots: Vec::new(),
            }),
            None,
        )
        .unwrap();

        let snapshot = db.load_full_session().unwrap().expect("session snapshot");
        assert_eq!(
            snapshot.metadata_snapshots,
            vec![(1, initial_metadata), (3, appended_metadata)]
        );
    }

    #[test]
    fn transcript_search_uses_indexed_short_utf8_and_paged_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = vec![
            transcript_record(0, "zero", "alpha café"),
            transcript_record(1, "one", "beta needle"),
            transcript_record(2, "two", "gamma café needle"),
            transcript_record(3, "three", "delta"),
        ];
        db.apply_test_descriptors(&records).unwrap();

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
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let mut records = (0..80)
            .map(|idx| transcript_record(idx, &format!("false-{idx}"), "abc false bcd"))
            .collect::<Vec<_>>();
        records.push(transcript_record(80, "true", "contains abcd exactly"));
        db.apply_test_descriptors(&records).unwrap();

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
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = (0..6)
            .map(|idx| transcript_record(idx * 10, &format!("block-{idx}"), &format!("text {idx}")))
            .collect::<Vec<_>>();
        db.apply_test_descriptors(&records).unwrap();

        let slice = db.read_transcript_descriptor_slice((2..5).into()).unwrap();
        assert_eq!(
            db.transcript_descriptor_index_for_block_idx(20).unwrap(),
            Some(TranscriptDescriptorIndex::new(2))
        );
        assert_eq!(
            db.transcript_descriptor_index_for_block_idx(25).unwrap(),
            None
        );
        assert_eq!(slice.start.get(), 2);
        assert_eq!(slice.end().get(), 5);
        assert_eq!(slice.total_count, 6);
        let expected_slice = records[2..5]
            .iter()
            .cloned()
            .map(without_indexed_text)
            .collect::<Vec<_>>();
        assert_eq!(slice.records, expected_slice);
        assert_eq!(
            slice.hydration,
            crate::TranscriptDescriptorHydration::ObjectBacked
        );

        let empty = db.read_transcript_descriptor_slice((4..4).into()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.start.get(), 4);
        assert_eq!(empty.total_count, 6);

        let centered = db
            .read_transcript_descriptor_centered_slice(3, 2, 1)
            .unwrap();
        assert_eq!(centered.start.get(), 1);
        assert_eq!(centered.end().get(), 5);
        let expected_centered = records[1..5]
            .iter()
            .cloned()
            .map(without_indexed_text)
            .collect::<Vec<_>>();
        assert_eq!(centered.records, expected_centered);
        assert_eq!(
            db.read_transcript_descriptor_centered_slice(0, 5, 1)
                .unwrap()
                .records,
            records[0..2]
                .iter()
                .cloned()
                .map(without_indexed_text)
                .collect::<Vec<_>>()
        );

        let tail = db.read_transcript_descriptor_tail_slice(2).unwrap();
        assert_eq!(tail.start.get(), 4);
        assert_eq!(tail.end().get(), 6);
        let expected_tail = records[4..6]
            .iter()
            .cloned()
            .map(without_indexed_text)
            .collect::<Vec<_>>();
        assert_eq!(tail.records, expected_tail);
        assert!(db
            .read_transcript_descriptor_tail_slice(0)
            .unwrap()
            .is_empty());
        let expected_all = records
            .iter()
            .cloned()
            .map(without_indexed_text)
            .collect::<Vec<_>>();
        assert_eq!(
            db.read_transcript_descriptor_tail_slice(99)
                .unwrap()
                .records,
            expected_all
        );
    }

    #[test]
    fn transcript_block_metadata_reads_include_descriptorless_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let records = vec![
            transcript_record(0, "zero", "zero text"),
            transcript_record(2, "two", "two text"),
        ];
        db.apply_test_descriptors(&records).unwrap();
        db.connection()
            .execute(
                "INSERT INTO transcript_blocks
                 (block_idx, history_idx, kind, estimated_text_bytes, estimated_rows, preview_text)
                 VALUES (1, NULL, 'note', 4, 2, 'gap one'),
                        (3, NULL, 'tool', 8, NULL, 'gap three')",
                [],
            )
            .unwrap();

        assert_eq!(db.transcript_block_count().unwrap(), 4);
        assert_eq!(db.transcript_descriptor_count().unwrap(), 2);
        assert_eq!(db.transcript_missing_descriptor_count().unwrap(), 2);

        let range = db.read_transcript_block_metadata_range(1..4).unwrap();
        assert_eq!(range.len(), 3);
        assert_eq!(range[0].block_idx, 1);
        assert_eq!(range[0].descriptor_idx, None);
        assert_eq!(range[0].kind, "note");
        assert_eq!(range[0].estimated_rows, Some(2));
        assert_eq!(range[0].preview_text, "gap one");
        assert!(!range[0].has_descriptor);
        assert_eq!(range[1].block_idx, 2);
        assert_eq!(range[1].descriptor_idx, Some(1));
        assert!(range[1].has_descriptor);
        assert_eq!(range[2].block_idx, 3);
        assert_eq!(range[2].descriptor_idx, None);
        assert_eq!(range[2].estimated_rows, None);
        assert!(!range[2].has_descriptor);

        let tail = db.read_transcript_block_metadata_tail(2).unwrap();
        assert_eq!(
            tail.iter().map(|row| row.block_idx).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(db.read_transcript_block_metadata_tail(0).unwrap(), vec![]);
        assert_eq!(
            db.read_transcript_block_metadata_range(2..2).unwrap(),
            vec![]
        );
    }

    #[test]
    fn transcript_descriptor_navigation_by_kind_reads_nearest_matching_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let mut records = (0..6)
            .map(|idx| transcript_record(idx * 10, &format!("block-{idx}"), &format!("text {idx}")))
            .collect::<Vec<_>>();
        records[1].kind = "user".into();
        records[4].kind = "user".into();
        db.apply_test_descriptors(&records).unwrap();

        assert_eq!(
            db.read_transcript_descriptor_before_kind_at_index("user", 5)
                .unwrap(),
            Some(without_indexed_text(records[4].clone()))
        );
        assert_eq!(
            db.read_transcript_descriptor_before_kind_at_index("user", 4)
                .unwrap(),
            Some(without_indexed_text(records[4].clone()))
        );
        assert_eq!(
            db.read_transcript_descriptor_before_kind_at_index("user", 3)
                .unwrap(),
            Some(without_indexed_text(records[1].clone()))
        );
        assert_eq!(
            db.read_transcript_descriptor_after_kind_at_index("user", 0)
                .unwrap(),
            Some(without_indexed_text(records[1].clone()))
        );
        assert_eq!(
            db.read_transcript_descriptor_after_kind_at_index("user", 1)
                .unwrap(),
            Some(without_indexed_text(records[1].clone()))
        );
        assert_eq!(
            db.read_transcript_descriptor_after_kind_at_index("user", 2)
                .unwrap(),
            Some(without_indexed_text(records[4].clone()))
        );
        assert_eq!(
            db.read_transcript_descriptor_before_kind_at_index("tool", 5)
                .unwrap(),
            None
        );
        assert_eq!(
            db.read_transcript_descriptor_after_kind_at_index("tool", 0)
                .unwrap(),
            None
        );
    }

    #[test]
    fn sparse_transcript_descriptor_reads_keep_metadata_object_backed() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let mut record = transcript_record(0, "tool", "tool text");
        let metadata_payload = "x".repeat(crate::history::METADATA_OBJECT_MIN_BYTES + 128);
        record.descriptor_json = serde_json::json!({
            "kind": "tool",
            "metadata": { "payload": metadata_payload },
        })
        .to_string();
        db.apply_test_descriptors(&[record]).unwrap();

        let full: serde_json::Value = serde_json::from_str(
            &db.read_all_transcript_descriptor_records().unwrap()[0].descriptor_json,
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

        let object = db.put_object(b"hello sqlite").unwrap();
        assert_eq!(object.codec(), ObjectCodec::None);
        assert_eq!(object.raw_size(), 12);
        assert_eq!(object.stored_size(), 12);
        assert_eq!(object.bytes, b"hello sqlite");

        let duplicate = db.put_object(b"hello sqlite").unwrap();
        assert_eq!(duplicate.hash(), object.hash());
        assert_eq!(db.object(object.hash()).unwrap().unwrap(), object);
    }

    #[test]
    fn object_meta_does_not_materialize_payload() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let object = db
            .put_object_with_compression(
                &representative_metadata_payload(),
                ObjectCompression::zstd(1, 128, 15),
            )
            .unwrap();

        let meta = db.object_meta(object.hash()).unwrap().unwrap();
        assert_eq!(meta.hash, object.hash());
        assert_eq!(meta.codec, ObjectCodec::Zstd);
        assert_eq!(db.object_bytes(&meta.hash).unwrap().unwrap(), object.bytes);
    }

    #[test]
    fn duplicate_object_write_keeps_intrinsic_storage_and_has_no_semantic_identity() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let bytes = representative_metadata_payload();

        let first = db
            .put_object_with_compression(&bytes, ObjectCompression::zstd(1, 128, 15))
            .unwrap();
        let second = db
            .put_object_with_compression(&bytes, ObjectCompression::none())
            .unwrap();

        assert_eq!(second.hash(), first.hash());
        assert_eq!(second.codec(), ObjectCodec::Zstd);
    }

    #[test]
    fn can_force_uncompressed_object_storage() {
        let dir = tempfile::tempdir().unwrap();
        let db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let bytes = representative_metadata_payload();

        let object = db.put_object_uncompressed(&bytes).unwrap();
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

        let object = db.put_object_with_compression(&bytes, compression).unwrap();
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

        let object = db.put_object_with_compression(&bytes, compression).unwrap();
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
    fn canonical_commit_appends_only_changed_history_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = protocol::HistoryItem::user(protocol::Content::text("first"));
        let second = protocol::HistoryItem::user(protocol::Content::text("second"));
        let mut snapshot = TestSessionFixture {
            state: TestSessionModel {
                id: "s1".into(),
                title: Some("title".into()),
                slug: Some("title".into()),
                first_user_message: None,
                cwd: Some("/tmp/project".into()),
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: Some("model-a".into()),
                fast_mode: None,
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
            history_start_idx: 0,
            history_len: 1,
            history: vec![first.clone()],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };

        let first_receipt = db.apply_test_fixture(&snapshot).unwrap();
        assert_eq!(first_receipt.previous.revision, Revision::ZERO);
        assert_eq!(first_receipt.current.revision, Revision::new(1));
        let first_created_at: i64 = db
            .connection()
            .query_row(
                "SELECT created_at FROM history_items WHERE idx = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();

        let no_op = db.apply_test_fixture(&snapshot).unwrap();
        assert_eq!(no_op.previous.revision, Revision::new(1));
        assert_eq!(no_op.current.revision, Revision::new(1));
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
        let append = db.apply_test_fixture(&snapshot).unwrap();
        assert_eq!(append.previous.revision, Revision::new(1));
        assert_eq!(append.current.revision, Revision::new(2));
        assert_eq!(db.load_full_session().unwrap().unwrap().history.len(), 2);
        assert_eq!(db.search_blob().unwrap(), "first\nuser\nsecond\nuser\n");
        assert_eq!(db.read_history_items_range(1..2).unwrap(), vec![second]);
        assert!(db.read_history_items_range(2..2).unwrap().is_empty());
    }

    #[test]
    fn combined_history_and_descriptor_commit_rolls_back_together() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = protocol::HistoryItem::user(protocol::Content::text("first"));
        let second = protocol::HistoryItem::user(protocol::Content::text("second"));
        let mut snapshot = TestSessionFixture {
            state: TestSessionModel {
                id: "s1".into(),
                title: Some("title".into()),
                slug: Some("title".into()),
                first_user_message: None,
                cwd: Some("/tmp/project".into()),
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: Some("model-a".into()),
                fast_mode: None,
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
            history_start_idx: 0,
            history_len: 1,
            history: vec![first.clone()],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };

        db.apply_test_fixture_with_descriptors(
            &snapshot,
            0,
            &[transcript_user_record_with_history(
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

        db.apply_test_fixture_with_descriptors(&snapshot, 1, &[invalid_record])
            .unwrap_err();

        let loaded = db.load_full_session().unwrap().unwrap();
        assert_eq!(loaded.history, vec![first]);
        assert_eq!(loaded.session.head.history_len, HistoryLen::new(1));
        assert_eq!(db.search_blob().unwrap(), "first descriptor\n");
        assert_eq!(
            db.read_all_transcript_descriptor_records().unwrap().len(),
            1
        );
    }

    #[test]
    fn history_append_preserves_descriptor_tail_without_descriptor_delta() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = protocol::HistoryItem::user(protocol::Content::text("first"));
        let second = protocol::HistoryItem::user(protocol::Content::text("second"));
        let mut snapshot = TestSessionFixture {
            state: TestSessionModel {
                id: "s1".into(),
                title: Some("title".into()),
                slug: Some("title".into()),
                first_user_message: None,
                cwd: Some("/tmp/project".into()),
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: Some("model-a".into()),
                fast_mode: None,
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
            history_start_idx: 0,
            history_len: 1,
            history: vec![first],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };

        db.apply_test_fixture(&snapshot).unwrap();
        db.apply_test_descriptors(&[
            TranscriptDescriptorRecord {
                block_idx: 0,
                history_idx: Some(0),
                kind: "user".into(),
                tool_call_id: None,
                tool_name: None,
                content_hash: "11".into(),
                estimated_text_bytes: 14,
                preview_text: "first detailed".into(),
                indexed_text: "first detailed".into(),
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
                indexed_text: "synthetic tail".into(),
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
        let append = db.apply_test_fixture(&snapshot).unwrap();

        assert_eq!(append.current.history_len, HistoryLen::new(2));
        assert_eq!(
            db.search_blob().unwrap(),
            "first detailed\nsynthetic tail\nsecond\nuser\n"
        );
        assert_eq!(db.load_full_session().unwrap().unwrap().history.len(), 2);
    }

    #[test]
    fn history_suffix_preserves_transcript_until_descriptor_delta() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = protocol::HistoryItem::user(protocol::Content::text("first"));
        let assistant = protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("assistant history")),
            None,
            Vec::new(),
        ));
        let old_request = protocol::HistoryItem::user(protocol::Content::text("old request"));
        let new_request = protocol::HistoryItem::user(protocol::Content::text("new request"));
        let mut snapshot = TestSessionFixture {
            state: TestSessionModel {
                id: "s1".into(),
                title: Some("title".into()),
                slug: Some("title".into()),
                first_user_message: None,
                cwd: Some("/tmp/project".into()),
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: Some("model-a".into()),
                fast_mode: None,
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
            history_start_idx: 0,
            history_len: 3,
            history: vec![first, assistant, old_request],
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            context_snapshots: Vec::new(),
        };

        db.apply_test_fixture(&snapshot).unwrap();
        db.apply_test_descriptors(&[
            transcript_user_record_with_history(0, 0, "first", "first descriptor"),
            transcript_record_with_history(1, 1, "thinking", "assistant thinking"),
            transcript_record_with_history(2, 1, "answer", "assistant answer"),
            transcript_user_record_with_history(3, 2, "old-request", "old request descriptor"),
        ])
        .unwrap();

        snapshot.history_start_idx = 2;
        snapshot.history = vec![new_request];
        snapshot.history_len = 3;
        snapshot.state.history_len = 3;
        snapshot.state.updated_at = 30;
        db.apply_test_fixture(&snapshot).unwrap();

        let search = db.search_blob().unwrap();
        assert!(search.contains("assistant answer\n"));
        assert!(search.contains("new request\n"));
        assert!(search.contains("old request descriptor\n"));
    }

    #[test]
    fn transcript_descriptors_roundtrip_and_feed_search_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        db.apply_test_descriptors(&[
            TranscriptDescriptorRecord {
                block_idx: 0,
                history_idx: None,
                kind: "assistant".into(),
                tool_call_id: None,
                tool_name: None,
                content_hash: "11".into(),
                estimated_text_bytes: 5,
                preview_text: "alpha".into(),
                indexed_text: "alpha".into(),
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
                indexed_text: "needle output".into(),
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

        let rows = db.read_all_transcript_descriptor_records().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(rows[1].estimated_text_bytes, 13);
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
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
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

        let id = db
            .append_request_attempt(&entry, RequestAuditPayloadMode::Full)
            .unwrap();
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
        let stats = db.request_audit_stats().unwrap();
        assert_eq!(stats.request_count, 1);
        assert_eq!(stats.streaming_count, 1);
        assert_eq!(stats.raw_response_count, 1);
        assert_eq!(stats.total_prompt_tokens, 10);
        assert_eq!(stats.total_completion_tokens, 5);
        assert_eq!(stats.total_cache_read_tokens, 2);
        assert_eq!(stats.total_cache_write_tokens, 1);
        assert_eq!(stats.total_reasoning_tokens, 3);
        assert_eq!(stats.total_elapsed_ms, 250);
        assert_eq!(stats.latest_timestamp_ms, Some(1000));
        assert_eq!(stats.first_request_ms, Some(1000));
        assert_eq!(stats.latest_provider_kind.as_deref(), Some("openai"));
        assert_eq!(stats.latest_model.as_deref(), Some("model-a"));
        assert_eq!(stats.latest_context_tokens, Some(20));
        assert_eq!(stats.max_context_tokens, Some(20));

        let payloads = db.request_payloads(id).unwrap().unwrap();
        assert_eq!(payloads.body.unwrap(), body);
        assert_eq!(payloads.response.unwrap()["raw"]["id"], "resp-1");
        assert!(payloads.error.is_none());
        let object_count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .unwrap();
        assert!(object_count >= 4);
        let manifest_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM request_object_refs WHERE role = 'body_manifest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(manifest_count, 1);
    }

    #[test]
    fn request_audit_summary_mode_omits_payload_objects() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let body = serde_json::json!({
            "model": "model-a",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let entry = protocol::request_log::RequestLogEntry {
            request_id: 10,
            kind: "turn".into(),
            turn_id: Some(10),
            ask_id: None,
            history_len: Some(1),
            timestamp_ms: 1000,
            provider_kind: "openai".into(),
            api_base: "https://api.example.test".into(),
            model: "model-a".into(),
            url: "https://api.example.test/v1/chat/completions".into(),
            http_status: Some(200),
            body: body.clone(),
            prompt_cache_key: None,
            stream: true,
            system_prompt: None,
            messages: None,
            tools: None,
            response: Some(protocol::request_log::RequestResponse {
                content: Some("hello".into()),
                reasoning: None,
                tool_calls: None,
                raw: Some(serde_json::json!({"id": "resp-1"})),
            }),
            usage: None,
            cost_usd: None,
            tokens_per_sec: None,
            elapsed_ms: Some(250),
            attempt: 1,
            error: None,
            background: false,
        };

        let id = db
            .append_request_attempt(&entry, RequestAuditPayloadMode::SUMMARY)
            .unwrap();
        let attempts = db
            .query_request_attempts(&RequestAuditQuery::default())
            .unwrap();
        assert_eq!(attempts[0].id, id);
        assert_eq!(
            attempts[0].raw_body_size,
            serde_json::to_vec(&body).unwrap().len() as u64
        );
        assert!(attempts[0].body_hash.is_none());
        assert!(attempts[0].response_hash.is_none());
        let payloads = db.request_payloads(id).unwrap().unwrap();
        assert!(payloads.body.is_none());
        assert!(payloads.response.is_none());
        let object_count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(object_count, 0);
    }

    #[test]
    fn request_audit_full_mode_dedupes_repeated_prefix_items() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let first = serde_json::json!({
            "model": "model-a",
            "input": [
                {"role": "user", "content": "one"},
                {"type": "function_call_output", "call_id": "c1", "output": "same"}
            ],
        });
        let second = serde_json::json!({
            "model": "model-a",
            "input": [
                {"role": "user", "content": "one"},
                {"type": "function_call_output", "call_id": "c1", "output": "same"},
                {"role": "assistant", "content": "two"}
            ],
        });
        let mut entry = protocol::request_log::RequestLogEntry {
            request_id: 11,
            kind: "turn".into(),
            turn_id: Some(11),
            ask_id: None,
            history_len: Some(1),
            timestamp_ms: 1000,
            provider_kind: "openai".into(),
            api_base: "https://api.example.test".into(),
            model: "model-a".into(),
            url: "https://api.example.test/v1/responses".into(),
            http_status: Some(200),
            body: first.clone(),
            prompt_cache_key: None,
            stream: true,
            system_prompt: None,
            messages: None,
            tools: None,
            response: None,
            usage: None,
            cost_usd: None,
            tokens_per_sec: None,
            elapsed_ms: Some(250),
            attempt: 1,
            error: None,
            background: false,
        };
        let first_id = db
            .append_request_attempt(&entry, RequestAuditPayloadMode::Full)
            .unwrap();
        entry.timestamp_ms = 2000;
        entry.body = second.clone();
        let second_id = db
            .append_request_attempt(&entry, RequestAuditPayloadMode::Full)
            .unwrap();

        assert_eq!(
            db.request_payloads(first_id)
                .unwrap()
                .unwrap()
                .body
                .unwrap(),
            first
        );
        assert_eq!(
            db.request_payloads(second_id)
                .unwrap()
                .unwrap()
                .body
                .unwrap(),
            second
        );
        let item_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(DISTINCT object_hash) FROM request_object_refs WHERE role = 'body_item'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(item_count, 3);
        let second_item_ref_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM request_object_refs WHERE request_attempt_id = ?1 AND role = 'body_item'",
                [second_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(second_item_ref_count, 3);
        let second_parent_ref_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM request_object_refs WHERE request_attempt_id = ?1 AND role = 'body_parent'",
                [second_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(second_parent_ref_count, 1);
        let second_manifest_hash: String = db
            .connection()
            .query_row(
                "SELECT object_hash FROM request_object_refs
                 WHERE request_attempt_id = ?1 AND role = 'body_manifest'",
                [second_id],
                |row| row.get(0),
            )
            .unwrap();
        let second_manifest: serde_json::Value =
            serde_json::from_slice(&db.object_bytes(&second_manifest_hash).unwrap().unwrap())
                .unwrap();
        assert!(second_manifest["parent_hash"].is_string());
        assert_eq!(second_manifest["item_hashes"].as_array().unwrap().len(), 1);
    }
    #[test]
    fn writes_session_meta_sidecar_from_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let state = TestSessionModel {
            id: "s1".into(),
            title: Some("title".into()),
            slug: Some("slug".into()),
            first_user_message: Some("hello".into()),
            cwd: Some("/tmp".into()),
            mode: Some("ask".into()),
            reasoning_effort: Some("low".into()),
            model: Some("model".into()),
            fast_mode: Some(true),
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

        db.apply_test_state(&state).unwrap();
        let mut expected = state;
        expected.revision = 1;
        expected.history_len = 0;
        assert_eq!(db.test_session_model().unwrap(), Some(expected));

        let meta_path = dir.path().join("meta.json");
        let meta = db.write_meta_sidecar(&meta_path).unwrap().unwrap();
        assert_eq!(meta.id, "s1");
        assert_eq!(meta.revision, 1);
        assert_eq!(meta.history_len, 0);
        assert_eq!(meta.fast_mode, Some(true));
        assert_eq!(meta.schema_version, schema::SCHEMA_VERSION);

        let from_file: SessionMeta = serde_json::from_slice(&fs::read(meta_path).unwrap()).unwrap();
        assert_eq!(from_file, meta);
    }

    #[test]
    fn session_identity_is_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let state = TestSessionModel {
            id: "s1".into(),
            title: None,
            slug: None,
            first_user_message: None,
            cwd: None,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
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

        db.apply_test_state(&state).unwrap();
        let err = db.apply_test_state(&next).unwrap_err();

        assert!(matches!(err, SessionCommitFailure::IdentityMismatch { .. }));
        let mut expected = state;
        expected.revision = 1;
        assert_eq!(db.test_session_model().unwrap(), Some(expected));
        let count: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM session_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn roundtrips_writer_owner() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = SessionDb::open(dir.path().join("session.db")).unwrap();
        let owner = WriterOwner {
            hostname: "host".into(),
            pid: 42,
            process_start_id: "process-start".into(),
            app_version: "test".into(),
            claimed_at: 10,
        };

        assert_eq!(db.writer_owner().unwrap(), None);
        db.claim_writer_owner("owner-token", &owner).unwrap();
        assert_eq!(db.writer_owner().unwrap(), Some(owner));
        db.release_writer_owner("owner-token").unwrap();
        assert_eq!(db.writer_owner().unwrap(), None);
    }

    #[test]
    fn corrupt_database_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.db");
        fs::write(&path, b"not a sqlite database").unwrap();

        let err = SessionDb::open(&path).unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)));
    }

    fn test_session_state(id: &str, history_len: usize) -> TestSessionModel {
        TestSessionModel {
            id: id.to_string(),
            title: None,
            slug: None,
            first_user_message: None,
            cwd: None,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
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

    fn commit_current_suffix(
        db: &mut SessionDb,
        state: TestSessionModel,
        history_start: usize,
        history: Vec<protocol::HistoryItem>,
        side_tables: Option<SideTableSuffixes>,
        descriptors: Option<(usize, Vec<TranscriptDescriptorRecord>)>,
    ) -> std::result::Result<SaveReceipt, SessionCommitFailure> {
        let expected = db
            .store_head()
            .map_err(session_commit_failure_from_store_error)?;
        let identity = test_identity_from_model(&state);
        let metadata =
            test_metadata_from_model(&state).map_err(session_commit_failure_from_store_error)?;
        db.apply_session_commit(&SessionCommit {
            session_id: state.id,
            save_id: crate::SaveId::new(expected.revision.get().saturating_add(1)),
            expected,
            identity,
            metadata,
            history: crate::HistorySuffix {
                start: HistoryIndex::new(history_start as u64),
                final_len: HistoryLen::new(state.history_len),
                items: history,
            },
            side_tables: side_tables.unwrap_or_else(|| SideTableSuffixes {
                start: HistoryIndex::new(history_start as u64),
                ..SideTableSuffixes::default()
            }),
            descriptors: descriptors.map(|(start, records)| crate::TranscriptDescriptorSuffix {
                start: DescriptorIndex::new(start as u64),
                records,
            }),
        })
    }

    fn transcript_user_record_with_history(
        block_idx: u64,
        history_idx: u64,
        label: &str,
        indexed_text: &str,
    ) -> TranscriptDescriptorRecord {
        let mut record =
            transcript_record_with_history(block_idx, history_idx, label, indexed_text);
        record.kind = "user".to_string();
        record.descriptor_json = serde_json::json!({
            "kind": "user",
            "label": label,
            "text": indexed_text,
        })
        .to_string();
        record
    }

    fn without_indexed_text(mut record: TranscriptDescriptorRecord) -> TranscriptDescriptorRecord {
        record.indexed_text.clear();
        record
    }

    fn transcript_record(
        block_idx: u64,
        label: &str,
        indexed_text: &str,
    ) -> TranscriptDescriptorRecord {
        TranscriptDescriptorRecord {
            block_idx,
            history_idx: None,
            kind: "assistant".to_string(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("hash-{label}"),
            estimated_text_bytes: indexed_text.len() as u64,
            preview_text: indexed_text.to_string(),
            indexed_text: indexed_text.to_string(),
            descriptor_json: serde_json::json!({
                "kind": "assistant",
                "label": label,
                "text": indexed_text,
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
        indexed_text: &str,
    ) -> TranscriptDescriptorRecord {
        TranscriptDescriptorRecord {
            block_idx,
            history_idx: Some(history_idx),
            kind: "assistant".to_string(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("hash-{label}"),
            estimated_text_bytes: indexed_text.len() as u64,
            preview_text: indexed_text.to_string(),
            indexed_text: indexed_text.to_string(),
            descriptor_json: serde_json::json!({
                "kind": "assistant",
                "label": label,
                "text": indexed_text,
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
