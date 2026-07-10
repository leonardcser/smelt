use protocol::request_log::RequestLogEntry;
use protocol::TokenUsage;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde_json::Value;

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::object::{self, checked_i64};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestAuditPayloadMode {
    Summary { raw_body_size: Option<u64> },
    Full,
}

impl RequestAuditPayloadMode {
    pub const SUMMARY: Self = Self::Summary {
        raw_body_size: None,
    };
}

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

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct RequestAuditStats {
    pub request_count: u64,
    pub error_count: u64,
    pub streaming_count: u64,
    pub raw_response_count: u64,
    pub total_cost_usd: f64,
    pub total_elapsed_ms: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_write_tokens: u64,
    pub total_reasoning_tokens: u64,
    pub latest_timestamp_ms: Option<u64>,
    pub first_request_ms: Option<u64>,
    pub latest_provider_kind: Option<String>,
    pub latest_model: Option<String>,
    pub latest_context_tokens: Option<u32>,
    pub max_context_tokens: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestAuditPayloads {
    pub body: Option<Value>,
    pub response: Option<Value>,
    pub error: Option<Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct RequestAuditRecord {
    pub request_id: Option<String>,
    pub kind: Option<String>,
    pub turn_id: Option<String>,
    pub ask_id: Option<String>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub history_len: Option<i64>,
    pub body: Option<Value>,
    pub response: Option<Value>,
    pub error: Option<Value>,
    pub error_summary: Option<String>,
    pub background: bool,
    pub api_base: Option<String>,
    pub url: Option<String>,
    pub http_status: Option<i64>,
    pub prompt_cache_key: Option<String>,
    pub stream: bool,
    pub attempt: i64,
    pub response_summary: Option<String>,
    pub usage: Option<TokenUsage>,
    pub cost_usd: Option<f64>,
    pub tokens_per_sec: Option<f64>,
}

impl RequestAuditRecord {
    fn from_entry(entry: &RequestLogEntry) -> Result<Self> {
        let started_at = checked_i64(entry.timestamp_ms, "started_at")?;
        let completed_at = entry
            .elapsed_ms
            .map(|elapsed| entry.timestamp_ms.saturating_add(elapsed))
            .map(|value| checked_i64(value, "completed_at"))
            .transpose()?;
        let response = entry
            .response
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        let error = entry.error.as_ref().map(serde_json::to_value).transpose()?;
        Ok(Self {
            request_id: Some(entry.request_id.to_string()),
            kind: Some(entry.kind.clone()),
            turn_id: entry.turn_id.map(|id| id.to_string()),
            ask_id: entry.ask_id.map(|id| id.to_string()),
            started_at,
            completed_at,
            provider: Some(entry.provider_kind.clone()),
            model: Some(entry.model.clone()),
            history_len: entry.history_len.map(|value| value as i64),
            body: Some(entry.body.clone()),
            response,
            error,
            error_summary: entry.error.as_ref().map(request_error_summary),
            background: entry.background,
            api_base: Some(entry.api_base.clone()),
            url: Some(entry.url.clone()),
            http_status: entry.http_status.map(i64::from),
            prompt_cache_key: entry.prompt_cache_key.clone(),
            stream: entry.stream,
            attempt: i64::from(entry.attempt),
            response_summary: entry.response.as_ref().and_then(response_summary),
            usage: entry.usage.clone(),
            cost_usd: entry.cost_usd,
            tokens_per_sec: entry.tokens_per_sec,
        })
    }
}

pub(crate) fn append_request_attempt(
    conn: &Connection,
    entry: &RequestLogEntry,
    compression: ObjectCompression,
    payload_mode: RequestAuditPayloadMode,
) -> Result<i64> {
    insert_request_record(
        conn,
        RequestAuditRecord::from_entry(entry)?,
        compression,
        payload_mode,
    )
}

fn insert_request_record(
    conn: &Connection,
    record: RequestAuditRecord,
    compression: ObjectCompression,
    payload_mode: RequestAuditPayloadMode,
) -> Result<i64> {
    let raw_body_size = match payload_mode {
        RequestAuditPayloadMode::Summary {
            raw_body_size: Some(raw_body_size),
        } => raw_body_size,
        RequestAuditPayloadMode::Summary {
            raw_body_size: None,
        }
        | RequestAuditPayloadMode::Full => record
            .body
            .as_ref()
            .map(json_size)
            .transpose()?
            .unwrap_or_default(),
    };
    let raw_body_size = checked_i64(raw_body_size, "raw_body_size")?;
    let body_hash = match payload_mode {
        RequestAuditPayloadMode::Summary { .. } => None,
        RequestAuditPayloadMode::Full => {
            put_request_body_manifest(conn, record.body.as_ref(), compression)?
        }
    };
    let response_hash = match payload_mode {
        RequestAuditPayloadMode::Summary { .. } => None,
        RequestAuditPayloadMode::Full => put_json_object(
            conn,
            record.response.as_ref(),
            "request_response",
            compression,
        )?,
    };
    let error_hash = match payload_mode {
        RequestAuditPayloadMode::Summary { .. } => None,
        RequestAuditPayloadMode::Full => {
            put_json_object(conn, record.error.as_ref(), "request_error", compression)?
        }
    };
    let cost_micros = cost_micros(record.cost_usd)?;
    let stats_json = record
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
            record.request_id,
            record.turn_id,
            record.ask_id,
            record.started_at,
            record.completed_at,
            record.provider,
            record.model,
            record.history_len,
            body_hash.as_deref(),
            response_hash.as_deref(),
            error_hash.as_deref(),
            record.kind,
            record.error_summary,
            record.background as i64,
            raw_body_size,
            record.api_base,
            record.url,
            record.http_status,
            record.prompt_cache_key,
            record.stream as i64,
            record.attempt,
            record.response_summary,
        ],
    )?;
    let request_attempt_id = conn.last_insert_rowid();
    if let Some(hash) = body_hash.as_deref() {
        insert_request_body_refs(conn, request_attempt_id, hash)?;
    }
    if let Some(hash) = response_hash.as_deref() {
        insert_request_ref(conn, request_attempt_id, hash, "response")?;
    }
    if let Some(hash) = error_hash.as_deref() {
        insert_request_ref(conn, request_attempt_id, hash, "error")?;
    }
    if record.usage.is_some() || cost_micros.is_some() || record.tokens_per_sec.is_some() {
        let usage = record.usage.as_ref();
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
                record.tokens_per_sec,
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

const LATEST_REQUEST_MODEL_SQL: &str = "SELECT provider, model
     FROM request_attempts
     ORDER BY started_at DESC, id DESC
     LIMIT 1";

const LATEST_CONTEXT_TOKENS_SQL: &str = "SELECT s.context_tokens
     FROM request_attempts a
     JOIN request_stats s ON s.request_attempt_id = a.id
     WHERE s.context_tokens IS NOT NULL
     ORDER BY a.started_at DESC, a.id DESC
     LIMIT 1";

pub(crate) fn request_stats(conn: &Connection) -> Result<RequestAuditStats> {
    let mut stats = conn.query_row(
        "SELECT COUNT(a.id),
                COALESCE(SUM(CASE WHEN a.error_summary IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN a.stream != 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN a.response_hash IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(s.total_cost_micros), 0),
                COALESCE(SUM(CASE
                    WHEN a.completed_at IS NOT NULL THEN MAX(a.completed_at - a.started_at, 0)
                    ELSE 0
                END), 0),
                COALESCE(SUM(s.input_tokens), 0),
                COALESCE(SUM(s.output_tokens), 0),
                COALESCE(SUM(s.cached_input_tokens), 0),
                COALESCE(SUM(s.cache_write_tokens), 0),
                COALESCE(SUM(s.reasoning_tokens), 0),
                MIN(a.started_at),
                MAX(a.started_at),
                MAX(s.context_tokens)
         FROM request_attempts a
         LEFT JOIN request_stats s ON s.request_attempt_id = a.id",
        [],
        |row| {
            let total_cost_micros: i64 = row.get(4)?;
            Ok(RequestAuditStats {
                request_count: nonnegative_u64(row.get(0)?),
                error_count: nonnegative_u64(row.get(1)?),
                streaming_count: nonnegative_u64(row.get(2)?),
                raw_response_count: nonnegative_u64(row.get(3)?),
                total_cost_usd: total_cost_micros as f64 / 1_000_000.0,
                total_elapsed_ms: nonnegative_u64(row.get(5)?),
                total_prompt_tokens: nonnegative_u64(row.get(6)?),
                total_completion_tokens: nonnegative_u64(row.get(7)?),
                total_cache_read_tokens: nonnegative_u64(row.get(8)?),
                total_cache_write_tokens: nonnegative_u64(row.get(9)?),
                total_reasoning_tokens: nonnegative_u64(row.get(10)?),
                first_request_ms: row.get::<_, Option<i64>>(11)?.map(nonnegative_u64),
                latest_timestamp_ms: row.get::<_, Option<i64>>(12)?.map(nonnegative_u64),
                max_context_tokens: row
                    .get::<_, Option<i64>>(13)?
                    .map(|value| nonnegative_u64(value) as u32),
                latest_provider_kind: None,
                latest_model: None,
                latest_context_tokens: None,
            })
        },
    )?;

    if let Some((provider, model)) = conn
        .query_row(LATEST_REQUEST_MODEL_SQL, [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?
    {
        stats.latest_provider_kind = provider;
        stats.latest_model = model;
    }
    stats.latest_context_tokens = conn
        .query_row(LATEST_CONTEXT_TOKENS_SQL, [], |row| row.get::<_, i64>(0))
        .optional()?
        .map(|value| nonnegative_u64(value) as u32);
    Ok(stats)
}

fn nonnegative_u64(value: i64) -> u64 {
    value.max(0) as u64
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
    request_payloads_from_hashes(
        conn,
        body_hash.as_deref(),
        response_hash.as_deref(),
        error_hash.as_deref(),
    )
    .map(Some)
}

pub(crate) fn request_payloads_from_hashes(
    conn: &Connection,
    body_hash: Option<&str>,
    response_hash: Option<&str>,
    error_hash: Option<&str>,
) -> Result<RequestAuditPayloads> {
    Ok(RequestAuditPayloads {
        body: read_request_body(conn, body_hash)?,
        response: read_json_object(conn, response_hash)?,
        error: read_json_object(conn, error_hash)?,
    })
}

fn json_size<T: serde::Serialize>(value: &T) -> Result<u64> {
    let mut writer = CountingWriter { len: 0 };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.len)
}

struct CountingWriter {
    len: u64,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.len = self
            .len
            .checked_add(buf.len() as u64)
            .ok_or_else(|| std::io::Error::other("json size overflow"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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

fn insert_request_ref(conn: &Connection, request_id: i64, hash: &str, role: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO request_object_refs (request_attempt_id, object_hash, role)
         VALUES (?1, ?2, ?3)",
        params![request_id, hash, role],
    )?;
    Ok(())
}

fn insert_request_body_refs(conn: &Connection, request_id: i64, hash: &str) -> Result<()> {
    insert_request_ref(conn, request_id, hash, "body")?;
    let mut seen = std::collections::HashSet::new();
    insert_request_body_manifest_refs(conn, request_id, hash, &mut seen)
}

fn insert_request_body_manifest_refs(
    conn: &Connection,
    request_id: i64,
    hash: &str,
    seen: &mut std::collections::HashSet<String>,
) -> Result<()> {
    if !seen.insert(hash.to_string()) {
        return Ok(());
    }
    let Some(meta) = object::object_meta(conn, hash)? else {
        return Ok(());
    };
    if meta.kind != "request_body_manifest" {
        return Ok(());
    }
    let manifest = read_request_body_manifest(conn, hash)?;
    insert_request_ref(conn, request_id, &manifest.top_hash, "body_top")?;
    for item_hash in &manifest.item_hashes {
        insert_request_ref(conn, request_id, item_hash, "body_item")?;
    }
    if let Some(parent_hash) = manifest.parent_hash.as_deref() {
        insert_request_ref(conn, request_id, parent_hash, "body_parent")?;
        insert_request_body_manifest_refs(conn, request_id, parent_hash, seen)?;
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RequestBodyManifest {
    version: u8,
    input_key: Option<String>,
    top_hash: String,
    parent_hash: Option<String>,
    checkpoint: bool,
    item_hashes: Vec<String>,
}

fn put_request_body_manifest(
    conn: &Connection,
    body: Option<&Value>,
    compression: ObjectCompression,
) -> Result<Option<String>> {
    let Some(body) = body else {
        return Ok(None);
    };
    let (input_key, top, items) = split_request_body(body);
    let top_hash = put_json_object(conn, Some(&top), "request_body_top", compression)?
        .expect("top object is present");
    let item_hashes = items
        .iter()
        .map(|item| {
            put_json_object(conn, Some(item), "request_body_item", compression)
                .map(|hash| hash.expect("item object is present"))
        })
        .collect::<Result<Vec<_>>>()?;
    let previous = previous_body_manifest(conn)?;
    let (parent_hash, checkpoint, manifest_items) = match previous {
        Some(previous)
            if previous.input_key == input_key
                && previous.top_hash == top_hash
                && item_hashes.starts_with(&previous.item_hashes)
                && previous.depth < 32 =>
        {
            (
                Some(previous.hash),
                false,
                item_hashes[previous.item_hashes.len()..].to_vec(),
            )
        }
        _ => (None, true, item_hashes),
    };
    let manifest = RequestBodyManifest {
        version: 1,
        input_key,
        top_hash,
        parent_hash,
        checkpoint,
        item_hashes: manifest_items,
    };
    put_json_object(conn, Some(&manifest), "request_body_manifest", compression)
}

struct ExpandedManifest {
    hash: String,
    input_key: Option<String>,
    top_hash: String,
    item_hashes: Vec<String>,
    depth: usize,
}

fn previous_body_manifest(conn: &Connection) -> Result<Option<ExpandedManifest>> {
    let hash = conn
        .query_row(
            "SELECT a.body_hash FROM request_attempts a
             JOIN objects o ON o.hash = a.body_hash
             WHERE a.body_hash IS NOT NULL AND o.kind = 'request_body_manifest'
             ORDER BY a.id DESC
             LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    hash.map(|hash| expand_body_manifest(conn, &hash))
        .transpose()
}

fn split_request_body(body: &Value) -> (Option<String>, Value, Vec<Value>) {
    let Some(map) = body.as_object() else {
        return (None, body.clone(), Vec::new());
    };
    let input_key = if map.get("input").is_some_and(Value::is_array) {
        Some("input")
    } else if map.get("messages").is_some_and(Value::is_array) {
        Some("messages")
    } else {
        None
    };
    let Some(input_key) = input_key else {
        return (None, body.clone(), Vec::new());
    };
    let mut top = map.clone();
    let items = top
        .remove(input_key)
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    (Some(input_key.to_string()), Value::Object(top), items)
}

fn read_request_body(conn: &Connection, hash: Option<&str>) -> Result<Option<Value>> {
    let Some(hash) = hash else {
        return Ok(None);
    };
    let Some(meta) = object::object_meta(conn, hash)? else {
        return Ok(None);
    };
    if meta.kind != "request_body_manifest" {
        return read_json_object(conn, Some(hash));
    }
    let manifest = expand_body_manifest(conn, hash)?;
    rebuild_request_body(conn, &manifest).map(Some)
}

fn read_request_body_manifest(conn: &Connection, hash: &str) -> Result<RequestBodyManifest> {
    let Some(value) = read_json_object(conn, Some(hash))? else {
        return Err(StoreError::Integrity(format!(
            "request body manifest {hash} missing"
        )));
    };
    let manifest: RequestBodyManifest = serde_json::from_value(value)?;
    if manifest.version != 1 {
        return Err(StoreError::Integrity(format!(
            "unknown request body manifest version {}",
            manifest.version
        )));
    }
    Ok(manifest)
}

fn expand_body_manifest(conn: &Connection, hash: &str) -> Result<ExpandedManifest> {
    let manifest = read_request_body_manifest(conn, hash)?;
    let (mut item_hashes, depth) = if let Some(parent_hash) = manifest.parent_hash.as_deref() {
        let parent = expand_body_manifest(conn, parent_hash)?;
        if parent.input_key != manifest.input_key || parent.top_hash != manifest.top_hash {
            return Err(StoreError::Integrity(
                "request body manifest parent shape mismatch".into(),
            ));
        }
        (parent.item_hashes, parent.depth + 1)
    } else {
        (Vec::new(), 0)
    };
    if manifest.checkpoint {
        item_hashes = manifest.item_hashes;
    } else {
        item_hashes.extend(manifest.item_hashes);
    }
    Ok(ExpandedManifest {
        hash: hash.to_string(),
        input_key: manifest.input_key,
        top_hash: manifest.top_hash,
        item_hashes,
        depth,
    })
}

fn rebuild_request_body(conn: &Connection, manifest: &ExpandedManifest) -> Result<Value> {
    let Some(mut top) = read_json_object(conn, Some(&manifest.top_hash))? else {
        return Err(StoreError::Integrity(format!(
            "request body top {} missing",
            manifest.top_hash
        )));
    };
    let Some(input_key) = manifest.input_key.as_deref() else {
        return Ok(top);
    };
    let items = manifest
        .item_hashes
        .iter()
        .map(|hash| {
            read_json_object(conn, Some(hash))?
                .ok_or_else(|| StoreError::Integrity(format!("request body item {hash} missing")))
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(map) = top.as_object_mut() else {
        return Err(StoreError::Integrity(
            "request body top is not an object".into(),
        ));
    };
    map.insert(input_key.to_string(), Value::Array(items));
    Ok(top)
}

fn read_json_object(conn: &Connection, hash: Option<&str>) -> Result<Option<Value>> {
    let Some(hash) = hash else {
        return Ok(None);
    };
    let Some(bytes) = object::object_bytes_by_hash(conn, hash)? else {
        return Ok(None);
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_stats_latest_queries_use_started_at_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut conn, "test").unwrap();

        for sql in [LATEST_REQUEST_MODEL_SQL, LATEST_CONTEXT_TOKENS_SQL] {
            let details = query_plan_details(&conn, sql);
            assert!(
                details
                    .iter()
                    .any(|detail| detail.contains("request_attempts_started_at_idx")),
                "{sql}\n{details:#?}"
            );
            assert!(
                details
                    .iter()
                    .all(|detail| !detail.contains("USE TEMP B-TREE")),
                "{sql}\n{details:#?}"
            );
        }
    }

    fn query_plan_details(conn: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }
}
