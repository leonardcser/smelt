use protocol::request_log::RequestLogEntry;
use protocol::TokenUsage;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde_json::Value;

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::object::{self, checked_i64};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestAuditOrder {
    NewestFirst,
    OldestFirst,
    LargestBody,
    Costliest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestAuditQuery {
    pub limit: u32,
    pub offset: u32,
    pub order: RequestAuditOrder,
    pub started_at_from_ms: Option<u64>,
    pub started_at_to_ms: Option<u64>,
    pub request_id: Option<String>,
    pub turn_id: Option<String>,
    pub ask_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub errors_only: bool,
    pub min_body_size: Option<u64>,
    pub min_input_tokens: Option<u64>,
    pub min_output_tokens: Option<u64>,
    pub min_cost_micros: Option<i64>,
}

impl Default for RequestAuditQuery {
    fn default() -> Self {
        Self {
            limit: 500,
            offset: 0,
            order: RequestAuditOrder::NewestFirst,
            started_at_from_ms: None,
            started_at_to_ms: None,
            request_id: None,
            turn_id: None,
            ask_id: None,
            provider: None,
            model: None,
            errors_only: false,
            min_body_size: None,
            min_input_tokens: None,
            min_output_tokens: None,
            min_cost_micros: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RequestAuditSummary {
    pub id: i64,
    pub request_id: Option<String>,
    pub kind: Option<String>,
    pub turn_id: Option<String>,
    pub ask_id: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub url: Option<String>,
    pub http_status: Option<u16>,
    pub history_len: Option<u64>,
    pub attempt: u32,
    pub stream: bool,
    pub prompt_cache_key: Option<String>,
    pub background: bool,
    pub raw_body_size: u64,
    pub body_hash: Option<String>,
    pub response_hash: Option<String>,
    pub response_summary: Option<String>,
    pub error_hash: Option<String>,
    pub error_summary: Option<String>,
    pub usage: Option<TokenUsage>,
    pub cost_usd: Option<f64>,
    pub tokens_per_sec: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestAuditPayloads {
    pub body: Option<Value>,
    pub response: Option<Value>,
    pub error: Option<Value>,
}

pub(crate) fn append_request_attempt(
    conn: &Connection,
    entry: &RequestLogEntry,
    compression: ObjectCompression,
) -> Result<i64> {
    let body_bytes = serde_json::to_vec(&entry.body)?;
    let body = object::put_object(conn, "request_body", &body_bytes, compression)?;
    let response_hash = put_json_object(
        conn,
        entry.response.as_ref(),
        "request_response",
        compression,
    )?;
    let error_hash = put_json_object(conn, entry.error.as_ref(), "request_error", compression)?;
    let started_at = checked_i64(entry.timestamp_ms, "started_at")?;
    let completed_at = entry
        .elapsed_ms
        .map(|elapsed| entry.timestamp_ms.saturating_add(elapsed))
        .map(|value| checked_i64(value, "completed_at"))
        .transpose()?;
    let raw_body_size = checked_i64(body.raw_size(), "raw_body_size")?;
    let cost_micros = cost_micros(entry.cost_usd)?;
    let stats_json = entry
        .usage
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    conn.execute(
        "INSERT INTO request_attempts (
            request_id, turn_id, ask_id, started_at, completed_at, provider, model,
            history_len, body_hash, response_hash, error_hash, kind, error_summary,
            background, raw_body_size, api_base, url, http_status, prompt_cache_key,
            stream, attempt, response_summary
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                   ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
        params![
            entry.request_id.to_string(),
            entry.turn_id.map(|id| id.to_string()),
            entry.ask_id.map(|id| id.to_string()),
            started_at,
            completed_at,
            &entry.provider_kind,
            &entry.model,
            entry.history_len.map(|value| value as i64),
            body.hash(),
            response_hash.as_deref(),
            error_hash.as_deref(),
            &entry.kind,
            entry.error.as_ref().map(request_error_summary),
            entry.background as i64,
            raw_body_size,
            &entry.api_base,
            &entry.url,
            entry.http_status.map(i64::from),
            entry.prompt_cache_key.as_deref(),
            entry.stream as i64,
            i64::from(entry.attempt),
            entry.response.as_ref().and_then(response_summary),
        ],
    )?;
    let request_attempt_id = conn.last_insert_rowid();
    insert_request_ref(conn, request_attempt_id, body.hash(), "body")?;
    if let Some(hash) = response_hash.as_deref() {
        insert_request_ref(conn, request_attempt_id, hash, "response")?;
    }
    if let Some(hash) = error_hash.as_deref() {
        insert_request_ref(conn, request_attempt_id, hash, "error")?;
    }
    if entry.usage.is_some() || cost_micros.is_some() || entry.tokens_per_sec.is_some() {
        let usage = entry.usage.as_ref();
        conn.execute(
            "INSERT OR REPLACE INTO request_stats (
                request_attempt_id, input_tokens, output_tokens, cached_input_tokens,
                reasoning_tokens, total_cost_micros, stats_json, context_tokens,
                cache_write_tokens, tokens_per_sec
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                request_attempt_id,
                usage.and_then(|usage| usage.prompt_tokens).map(i64::from),
                usage
                    .and_then(|usage| usage.completion_tokens)
                    .map(i64::from),
                usage
                    .and_then(|usage| usage.cache_read_tokens)
                    .map(i64::from),
                usage
                    .and_then(|usage| usage.reasoning_tokens)
                    .map(i64::from),
                cost_micros,
                stats_json,
                usage.and_then(|usage| usage.context_tokens).map(i64::from),
                usage
                    .and_then(|usage| usage.cache_write_tokens)
                    .map(i64::from),
                entry.tokens_per_sec,
            ],
        )?;
    }
    Ok(request_attempt_id)
}

pub(crate) fn request_attempts(
    conn: &Connection,
    query: &RequestAuditQuery,
) -> Result<Vec<RequestAuditSummary>> {
    let mut sql = String::from(
        "SELECT a.id, a.request_id, a.kind, a.turn_id, a.ask_id, a.started_at, a.completed_at,
                a.provider, a.model, a.api_base, a.url, a.http_status, a.history_len, a.attempt,
                a.stream, a.prompt_cache_key, a.background, a.raw_body_size, a.body_hash,
                a.response_hash, a.response_summary, a.error_hash, a.error_summary,
                s.stats_json, s.total_cost_micros, s.tokens_per_sec
         FROM request_attempts a
         LEFT JOIN request_stats s ON s.request_attempt_id = a.id",
    );
    let mut clauses: Vec<&str> = Vec::new();
    let mut values: Vec<Box<dyn ToSql>> = Vec::new();
    push_i64(
        &mut clauses,
        &mut values,
        "a.started_at >= ?",
        query.started_at_from_ms,
        "started_at_from_ms",
    )?;
    push_i64(
        &mut clauses,
        &mut values,
        "a.started_at <= ?",
        query.started_at_to_ms,
        "started_at_to_ms",
    )?;
    push_string(
        &mut clauses,
        &mut values,
        "a.request_id = ?",
        &query.request_id,
    );
    push_string(&mut clauses, &mut values, "a.turn_id = ?", &query.turn_id);
    push_string(&mut clauses, &mut values, "a.ask_id = ?", &query.ask_id);
    push_string(&mut clauses, &mut values, "a.provider = ?", &query.provider);
    push_string(&mut clauses, &mut values, "a.model = ?", &query.model);
    if query.errors_only {
        clauses.push("a.error_summary IS NOT NULL");
    }
    push_i64(
        &mut clauses,
        &mut values,
        "a.raw_body_size >= ?",
        query.min_body_size,
        "min_body_size",
    )?;
    push_i64(
        &mut clauses,
        &mut values,
        "s.input_tokens >= ?",
        query.min_input_tokens,
        "min_input_tokens",
    )?;
    push_i64(
        &mut clauses,
        &mut values,
        "s.output_tokens >= ?",
        query.min_output_tokens,
        "min_output_tokens",
    )?;
    if let Some(min_cost_micros) = query.min_cost_micros {
        clauses.push("s.total_cost_micros >= ?");
        values.push(Box::new(min_cost_micros));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(match query.order {
        RequestAuditOrder::NewestFirst => " ORDER BY a.started_at DESC, a.id DESC",
        RequestAuditOrder::OldestFirst => " ORDER BY a.started_at ASC, a.id ASC",
        RequestAuditOrder::LargestBody => " ORDER BY a.raw_body_size DESC, a.started_at DESC, a.id DESC",
        RequestAuditOrder::Costliest => {
            " ORDER BY s.total_cost_micros IS NULL, s.total_cost_micros DESC, a.started_at DESC, a.id DESC"
        }
    });
    sql.push_str(" LIMIT ? OFFSET ?");
    values.push(Box::new(i64::from(query.limit)));
    values.push(Box::new(i64::from(query.offset)));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params_from_iter(values.iter().map(|value| value.as_ref())),
        |row| {
            let history_len: Option<i64> = row.get(12)?;
            let http_status: Option<i64> = row.get(11)?;
            let raw_body_size: i64 = row.get(17)?;
            let stats_json: Option<String> = row.get(23)?;
            let cost_micros: Option<i64> = row.get(24)?;
            Ok(RequestAuditSummary {
                id: row.get(0)?,
                request_id: row.get(1)?,
                kind: row.get(2)?,
                turn_id: row.get(3)?,
                ask_id: row.get(4)?,
                started_at: row.get(5)?,
                completed_at: row.get(6)?,
                provider: row.get(7)?,
                model: row.get(8)?,
                api_base: row.get(9)?,
                url: row.get(10)?,
                http_status: http_status.map(|value| value as u16),
                history_len: history_len.map(|value| value as u64),
                attempt: row.get::<_, i64>(13)? as u32,
                stream: row.get::<_, i64>(14)? != 0,
                prompt_cache_key: row.get(15)?,
                background: row.get::<_, i64>(16)? != 0,
                raw_body_size: raw_body_size as u64,
                body_hash: row.get(18)?,
                response_hash: row.get(19)?,
                response_summary: row.get(20)?,
                error_hash: row.get(21)?,
                error_summary: row.get(22)?,
                usage: stats_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(crate::error::to_sql_error)?,
                cost_usd: cost_micros.map(|micros| micros as f64 / 1_000_000.0),
                tokens_per_sec: row.get(25)?,
            })
        },
    )?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub(crate) fn request_payloads(
    conn: &Connection,
    request_attempt_id: i64,
) -> Result<Option<RequestAuditPayloads>> {
    let hashes = conn
        .query_row(
            "SELECT body_hash, response_hash, error_hash FROM request_attempts WHERE id = ?1",
            [request_attempt_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((body_hash, response_hash, error_hash)) = hashes else {
        return Ok(None);
    };
    Ok(Some(RequestAuditPayloads {
        body: read_json_object(conn, body_hash.as_deref())?,
        response: read_json_object(conn, response_hash.as_deref())?,
        error: read_json_object(conn, error_hash.as_deref())?,
    }))
}

fn put_json_object<T: serde::Serialize>(
    conn: &Connection,
    value: Option<&T>,
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

pub(crate) fn insert_request_ref(
    conn: &Connection,
    request_id: i64,
    hash: &str,
    role: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO request_object_refs (request_attempt_id, object_hash, role)
         VALUES (?1, ?2, ?3)",
        params![request_id, hash, role],
    )?;
    Ok(())
}

fn read_json_object(conn: &Connection, hash: Option<&str>) -> Result<Option<Value>> {
    let Some(hash) = hash else {
        return Ok(None);
    };
    let Some(meta) = object::object_meta(conn, hash)? else {
        return Ok(None);
    };
    let bytes = object::object_bytes(conn, &meta)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn request_error_summary(error: &protocol::request_log::RequestError) -> String {
    if error.message.is_empty() {
        error.kind.clone()
    } else {
        error.message.clone()
    }
}

fn response_summary(response: &protocol::request_log::RequestResponse) -> Option<String> {
    response
        .content
        .as_deref()
        .or(response.reasoning.as_deref())
        .map(|text| preview(text, 512))
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

fn cost_micros(cost_usd: Option<f64>) -> Result<Option<i64>> {
    let Some(cost_usd) = cost_usd else {
        return Ok(None);
    };
    if !cost_usd.is_finite() {
        return Ok(None);
    }
    let micros = (cost_usd * 1_000_000.0).round();
    if micros < i64::MIN as f64 || micros > i64::MAX as f64 {
        return Err(StoreError::Integrity(
            "cost_usd overflows i64 micros".into(),
        ));
    }
    Ok(Some(micros as i64))
}

fn push_string(
    clauses: &mut Vec<&'static str>,
    values: &mut Vec<Box<dyn ToSql>>,
    clause: &'static str,
    value: &Option<String>,
) {
    if let Some(value) = value {
        clauses.push(clause);
        values.push(Box::new(value.clone()));
    }
}

fn push_i64(
    clauses: &mut Vec<&'static str>,
    values: &mut Vec<Box<dyn ToSql>>,
    clause: &'static str,
    value: Option<u64>,
    field: &str,
) -> Result<()> {
    if let Some(value) = value {
        clauses.push(clause);
        values.push(Box::new(checked_i64(value, field)?));
    }
    Ok(())
}
