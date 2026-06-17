use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::legacy::{self, LegacyImportReport, RequestAttemptSummary};
use crate::meta::{self, SessionMeta, SessionState, WriterLease};
use crate::object::{self, ObjectMeta, StoredObject};
use crate::schema;

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

    pub fn export_history_jsonl(&self, out: impl Write) -> Result<()> {
        legacy::export_history_jsonl(&self.conn, out)
    }

    pub fn export_requests_jsonl(&self, out: impl Write) -> Result<()> {
        legacy::export_requests_jsonl(&self.conn, out)
    }

    pub fn request_attempts(&self) -> Result<Vec<RequestAttemptSummary>> {
        legacy::request_attempts(&self.conn)
    }
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
    use crate::{
        benchmark_zstd_compression, ObjectCodec, DEFAULT_ZSTD_LEVEL,
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
            cwd: Some("/tmp".into()),
            mode: Some("ask".into()),
            model: Some("model".into()),
            accounting_json: Some(serde_json::json!({"cost": 1})),
            checkpoint_json: None,
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
            cwd: None,
            mode: None,
            model: None,
            accounting_json: None,
            checkpoint_json: None,
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
