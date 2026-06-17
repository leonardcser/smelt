use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use protocol::{history_from_messages, HistoryItem, Message};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::meta::{self, SessionState};
use crate::object::{self, checked_i64, sha256_hex};

const METADATA_OBJECT_MIN_BYTES: usize = 4 * 1024;
const OBJECT_REF_KEY: &str = "$smelt_object_ref";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LegacyImportReport {
    pub history_items: usize,
    pub transcript_blocks: usize,
    pub request_attempts: usize,
    pub objects: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestAttemptSummary {
    pub id: i64,
    pub request_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub started_at: i64,
    pub history_len: Option<u64>,
    pub raw_body_size: u64,
    pub error_summary: Option<String>,
    pub background: bool,
}

pub(crate) fn import_session_dir(
    conn: &Connection,
    session_dir: &Path,
    compression: ObjectCompression,
) -> Result<LegacyImportReport> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = ensure_empty_import_target(conn)
        .and_then(|()| import_session_dir_inner(conn, session_dir, compression));
    match result {
        Ok(report) => {
            conn.execute_batch("COMMIT")?;
            Ok(report)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

fn ensure_empty_import_target(conn: &Connection) -> Result<()> {
    for table in [
        "history_items",
        "request_attempts",
        "session_state",
        "objects",
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
        if count != 0 {
            return Err(StoreError::Integrity(format!(
                "cannot import legacy session into non-empty database; table {table} has {count} rows"
            )));
        }
    }
    Ok(())
}

pub(crate) fn export_history_jsonl(conn: &Connection, mut out: impl Write) -> Result<()> {
    let mut stmt = conn.prepare("SELECT json FROM history_items ORDER BY idx")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let mut value: Value = serde_json::from_str(&row?)?;
        rehydrate_object_refs(conn, &mut value)?;
        serde_json::to_writer(&mut out, &value)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

pub(crate) fn export_requests_jsonl(conn: &Connection, mut out: impl Write) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT body_hash, response_hash, error_hash, request_id, kind, turn_id, ask_id, started_at,
                completed_at, provider, model, history_len, error_summary, background
         FROM request_attempts
         ORDER BY started_at, id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<String>>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, Option<i64>>(11)?,
            row.get::<_, Option<String>>(12)?,
            row.get::<_, i64>(13)?,
        ))
    })?;

    for row in rows {
        let (
            body_hash,
            response_hash,
            error_hash,
            request_id,
            kind,
            turn_id,
            ask_id,
            started_at,
            completed_at,
            provider,
            model,
            history_len,
            error_summary,
            background,
        ) = row?;
        let elapsed_ms = completed_at.map(|completed| completed.saturating_sub(started_at));

        let mut value = json!({
            "request_id": request_id,
            "kind": kind,
            "turn_id": turn_id,
            "ask_id": ask_id,
            "timestamp_ms": started_at,
            "provider_kind": provider,
            "model": model,
            "history_len": history_len,
            "elapsed_ms": elapsed_ms,
            "background": background != 0,
        });
        if let Some(hash) = body_hash {
            insert_json_object(conn, &mut value, "body", &hash)?;
        }
        if let Some(hash) = response_hash {
            insert_json_object(conn, &mut value, "response", &hash)?;
        }
        if let Some(hash) = error_hash {
            insert_json_object(conn, &mut value, "error", &hash)?;
        } else if let Some(summary) = error_summary {
            value["error"] = json!({ "message": summary });
        }
        remove_null_fields(&mut value);
        serde_json::to_writer(&mut out, &value)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

pub(crate) fn request_attempts(conn: &Connection) -> Result<Vec<RequestAttemptSummary>> {
    let mut stmt = conn.prepare(
        "SELECT id, request_id, provider, model, started_at, history_len, raw_body_size,
                error_summary, background
         FROM request_attempts
         ORDER BY started_at, id",
    )?;
    let rows = stmt.query_map([], |row| {
        let history_len: Option<i64> = row.get(5)?;
        let raw_body_size: i64 = row.get(6)?;
        Ok(RequestAttemptSummary {
            id: row.get(0)?,
            request_id: row.get(1)?,
            provider: row.get(2)?,
            model: row.get(3)?,
            started_at: row.get(4)?,
            history_len: history_len.map(|value| value as u64),
            raw_body_size: raw_body_size as u64,
            error_summary: row.get(7)?,
            background: row.get::<_, i64>(8)? != 0,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn import_session_dir_inner(
    conn: &Connection,
    session_dir: &Path,
    compression: ObjectCompression,
) -> Result<LegacyImportReport> {
    let meta_value = read_legacy_meta(session_dir)?;

    let mut report = LegacyImportReport::default();
    report.history_items = import_legacy_history(conn, session_dir, compression)?;
    report.transcript_blocks = report.history_items;
    let state = session_state_from_json(&meta_value, report.history_items as u64)?;
    meta::upsert_session_state(conn, &state)?;
    report.request_attempts = import_requests(conn, session_dir, compression)?;
    report.objects = conn.query_row("SELECT COUNT(*) FROM objects", [], |row| {
        row.get::<_, i64>(0)
    })? as usize;
    meta::set_meta(conn, "import_source", import_source(session_dir))?;
    meta::set_meta(conn, "migration_status", "imported")?;
    Ok(report)
}

fn read_legacy_meta(session_dir: &Path) -> Result<Value> {
    let meta_path = session_dir.join("meta.json");
    if meta_path.is_file() && session_dir.join("history.jsonl").is_file() {
        return read_json_file(&meta_path);
    }
    read_json_file(&session_dir.join("session.json"))
}

fn read_json_file(path: &Path) -> Result<Value> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn import_legacy_history(
    conn: &Connection,
    session_dir: &Path,
    compression: ObjectCompression,
) -> Result<usize> {
    let history_path = session_dir.join("history.jsonl");
    if history_path.is_file() {
        return import_history_jsonl(conn, &history_path, compression);
    }

    let session_value = read_json_file(&session_dir.join("session.json"))?;
    if let Some(history) = session_value.get("history") {
        let items: Vec<HistoryItem> = serde_json::from_value(history.clone())?;
        return import_history_items(conn, items.into_iter(), compression);
    }
    if let Some(messages) = session_value.get("messages") {
        let messages: Vec<Message> = serde_json::from_value(messages.clone())?;
        return import_history_items(
            conn,
            history_from_messages(messages).into_iter(),
            compression,
        );
    }
    Ok(0)
}

fn import_history_jsonl(
    conn: &Connection,
    path: &Path,
    compression: ObjectCompression,
) -> Result<usize> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut count = 0usize;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let item: HistoryItem = serde_json::from_str(&line)?;
        import_history_item(conn, count, item, compression)?;
        count += 1;
    }
    Ok(count)
}

fn import_history_items(
    conn: &Connection,
    items: impl Iterator<Item = HistoryItem>,
    compression: ObjectCompression,
) -> Result<usize> {
    let mut count = 0usize;
    for item in items {
        import_history_item(conn, count, item, compression)?;
        count += 1;
    }
    Ok(count)
}

fn import_source(session_dir: &Path) -> &'static str {
    if session_dir.join("meta.json").is_file() && session_dir.join("history.jsonl").is_file() {
        "split_jsonl"
    } else {
        "session_json"
    }
}

fn session_state_from_json(value: &Value, history_len: u64) -> Result<SessionState> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown-session")
        .to_string();
    Ok(SessionState {
        id,
        title: optional_string(value, "title"),
        slug: optional_string(value, "slug"),
        cwd: optional_string(value, "cwd"),
        mode: optional_string(value, "mode"),
        model: optional_string(value, "model"),
        accounting_json: value.get("session_usage").cloned(),
        checkpoint_json: value.get("checkpoint").cloned(),
        revision: history_len,
        history_len,
        created_at: optional_u64(value, "created_at_ms").unwrap_or_default() as i64,
        updated_at: optional_u64(value, "updated_at_ms").unwrap_or_default() as i64,
    })
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn optional_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn import_history_item(
    conn: &Connection,
    idx: usize,
    item: HistoryItem,
    compression: ObjectCompression,
) -> Result<()> {
    let mut normalized = serde_json::to_value(item)?;
    let mut refs = Vec::new();
    normalize_metadata(conn, &mut normalized, compression, &mut refs)?;
    let json = serde_json::to_string(&normalized)?;
    let hash = sha256_hex(json.as_bytes());
    let kind = normalized
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let search_text = collect_text(&normalized, 64 * 1024);
    conn.execute(
        "INSERT INTO history_items (idx, kind, json, hash, search_text, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())",
        params![idx as i64, kind, json, hash, search_text],
    )?;
    for (hash, role) in refs {
        conn.execute(
            "INSERT OR IGNORE INTO history_object_refs (history_idx, object_hash, role)
             VALUES (?1, ?2, ?3)",
            params![idx as i64, hash, role],
        )?;
    }
    insert_transcript_block(conn, idx as i64, kind, &normalized, &search_text, &hash)
}

fn normalize_metadata(
    conn: &Connection,
    value: &mut Value,
    compression: ObjectCompression,
    refs: &mut Vec<(String, &'static str)>,
) -> Result<()> {
    match value {
        Value::Object(map) => {
            let keys = map.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let Some(child) = map.get_mut(&key) else {
                    continue;
                };
                if key == "metadata" && !child.is_null() {
                    let bytes = serde_json::to_vec(child)?;
                    if bytes.len() >= METADATA_OBJECT_MIN_BYTES {
                        let object =
                            object::put_object(conn, "tool_metadata", &bytes, compression)?;
                        refs.push((object.hash().to_string(), "metadata"));
                        *child = object_ref_json(object.hash(), object.raw_size());
                        continue;
                    }
                }
                normalize_metadata(conn, child, compression, refs)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                normalize_metadata(conn, child, compression, refs)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn object_ref_json(hash: &str, raw_size: u64) -> Value {
    json!({ OBJECT_REF_KEY: { "hash": hash, "raw_size": raw_size } })
}

fn insert_transcript_block(
    conn: &Connection,
    idx: i64,
    kind: &str,
    value: &Value,
    search_text: &str,
    content_hash: &str,
) -> Result<()> {
    let tool_call_id =
        find_string_key(value, "call_id").or_else(|| find_string_key(value, "tool_call_id"));
    let tool_name = find_string_key(value, "name");
    let preview = preview(search_text, 512);
    conn.execute(
        "INSERT INTO transcript_blocks (
            block_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
            estimated_text_bytes, preview_text, search_text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            idx,
            idx,
            kind,
            tool_call_id,
            tool_name,
            content_hash,
            checked_i64(search_text.len() as u64, "estimated_text_bytes")?,
            preview,
            search_text
        ],
    )?;
    conn.execute(
        "INSERT INTO transcript_search (block_idx, history_idx, text)
         VALUES (?1, ?2, ?3)",
        params![idx, idx, search_text],
    )?;
    Ok(())
}

fn import_requests(
    conn: &Connection,
    session_dir: &Path,
    compression: ObjectCompression,
) -> Result<usize> {
    let path = session_dir.join("requests.jsonl");
    if !path.is_file() {
        return Ok(0);
    }
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut count = 0usize;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line)?;
        insert_request_attempt(conn, &value, compression)?;
        count += 1;
    }
    Ok(count)
}

fn insert_request_attempt(
    conn: &Connection,
    value: &Value,
    compression: ObjectCompression,
) -> Result<()> {
    let body_hash = put_json_object(conn, value.get("body"), "request_body", compression)?;
    let response_hash =
        put_json_object(conn, value.get("response"), "request_response", compression)?;
    let error_hash = put_json_object(conn, value.get("error"), "request_error", compression)?;
    let started_at = value
        .get("timestamp_ms")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let elapsed_ms = value.get("elapsed_ms").and_then(Value::as_i64);
    let completed_at = elapsed_ms.map(|elapsed| started_at.saturating_add(elapsed));
    let raw_body_size = body_hash
        .as_ref()
        .and_then(|hash| object::object_meta(conn, hash).transpose())
        .transpose()?
        .map(|meta| meta.raw_size)
        .unwrap_or_default();

    conn.execute(
        "INSERT INTO request_attempts (
            request_id, turn_id, ask_id, started_at, completed_at, provider, model,
            history_len, body_hash, response_hash, error_hash, kind,
            error_summary, background, raw_body_size
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            scalar_string(value.get("request_id")),
            scalar_string(value.get("turn_id")),
            scalar_string(value.get("ask_id")),
            started_at,
            completed_at,
            optional_string_multi(value, &["provider_kind", "provider"]),
            optional_string(value, "model"),
            value.get("history_len").and_then(Value::as_i64),
            body_hash.clone(),
            response_hash.clone(),
            error_hash.clone(),
            optional_string(value, "kind"),
            request_error_summary(value.get("error")),
            value
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false) as i64,
            checked_i64(raw_body_size, "raw_body_size")?,
        ],
    )?;
    let request_attempt_id = conn.last_insert_rowid();
    if let Some(hash) = &body_hash {
        insert_request_ref(conn, request_attempt_id, hash, "body")?;
    }
    if let Some(hash) = &response_hash {
        insert_request_ref(conn, request_attempt_id, hash, "response")?;
    }
    if let Some(hash) = &error_hash {
        insert_request_ref(conn, request_attempt_id, hash, "error")?;
    }
    if let Some(usage) = value.get("usage") {
        conn.execute(
            "INSERT OR REPLACE INTO request_stats (request_attempt_id, stats_json)
             VALUES (?1, ?2)",
            params![request_attempt_id, serde_json::to_string(usage)?],
        )?;
    }
    Ok(())
}

fn put_json_object(
    conn: &Connection,
    value: Option<&Value>,
    kind: &str,
    compression: ObjectCompression,
) -> Result<Option<String>> {
    value_hash(value, conn, kind, compression)
}

fn value_hash(
    value: Option<&Value>,
    conn: &Connection,
    kind: &str,
    compression: ObjectCompression,
) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let bytes = serde_json::to_vec(value)?;
    Ok(Some(
        object::put_object(conn, kind, &bytes, compression)?
            .hash()
            .to_string(),
    ))
}

fn insert_request_ref(conn: &Connection, request_id: i64, hash: &str, role: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO request_object_refs (request_attempt_id, object_hash, role)
         VALUES (?1, ?2, ?3)",
        params![request_id, hash, role],
    )?;
    Ok(())
}

fn request_error_summary(value: Option<&Value>) -> Option<String> {
    let value = value?;
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("kind").and_then(Value::as_str))
        .map(ToString::to_string)
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn optional_string_multi(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| optional_string(value, key))
}

fn collect_text(value: &Value, max_bytes: usize) -> String {
    let mut out = String::new();
    collect_text_inner(value, &mut out, max_bytes);
    out
}

fn collect_text_inner(value: &Value, out: &mut String, max_bytes: usize) {
    if out.len() >= max_bytes {
        return;
    }
    match value {
        Value::String(text) => {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
            truncate_utf8(out, max_bytes);
        }
        Value::Array(values) => {
            for value in values {
                collect_text_inner(value, out, max_bytes);
            }
        }
        Value::Object(map) => {
            if map.contains_key(OBJECT_REF_KEY) {
                return;
            }
            for value in map.values() {
                collect_text_inner(value, out, max_bytes);
            }
        }
        _ => {}
    }
}

fn truncate_utf8(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = 0;
    for (idx, _) in text.char_indices() {
        if idx > max_bytes {
            break;
        }
        end = idx;
    }
    text.truncate(end);
}

fn preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = 0;
    for (idx, _) in text.char_indices() {
        if idx > max_bytes {
            break;
        }
        end = idx;
    }
    text[..end].to_string()
}

fn find_string_key(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(value) = map.get(key).and_then(Value::as_str) {
                return Some(value.to_string());
            }
            map.values().find_map(|value| find_string_key(value, key))
        }
        Value::Array(values) => values.iter().find_map(|value| find_string_key(value, key)),
        _ => None,
    }
}

fn rehydrate_object_refs(conn: &Connection, value: &mut Value) -> Result<()> {
    match value {
        Value::Object(map) => {
            if let Some(hash) = map
                .get(OBJECT_REF_KEY)
                .and_then(|value| value.get("hash"))
                .and_then(Value::as_str)
            {
                if let Some(bytes) = object_bytes_by_hash(conn, hash)? {
                    *value = serde_json::from_slice(&bytes)?;
                }
                return Ok(());
            }
            for child in map.values_mut() {
                rehydrate_object_refs(conn, child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                rehydrate_object_refs(conn, child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn remove_null_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, child| !child.is_null());
            for child in map.values_mut() {
                remove_null_fields(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                remove_null_fields(child);
            }
        }
        _ => {}
    }
}

fn object_bytes_by_hash(conn: &Connection, hash: &str) -> Result<Option<Vec<u8>>> {
    let Some(meta) = object::object_meta(conn, hash)? else {
        return Ok(None);
    };
    object::object_bytes(conn, &meta).map(Some)
}

fn insert_json_object(conn: &Connection, value: &mut Value, key: &str, hash: &str) -> Result<()> {
    let Some(bytes) = object_bytes_by_hash(conn, hash)? else {
        return Ok(());
    };
    value[key] = serde_json::from_slice(&bytes)?;
    Ok(())
}
