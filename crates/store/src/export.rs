use std::io::Write;

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::db::{self, unsigned};
use crate::error::{Result, StoreError};

pub(crate) fn history_jsonl(
    conn: &Connection,
    sid: i64,
    mut out: impl Write,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let mut stmt = conn.prepare_cached("SELECT body FROM history WHERE sid = ?1 ORDER BY idx")?;
    let mut rows = stmt.query([sid])?;
    while let Some(row) = rows.next()? {
        if cancelled() {
            return Err(StoreError::Cancelled);
        }
        out.write_all(&db::decode_blob(&row.get::<_, Vec<u8>>(0)?)?)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// The kinds of transcript records that make up a session's conversation text.
const CONVERSATION_KINDS: &str = "('user', 'assistant', 'compacted')";

pub(crate) fn conversation_text(conn: &Connection, sid: i64) -> Result<String> {
    let mut output = String::new();
    let mut push = |text: &str| {
        if !text.is_empty() {
            output.push_str(text);
            if !text.ends_with('\n') {
                output.push('\n');
            }
        }
    };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT indexed_text FROM transcript WHERE sid = ?1 AND kind IN {CONVERSATION_KINDS}
         ORDER BY idx"
    ))?;
    let mut rows = stmt.query([sid])?;
    let mut any = false;
    while let Some(row) = rows.next()? {
        any = true;
        push(&row.get::<_, String>(0)?);
    }
    // Sessions saved without transcript records fall back to their history.
    let records: i64 = conn.query_row(
        "SELECT record_count FROM sessions WHERE sid = ?1",
        [sid],
        |row| row.get(0),
    )?;
    if !any && records == 0 {
        let mut stmt =
            conn.prepare_cached("SELECT body FROM history WHERE sid = ?1 ORDER BY idx")?;
        let mut rows = stmt.query([sid])?;
        while let Some(row) = rows.next()? {
            let item: protocol::HistoryItem = db::decode_json(&row.get::<_, Vec<u8>>(0)?)?;
            if let Some(text) = crate::history::conversation_text(&item) {
                push(&text);
            }
        }
    }
    Ok(output)
}

pub(crate) fn requests_jsonl(conn: &Connection, sid: i64, mut out: impl Write) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, request_id, kind, turn_id, ask_id, started_at, completed_at, provider, model,
             history_len, error_summary, background, api_base, url, http_status,
             prompt_cache_key, stream, attempt, usage, cost_micros, tokens_per_sec
         FROM requests WHERE sid = ?1 ORDER BY started_at, id",
    )?;
    let mut rows = stmt.query([sid])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let started_at: i64 = row.get(5)?;
        let completed_at: Option<i64> = row.get(6)?;
        let error_summary: Option<String> = row.get(10)?;
        let usage: Option<Value> = row
            .get::<_, Option<String>>(18)?
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let mut value = json!({
            "request_id": row.get::<_, Option<String>>(1)?,
            "kind": row.get::<_, Option<String>>(2)?,
            "turn_id": row.get::<_, Option<String>>(3)?,
            "ask_id": row.get::<_, Option<String>>(4)?,
            "timestamp_ms": started_at,
            "provider_kind": row.get::<_, Option<String>>(7)?,
            "api_base": row.get::<_, Option<String>>(12)?,
            "model": row.get::<_, Option<String>>(8)?,
            "url": row.get::<_, Option<String>>(13)?,
            "http_status": row.get::<_, Option<i64>>(14)?,
            "history_len": row.get::<_, Option<i64>>(9)?,
            "prompt_cache_key": row.get::<_, Option<String>>(15)?,
            "stream": row.get::<_, bool>(16)?,
            "usage": usage,
            "cost_usd": row.get::<_, Option<i64>>(19)?.map(|micros| micros as f64 / 1_000_000.0),
            "tokens_per_sec": row.get::<_, Option<f64>>(20)?,
            "elapsed_ms": completed_at.map(|completed| unsigned(completed - started_at)),
            "attempt": row.get::<_, i64>(17)?,
            "background": row.get::<_, bool>(11)?,
        });
        let payloads = crate::requests::payloads(conn, sid, id)?;
        if let Some(payloads) = payloads {
            if let Some(body) = payloads.body {
                value["body"] = body;
            }
            if let Some(response) = payloads.response {
                value["response"] = response;
                value["response_payload_kind"] =
                    serde_json::to_value(payloads.response_payload_kind)?;
            }
            if let Some(error) = payloads.error {
                value["error"] = error;
            }
        }
        if value.get("error").is_none() {
            if let Some(summary) = error_summary {
                value["error"] = json!({ "message": summary });
            }
        }
        remove_null_fields(&mut value);
        serde_json::to_writer(&mut out, &value)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

fn remove_null_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, child| !child.is_null());
            map.values_mut().for_each(remove_null_fields);
        }
        Value::Array(values) => values.iter_mut().for_each(remove_null_fields),
        _ => {}
    }
}
