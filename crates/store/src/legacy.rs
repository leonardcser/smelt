use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use protocol::{history_from_messages, HistoryItem, Message};
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::history;
use crate::meta::{self, SessionState};
use crate::request_audit;
use crate::session_snapshot;

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

// COMPAT(session-split-jsonl) / COMPAT(session-json-monolith): import pre-SQLite
// session sidecars into canonical SQLite during the alpha migration window.
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
        history::rehydrate_object_refs(conn, &mut value)?;
        serde_json::to_writer(&mut out, &value)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

pub(crate) fn export_requests_jsonl(conn: &Connection, mut out: impl Write) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT a.id, body_hash, response_hash, error_hash, request_id, kind, turn_id, ask_id, started_at,
                completed_at, provider, model, history_len, error_summary, background,
                api_base, url, http_status, prompt_cache_key, stream, attempt,
                s.stats_json, s.total_cost_micros, s.tokens_per_sec
         FROM request_attempts a
         LEFT JOIN request_stats s ON s.request_attempt_id = a.id
         ORDER BY started_at, a.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, Option<String>>(11)?,
            row.get::<_, Option<i64>>(12)?,
            row.get::<_, Option<String>>(13)?,
            row.get::<_, i64>(14)?,
            row.get::<_, Option<String>>(15)?,
            row.get::<_, Option<String>>(16)?,
            row.get::<_, Option<i64>>(17)?,
            row.get::<_, Option<String>>(18)?,
            row.get::<_, i64>(19)?,
            row.get::<_, i64>(20)?,
            row.get::<_, Option<String>>(21)?,
            row.get::<_, Option<i64>>(22)?,
            row.get::<_, Option<f64>>(23)?,
        ))
    })?;

    for row in rows {
        let (
            id,
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
            api_base,
            url,
            http_status,
            prompt_cache_key,
            stream,
            attempt,
            stats_json,
            total_cost_micros,
            tokens_per_sec,
        ) = row?;
        let elapsed_ms = completed_at.map(|completed| completed.saturating_sub(started_at));
        let usage: Option<Value> = stats_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let cost_usd = total_cost_micros.map(|micros| micros as f64 / 1_000_000.0);

        let mut value = json!({
            "request_id": request_id,
            "kind": kind,
            "turn_id": turn_id,
            "ask_id": ask_id,
            "timestamp_ms": started_at,
            "provider_kind": provider,
            "api_base": api_base,
            "model": model,
            "url": url,
            "http_status": http_status,
            "history_len": history_len,
            "prompt_cache_key": prompt_cache_key,
            "stream": stream != 0,
            "usage": usage,
            "cost_usd": cost_usd,
            "tokens_per_sec": tokens_per_sec,
            "elapsed_ms": elapsed_ms,
            "attempt": attempt,
            "background": background != 0,
        });
        if body_hash.is_some() {
            if let Some(body) =
                request_audit::request_payloads(conn, id)?.and_then(|payloads| payloads.body)
            {
                value["body"] = body;
            }
        }
        if let Some(hash) = response_hash {
            history::insert_json_object(conn, &mut value, "response", &hash)?;
        }
        if let Some(hash) = error_hash {
            history::insert_json_object(conn, &mut value, "error", &hash)?;
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
    let canonical_meta = canonical_meta_from_legacy(&meta_value, report.history_items as u64);
    meta::set_meta(
        conn,
        session_snapshot::SESSION_META_JSON_KEY,
        &serde_json::to_string(&canonical_meta)?,
    )?;
    report.request_attempts = import_requests_jsonl(conn, session_dir, compression)?;
    report.objects = conn.query_row("SELECT COUNT(*) FROM objects", [], |row| {
        row.get::<_, i64>(0)
    })? as usize;
    meta::set_meta(conn, "import_source", import_source(session_dir))?;
    meta::set_meta(conn, "migration_status", "imported")?;
    Ok(report)
}

// COMPAT(session-split-jsonl) / COMPAT(session-json-monolith): accept legacy metadata
// sidecars as migration input for canonical SQLite storage.
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

// COMPAT(session-split-jsonl) / COMPAT(session-json-monolith): import old history rows
// from split `history.jsonl`, monolithic native `history`, or provider `messages`.
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
        history::write_history_item(conn, count, &item, compression)?;
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
        history::write_history_item(conn, count, &item, compression)?;
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

// COMPAT(session-v1-messages): old monolithic sessions used
// `context_snapshots`; canonical metadata stores `accounting_snapshots`.
fn canonical_meta_from_legacy(value: &Value, history_len: u64) -> Value {
    let mut meta = value.clone();
    if let Value::Object(map) = &mut meta {
        map.insert("schema_version".into(), Value::from(2));
        map.remove("history");
        map.remove("messages");
        map.entry("metadata_snapshots")
            .or_insert_with(|| Value::Array(Vec::new()));
        map.entry("turn_metas")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(context_snapshots) = map.remove("context_snapshots") {
            map.entry("accounting_snapshots")
                .or_insert(context_snapshots);
        } else {
            map.entry("accounting_snapshots")
                .or_insert_with(|| Value::Array(Vec::new()));
        }
        map.entry("session_usage")
            .or_insert_with(|| serde_json::json!({}));
        map.entry("session_cost_usd").or_insert(Value::from(0.0));
        map.entry("text_bytes").or_insert(Value::Null);
        map.entry("history_len").or_insert(Value::from(history_len));
    }
    meta
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
        first_user_message: optional_string(value, "first_user_message"),
        cwd: optional_string(value, "cwd"),
        mode: optional_string(value, "mode"),
        reasoning_effort: optional_string(value, "reasoning_effort"),
        model: optional_string(value, "model"),
        parent_id: optional_string(value, "parent_id"),
        accounting_json: value.get("session_usage").cloned(),
        checkpoint_json: value.get("checkpoint").cloned(),
        context_tokens: optional_u64(value, "context_tokens"),
        context_tokens_history_len: optional_u64(value, "context_tokens_history_len"),
        display_context_tokens: optional_u64(value, "display_context_tokens"),
        session_cost_usd: optional_f64(value, "session_cost_usd").unwrap_or_default(),
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

fn optional_f64(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

pub(crate) fn import_requests_jsonl(
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
    let values = serde_json::Deserializer::from_reader(reader).into_iter::<Value>();
    for value in values {
        request_audit::import_request_value(conn, &value?, compression)?;
        count += 1;
    }
    Ok(count)
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
