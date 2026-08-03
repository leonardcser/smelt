use protocol::request_log::RequestLogEntry;
use protocol::TokenUsage;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

use crate::compression::ObjectCompression;
use crate::error::{Result, StoreError};
use crate::object::{
    self, checked_i64, MAX_REQUEST_BODY_ITEMS, MAX_REQUEST_MANIFEST_COUNT,
    MAX_REQUEST_MANIFEST_DECODED_BYTES, MAX_REQUEST_MANIFEST_DEPTH,
    MAX_REQUEST_RECONSTRUCTED_BYTES,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RequestObjectRole {
    BodyJson,
    BodyManifest,
    BodyTop,
    BodyItem,
    BodyParent,
    Response,
    Error,
}

impl RequestObjectRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BodyJson => "body_json",
            Self::BodyManifest => "body_manifest",
            Self::BodyTop => "body_top",
            Self::BodyItem => "body_item",
            Self::BodyParent => "body_parent",
            Self::Response => "response",
            Self::Error => "error",
        }
    }

    pub(crate) fn from_str(role: &str) -> Result<Self> {
        match role {
            "body_json" => Ok(Self::BodyJson),
            "body_manifest" => Ok(Self::BodyManifest),
            "body_top" => Ok(Self::BodyTop),
            "body_item" => Ok(Self::BodyItem),
            "body_parent" => Ok(Self::BodyParent),
            "response" => Ok(Self::Response),
            "error" => Ok(Self::Error),
            _ => Err(StoreError::Integrity(format!(
                "unknown request object role {role:?}"
            ))),
        }
    }
}

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
    let body = match payload_mode {
        RequestAuditPayloadMode::Summary { .. } => None,
        RequestAuditPayloadMode::Full => put_request_body(conn, record.body.as_ref(), compression)?,
    };
    let response_hash = match payload_mode {
        RequestAuditPayloadMode::Summary { .. } => None,
        RequestAuditPayloadMode::Full => {
            put_json_object(conn, record.response.as_ref(), compression)?
        }
    };
    let error_hash = match payload_mode {
        RequestAuditPayloadMode::Summary { .. } => None,
        RequestAuditPayloadMode::Full => put_json_object(conn, record.error.as_ref(), compression)?,
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
            history_len, kind, error_summary, background, raw_body_size, api_base, url,
            http_status, prompt_cache_key, stream, attempt, response_summary
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                   ?15, ?16, ?17, ?18, ?19)",
        params![
            record.request_id,
            record.turn_id,
            record.ask_id,
            record.started_at,
            record.completed_at,
            record.provider,
            record.model,
            record.history_len,
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
    if let Some(body) = body.as_ref() {
        match body {
            StoredRequestBody::Json { hash } => {
                insert_request_ref(conn, request_attempt_id, hash, RequestObjectRole::BodyJson)?;
            }
            StoredRequestBody::Manifest { hash, refs } => {
                insert_request_ref(
                    conn,
                    request_attempt_id,
                    hash,
                    RequestObjectRole::BodyManifest,
                )?;
                install_manifest_refs(conn, request_attempt_id, refs)?;
            }
        }
    }
    if let Some(hash) = response_hash.as_deref() {
        insert_request_ref(conn, request_attempt_id, hash, RequestObjectRole::Response)?;
    }
    if let Some(hash) = error_hash.as_deref() {
        insert_request_ref(conn, request_attempt_id, hash, RequestObjectRole::Error)?;
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

pub(crate) fn lineage_request_attempts(
    conn: &Connection,
    lineage_id: &str,
    session_id: &str,
    query: &RequestAuditQuery,
) -> Result<Vec<RequestAuditSummary>> {
    request_attempts_for_branch(conn, query, Some((lineage_id, session_id)))
}

fn request_attempts_for_branch(
    conn: &Connection,
    query: &RequestAuditQuery,
    branch: Option<(&str, &str)>,
) -> Result<Vec<RequestAuditSummary>> {
    let mut sql = String::from(
        "SELECT a.id, a.request_id, a.kind, a.turn_id, a.ask_id, a.started_at, a.completed_at,
                a.provider, a.model, a.api_base, a.url, a.http_status, a.history_len, a.attempt,
                a.stream, a.prompt_cache_key, a.background, a.raw_body_size, body.object_hash,
                response.object_hash, a.response_summary, error.object_hash, a.error_summary,
                s.stats_json, s.total_cost_micros, s.tokens_per_sec
         FROM request_attempts a",
    );
    if branch.is_some() {
        sql.push_str(
            " JOIN lineage_request_attempts lineage_request
                ON lineage_request.request_attempt_id = a.id",
        );
    }
    sql.push_str(
        " LEFT JOIN request_stats s ON s.request_attempt_id = a.id
          LEFT JOIN request_object_refs body
            ON body.request_attempt_id = a.id AND body.role IN ('body_json', 'body_manifest')
          LEFT JOIN request_object_refs response
            ON response.request_attempt_id = a.id AND response.role = 'response'
          LEFT JOIN request_object_refs error
            ON error.request_attempt_id = a.id AND error.role = 'error'",
    );
    let mut clauses: Vec<&str> = Vec::new();
    let mut values: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some((lineage_id, session_id)) = branch {
        clauses.push("lineage_request.lineage_id = ?");
        values.push(Box::new(lineage_id.to_owned()));
        clauses.push("lineage_request.session_id = ?");
        values.push(Box::new(session_id.to_owned()));
    }
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

fn request_stats_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RequestAuditStats> {
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
}

pub(crate) fn lineage_request_stats(
    conn: &Connection,
    lineage_id: &str,
    session_id: &str,
) -> Result<RequestAuditStats> {
    let branch = params![lineage_id, session_id];
    let mut stats = conn.query_row(
        "SELECT COUNT(a.id),
                COALESCE(SUM(CASE WHEN a.error_summary IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN a.stream != 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN EXISTS (
                    SELECT 1 FROM request_object_refs response
                    WHERE response.request_attempt_id = a.id AND response.role = 'response'
                ) THEN 1 ELSE 0 END), 0),
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
         JOIN lineage_request_attempts branch
           ON branch.request_attempt_id = a.id
         LEFT JOIN request_stats s ON s.request_attempt_id = a.id
         WHERE branch.lineage_id = ?1 AND branch.session_id = ?2",
        branch,
        request_stats_from_row,
    )?;

    if let Some((provider, model)) = conn
        .query_row(
            "SELECT a.provider, a.model
             FROM request_attempts a
             JOIN lineage_request_attempts branch
               ON branch.request_attempt_id = a.id
             WHERE branch.lineage_id = ?1 AND branch.session_id = ?2
             ORDER BY a.started_at DESC, a.id DESC
             LIMIT 1",
            params![lineage_id, session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
    {
        stats.latest_provider_kind = provider;
        stats.latest_model = model;
    }
    stats.latest_context_tokens = conn
        .query_row(
            "SELECT s.context_tokens
             FROM request_attempts a
             JOIN lineage_request_attempts branch
               ON branch.request_attempt_id = a.id
             JOIN request_stats s ON s.request_attempt_id = a.id
             WHERE branch.lineage_id = ?1 AND branch.session_id = ?2
               AND s.context_tokens IS NOT NULL
             ORDER BY a.started_at DESC, a.id DESC
             LIMIT 1",
            params![lineage_id, session_id],
            |row| row.get::<_, i64>(0),
        )
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
    let exists = conn
        .query_row(
            "SELECT 1 FROM request_attempts WHERE id = ?1",
            [request_attempt_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(None);
    }
    let refs = request_payload_refs(conn, request_attempt_id)?;
    let body = match refs.body {
        Some((RequestObjectRole::BodyJson, hash)) => {
            validate_body_refs(
                conn,
                request_attempt_id,
                &BTreeSet::from([(RequestObjectRole::BodyJson, hash.clone())]),
            )?;
            Some(read_json_object_required(conn, &hash)?)
        }
        Some((RequestObjectRole::BodyManifest, hash)) => {
            let manifest = walk_body_manifest(conn, Some(request_attempt_id), &hash)?;
            Some(rebuild_request_body(&manifest)?)
        }
        Some((role, _)) => {
            return Err(StoreError::Integrity(format!(
                "request {request_attempt_id} has invalid body root role {:?}",
                role.as_str()
            )))
        }
        None => {
            validate_body_refs(conn, request_attempt_id, &BTreeSet::new())?;
            None
        }
    };
    Ok(Some(RequestAuditPayloads {
        body,
        response: refs
            .response
            .as_deref()
            .map(|hash| read_json_object_required(conn, hash))
            .transpose()?,
        error: refs
            .error
            .as_deref()
            .map(|hash| read_json_object_required(conn, hash))
            .transpose()?,
    }))
}

struct RequestPayloadRefs {
    body: Option<(RequestObjectRole, String)>,
    response: Option<String>,
    error: Option<String>,
}

fn request_payload_refs(conn: &Connection, request_attempt_id: i64) -> Result<RequestPayloadRefs> {
    let mut stmt = conn.prepare(
        "SELECT object_hash, role
         FROM request_object_refs
         WHERE request_attempt_id = ?1
           AND role IN ('body_json', 'body_manifest', 'response', 'error')
         ORDER BY role, object_hash",
    )?;
    let rows = stmt.query_map([request_attempt_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut refs = RequestPayloadRefs {
        body: None,
        response: None,
        error: None,
    };
    for row in rows {
        let (hash, role) = row?;
        let role = RequestObjectRole::from_str(&role)?;
        let target = match role {
            RequestObjectRole::BodyJson | RequestObjectRole::BodyManifest => {
                if refs.body.replace((role, hash)).is_some() {
                    return Err(StoreError::Integrity(format!(
                        "request {request_attempt_id} has multiple body roots"
                    )));
                }
                continue;
            }
            RequestObjectRole::Response => &mut refs.response,
            RequestObjectRole::Error => &mut refs.error,
            _ => unreachable!("query selects only payload root roles"),
        };
        if target.replace(hash).is_some() {
            return Err(StoreError::Integrity(format!(
                "request {request_attempt_id} has multiple {} references",
                role.as_str()
            )));
        }
    }
    Ok(refs)
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
    compression: ObjectCompression,
) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let bytes = serde_json::to_vec(value)?;
    Ok(Some(
        object::put_object(conn, &bytes, compression)?
            .hash()
            .to_string(),
    ))
}

fn insert_request_ref(
    conn: &Connection,
    request_id: i64,
    hash: &str,
    role: RequestObjectRole,
) -> Result<()> {
    conn.execute(
        "INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
         VALUES (?1, ?2, ?3)",
        params![request_id, hash, role.as_str()],
    )?;
    Ok(())
}

fn install_manifest_refs(
    conn: &Connection,
    request_id: i64,
    refs: &ManifestReferences,
) -> Result<()> {
    for hash in &refs.top_hashes {
        insert_request_ref(conn, request_id, hash, RequestObjectRole::BodyTop)?;
    }
    for hash in &refs.item_hashes {
        insert_request_ref(conn, request_id, hash, RequestObjectRole::BodyItem)?;
    }
    for hash in &refs.parent_hashes {
        insert_request_ref(conn, request_id, hash, RequestObjectRole::BodyParent)?;
    }
    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RequestBodyManifest {
    version: u8,
    input_key: Option<String>,
    top_hash: String,
    parent_hash: Option<String>,
    checkpoint: bool,
    item_hashes: Vec<String>,
}

enum StoredRequestBody {
    Json {
        hash: String,
    },
    Manifest {
        hash: String,
        refs: ManifestReferences,
    },
}

fn put_request_body(
    conn: &Connection,
    body: Option<&Value>,
    compression: ObjectCompression,
) -> Result<Option<StoredRequestBody>> {
    let Some(body) = body else {
        return Ok(None);
    };
    let body_size = json_size(body)?;
    enforce_limit(
        body_size,
        MAX_REQUEST_RECONSTRUCTED_BYTES,
        "request body reconstructed bytes",
    )?;
    let Some((input_key, top, items)) = split_request_body(body) else {
        let hash =
            put_json_object(conn, Some(body), compression)?.expect("request body is present");
        return Ok(Some(StoredRequestBody::Json { hash }));
    };
    if items.len() > MAX_REQUEST_BODY_ITEMS {
        return Err(manifest_limit_error(
            "request body item count",
            items.len(),
            MAX_REQUEST_BODY_ITEMS,
        ));
    }
    let top_hash = put_json_object(conn, Some(&top), compression)?.expect("top object is present");
    let item_hashes = items
        .iter()
        .map(|item| {
            put_json_object(conn, Some(item), compression)
                .map(|hash| hash.expect("item object is present"))
        })
        .collect::<Result<Vec<_>>>()?;
    let previous = previous_body_manifest(conn)?;
    let (parent_hash, checkpoint, manifest_items) = match previous {
        Some(previous)
            if previous.input_key.as_deref() == Some(input_key.as_str())
                && previous.top_hash == top_hash
                && item_hashes.starts_with(&previous.item_hashes)
                && previous.manifest_count < MAX_REQUEST_MANIFEST_DEPTH
                && previous.manifest_count < MAX_REQUEST_MANIFEST_COUNT =>
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
        input_key: Some(input_key),
        top_hash,
        parent_hash,
        checkpoint,
        item_hashes: manifest_items,
    };
    let hash = put_json_object(conn, Some(&manifest), compression)?
        .expect("request body manifest is present");
    let refs = walk_body_manifest(conn, None, &hash)?.refs;
    Ok(Some(StoredRequestBody::Manifest { hash, refs }))
}

#[derive(Debug, Default)]
struct ManifestReferences {
    top_hashes: BTreeSet<String>,
    item_hashes: BTreeSet<String>,
    parent_hashes: BTreeSet<String>,
}

#[derive(Debug)]
struct ExpandedManifest {
    hash: String,
    input_key: Option<String>,
    top_hash: String,
    item_hashes: Vec<String>,
    top: Value,
    items: Vec<Value>,
    manifest_count: usize,
    refs: ManifestReferences,
}

fn previous_body_manifest(conn: &Connection) -> Result<Option<ExpandedManifest>> {
    let root = conn
        .query_row(
            "SELECT request_attempt_id, object_hash
             FROM request_object_refs
             WHERE role = 'body_manifest'
             ORDER BY request_attempt_id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    root.map(|(request_id, hash)| walk_body_manifest(conn, Some(request_id), &hash))
        .transpose()
}

fn split_request_body(body: &Value) -> Option<(String, Value, Vec<Value>)> {
    let map = body.as_object()?;
    let input_key = if map.get("input").is_some_and(Value::is_array) {
        "input"
    } else if map.get("messages").is_some_and(Value::is_array) {
        "messages"
    } else {
        return None;
    };
    let mut top = map.clone();
    let items = top
        .remove(input_key)
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    Some((input_key.to_string(), Value::Object(top), items))
}

fn walk_body_manifest(
    conn: &Connection,
    request_attempt_id: Option<i64>,
    root_hash: &str,
) -> Result<ExpandedManifest> {
    validate_object_hash(root_hash)?;

    let (mut chain, mut decoded_bytes) = collect_manifest_chain(root_hash, |hash| {
        let bytes = read_object_bytes_required(conn, hash)?;
        let decoded_size = bytes.len();
        let manifest = serde_json::from_slice(&bytes).map_err(|err| {
            StoreError::Integrity(format!(
                "request body manifest {hash} is invalid JSON: {err}"
            ))
        })?;
        Ok((manifest, decoded_size))
    })?;
    let mut refs = ManifestReferences::default();
    for (_, manifest) in &chain {
        refs.top_hashes.insert(manifest.top_hash.clone());
        refs.item_hashes
            .extend(manifest.item_hashes.iter().cloned());
        if let Some(parent_hash) = manifest.parent_hash.as_deref() {
            refs.parent_hashes.insert(parent_hash.to_string());
        }
    }
    if let Some(request_id) = request_attempt_id {
        validate_manifest_refs(conn, request_id, root_hash, &refs)?;
    }
    chain.reverse();
    let (_, first) = chain
        .first()
        .expect("manifest traversal always contains the root");
    let input_key = first.input_key.clone();
    let top_hash = first.top_hash.clone();
    let mut item_hashes = Vec::new();
    for (hash, manifest) in &chain {
        if manifest.input_key != input_key || manifest.top_hash != top_hash {
            return Err(StoreError::Integrity(format!(
                "request body manifest {hash} parent shape mismatch"
            )));
        }
        if manifest.checkpoint {
            item_hashes.clear();
        }
        let new_len = item_hashes
            .len()
            .checked_add(manifest.item_hashes.len())
            .ok_or_else(|| StoreError::Integrity("request body item count overflow".into()))?;
        if new_len > MAX_REQUEST_BODY_ITEMS {
            return Err(manifest_limit_error(
                "request body item count",
                new_len,
                MAX_REQUEST_BODY_ITEMS,
            ));
        }
        item_hashes.extend(manifest.item_hashes.iter().cloned());
    }

    let top_bytes = read_object_bytes_required(conn, &top_hash)?;
    add_decoded_bytes(&mut decoded_bytes, top_bytes.len())?;
    let top: Value = serde_json::from_slice(&top_bytes).map_err(|err| {
        StoreError::Integrity(format!(
            "request body top {top_hash} is invalid JSON: {err}"
        ))
    })?;
    let mut items = Vec::with_capacity(item_hashes.len());
    for item_hash in &item_hashes {
        let bytes = read_object_bytes_required(conn, item_hash)?;
        add_decoded_bytes(&mut decoded_bytes, bytes.len())?;
        items.push(serde_json::from_slice(&bytes).map_err(|err| {
            StoreError::Integrity(format!(
                "request body item {item_hash} is invalid JSON: {err}"
            ))
        })?);
    }

    let expanded = ExpandedManifest {
        hash: root_hash.to_string(),
        input_key,
        top_hash,
        item_hashes,
        top,
        items,
        manifest_count: chain.len(),
        refs,
    };
    let rebuilt = rebuild_request_body(&expanded)?;
    enforce_limit(
        json_size(&rebuilt)?,
        MAX_REQUEST_RECONSTRUCTED_BYTES,
        "request body reconstructed bytes",
    )?;
    Ok(expanded)
}

fn collect_manifest_chain<F>(
    root_hash: &str,
    mut load: F,
) -> Result<(Vec<(String, RequestBodyManifest)>, u64)>
where
    F: FnMut(&str) -> Result<(RequestBodyManifest, usize)>,
{
    validate_object_hash(root_hash)?;
    let mut seen = HashSet::new();
    let mut chain = Vec::new();
    let mut current_hash = root_hash.to_string();
    let mut decoded_bytes = 0u64;
    loop {
        if chain.len() >= MAX_REQUEST_MANIFEST_DEPTH {
            return Err(manifest_limit_error(
                "request body manifest depth",
                chain.len() + 1,
                MAX_REQUEST_MANIFEST_DEPTH,
            ));
        }
        if chain.len() >= MAX_REQUEST_MANIFEST_COUNT {
            return Err(manifest_limit_error(
                "request body manifest count",
                chain.len() + 1,
                MAX_REQUEST_MANIFEST_COUNT,
            ));
        }
        if !seen.insert(current_hash.clone()) {
            return Err(StoreError::Integrity(format!(
                "request body manifest cycle at {current_hash}"
            )));
        }
        let (manifest, decoded_size) = load(&current_hash)?;
        add_decoded_bytes(&mut decoded_bytes, decoded_size)?;
        validate_manifest(&current_hash, &manifest)?;
        let parent_hash = manifest.parent_hash.clone();
        chain.push((current_hash, manifest));
        let Some(parent_hash) = parent_hash else {
            break;
        };
        current_hash = parent_hash;
    }
    Ok((chain, decoded_bytes))
}

fn validate_manifest(hash: &str, manifest: &RequestBodyManifest) -> Result<()> {
    if manifest.version != 1 {
        return Err(StoreError::Integrity(format!(
            "request body manifest {hash} has unknown version {}",
            manifest.version
        )));
    }
    if manifest.parent_hash.is_some() == manifest.checkpoint {
        return Err(StoreError::Integrity(format!(
            "request body manifest {hash} has invalid checkpoint/parent state"
        )));
    }
    if manifest.input_key.is_none() && !manifest.item_hashes.is_empty() {
        return Err(StoreError::Integrity(format!(
            "request body manifest {hash} has items without an input key"
        )));
    }
    if let Some(input_key) = manifest.input_key.as_deref() {
        if !matches!(input_key, "input" | "messages") {
            return Err(StoreError::Integrity(format!(
                "request body manifest {hash} has unsupported input key {input_key:?}"
            )));
        }
    }
    validate_object_hash(&manifest.top_hash)?;
    if let Some(parent_hash) = manifest.parent_hash.as_deref() {
        validate_object_hash(parent_hash)?;
    }
    for item_hash in &manifest.item_hashes {
        validate_object_hash(item_hash)?;
    }
    Ok(())
}

fn rebuild_request_body(manifest: &ExpandedManifest) -> Result<Value> {
    let mut top = manifest.top.clone();
    let Some(input_key) = manifest.input_key.as_deref() else {
        if !manifest.items.is_empty() {
            return Err(StoreError::Integrity(
                "request body manifest without input key has items".into(),
            ));
        }
        return Ok(top);
    };
    let Some(map) = top.as_object_mut() else {
        return Err(StoreError::Integrity(
            "request body top is not an object".into(),
        ));
    };
    map.insert(input_key.to_string(), Value::Array(manifest.items.clone()));
    Ok(top)
}

fn validate_manifest_refs(
    conn: &Connection,
    request_id: i64,
    root_hash: &str,
    refs: &ManifestReferences,
) -> Result<()> {
    let mut expected = BTreeSet::from([(RequestObjectRole::BodyManifest, root_hash.to_string())]);
    expected.extend(
        refs.top_hashes
            .iter()
            .cloned()
            .map(|hash| (RequestObjectRole::BodyTop, hash)),
    );
    expected.extend(
        refs.item_hashes
            .iter()
            .cloned()
            .map(|hash| (RequestObjectRole::BodyItem, hash)),
    );
    expected.extend(
        refs.parent_hashes
            .iter()
            .cloned()
            .map(|hash| (RequestObjectRole::BodyParent, hash)),
    );
    validate_body_refs(conn, request_id, &expected)
}

fn validate_body_refs(
    conn: &Connection,
    request_id: i64,
    expected: &BTreeSet<(RequestObjectRole, String)>,
) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT role, object_hash
         FROM request_object_refs
         WHERE request_attempt_id = ?1
           AND role IN ('body_json', 'body_manifest', 'body_top', 'body_item', 'body_parent')
         ORDER BY role, object_hash",
    )?;
    let rows = stmt.query_map([request_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut actual = BTreeSet::new();
    for row in rows {
        let (role, hash) = row?;
        actual.insert((RequestObjectRole::from_str(&role)?, hash));
    }
    if &actual == expected {
        return Ok(());
    }

    let missing = expected
        .difference(&actual)
        .map(|(role, hash)| format!("{}:{hash}", role.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let unexpected = actual
        .difference(expected)
        .map(|(role, hash)| format!("{}:{hash}", role.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    Err(StoreError::Integrity(format!(
        "request {request_id} body references differ from its payload: missing [{missing}]; unexpected [{unexpected}]"
    )))
}

fn validate_object_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StoreError::Integrity(format!(
            "invalid SHA-256 object hash {hash:?}"
        )));
    }
    Ok(())
}

fn read_object_bytes_required(conn: &Connection, hash: &str) -> Result<Vec<u8>> {
    validate_object_hash(hash)?;
    object::object_bytes_by_hash(conn, hash)?.ok_or_else(|| StoreError::MissingObject {
        reference: hash.to_string(),
    })
}

fn read_json_object_required(conn: &Connection, hash: &str) -> Result<Value> {
    let bytes = read_object_bytes_required(conn, hash)?;
    serde_json::from_slice(&bytes).map_err(Into::into)
}

fn add_decoded_bytes(total: &mut u64, added: usize) -> Result<()> {
    *total = total
        .checked_add(added as u64)
        .ok_or_else(|| StoreError::Integrity("request manifest decoded bytes overflow".into()))?;
    enforce_limit(
        *total,
        MAX_REQUEST_MANIFEST_DECODED_BYTES,
        "request manifest decoded bytes",
    )
}

fn enforce_limit(actual: u64, limit: u64, name: &str) -> Result<()> {
    if actual > limit {
        return Err(StoreError::Integrity(format!(
            "{name} {actual} exceeds limit {limit}"
        )));
    }
    Ok(())
}

fn manifest_limit_error(name: &str, actual: usize, limit: usize) -> StoreError {
    StoreError::Integrity(format!("{name} {actual} exceeds limit {limit}"))
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
    smelt_buffer::text::slice(text, 0..max_bytes).to_string()
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

    fn manifest(parent_hash: Option<String>, checkpoint: bool) -> RequestBodyManifest {
        RequestBodyManifest {
            version: 1,
            input_key: Some("messages".into()),
            top_hash: "f".repeat(64),
            parent_hash,
            checkpoint,
            item_hashes: Vec::new(),
        }
    }

    #[test]
    fn manifest_chain_accepts_exact_depth_limit_and_rejects_limit_plus_one() {
        fn chain(
            count: usize,
        ) -> (
            String,
            std::collections::HashMap<String, RequestBodyManifest>,
        ) {
            let mut manifests = std::collections::HashMap::new();
            let mut parent = None;
            for index in 0..count {
                let hash = format!("{index:064x}");
                manifests.insert(hash.clone(), manifest(parent, index == 0));
                parent = Some(hash);
            }
            (parent.expect("chain is nonempty"), manifests)
        }

        let (root, manifests) = chain(MAX_REQUEST_MANIFEST_DEPTH);
        let (walked, _) =
            collect_manifest_chain(&root, |hash| Ok((manifests.get(hash).unwrap().clone(), 1)))
                .unwrap();
        assert_eq!(walked.len(), MAX_REQUEST_MANIFEST_DEPTH);

        let (root, manifests) = chain(MAX_REQUEST_MANIFEST_DEPTH + 1);
        let err =
            collect_manifest_chain(&root, |hash| Ok((manifests.get(hash).unwrap().clone(), 1)))
                .unwrap_err();
        assert!(err.to_string().contains("manifest depth"), "{err}");
    }

    #[test]
    fn manifest_chain_rejects_cycles_before_revisiting_an_object() {
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let manifests = std::collections::HashMap::from([
            (first.clone(), manifest(Some(second.clone()), false)),
            (second, manifest(Some(first.clone()), false)),
        ]);

        let err =
            collect_manifest_chain(&first, |hash| Ok((manifests.get(hash).unwrap().clone(), 1)))
                .unwrap_err();
        assert!(err.to_string().contains("manifest cycle"), "{err}");
    }

    #[test]
    fn manifest_byte_limit_accepts_exact_boundary_and_rejects_one_over() {
        let root = "a".repeat(64);
        let parent = "b".repeat(64);
        let manifests = std::collections::HashMap::from([
            (root.clone(), manifest(Some(parent.clone()), false)),
            (parent.clone(), manifest(None, true)),
        ]);
        let exact_sizes = std::collections::HashMap::from([
            (
                root.clone(),
                usize::try_from(MAX_REQUEST_MANIFEST_DECODED_BYTES - 1).unwrap(),
            ),
            (parent.clone(), 1),
        ]);
        let (_, decoded_bytes) = collect_manifest_chain(&root, |hash| {
            Ok((manifests.get(hash).unwrap().clone(), exact_sizes[hash]))
        })
        .unwrap();
        assert_eq!(decoded_bytes, MAX_REQUEST_MANIFEST_DECODED_BYTES);

        let err = collect_manifest_chain(&root, |hash| {
            let size = if hash == root {
                usize::try_from(MAX_REQUEST_MANIFEST_DECODED_BYTES).unwrap()
            } else {
                1
            };
            Ok((manifests.get(hash).unwrap().clone(), size))
        })
        .unwrap_err();
        assert!(err.to_string().contains("exceeds limit"), "{err}");
    }

    #[test]
    fn manifest_walk_rejects_missing_component_objects() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let top_hash = object::put_object(&conn, b"{}", ObjectCompression::none())
            .unwrap()
            .hash()
            .to_string();
        let missing = "d".repeat(64);
        let manifest = RequestBodyManifest {
            version: 1,
            input_key: Some("messages".into()),
            top_hash,
            parent_hash: None,
            checkpoint: true,
            item_hashes: vec![missing.clone()],
        };
        let root = put_json_object(&conn, Some(&manifest), ObjectCompression::none())
            .unwrap()
            .unwrap();

        let err = walk_body_manifest(&conn, None, &root).unwrap_err();
        assert!(matches!(
            err,
            StoreError::MissingObject { reference } if reference == missing
        ));
    }

    #[test]
    fn manifest_walk_rejects_reference_role_mismatch() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let body = serde_json::json!({"messages": [{"role": "user", "content": "hi"}]});
        let stored = put_request_body(&conn, Some(&body), ObjectCompression::none())
            .unwrap()
            .unwrap();
        let StoredRequestBody::Manifest { hash, refs } = stored else {
            panic!("array request body was not stored as a manifest");
        };
        conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
            .unwrap();
        let request_id = conn.last_insert_rowid();
        insert_request_ref(&conn, request_id, &hash, RequestObjectRole::BodyManifest).unwrap();
        for hash in &refs.top_hashes {
            insert_request_ref(&conn, request_id, hash, RequestObjectRole::BodyTop).unwrap();
        }
        for hash in &refs.item_hashes {
            insert_request_ref(&conn, request_id, hash, RequestObjectRole::BodyTop).unwrap();
        }

        let err = walk_body_manifest(&conn, Some(request_id), &hash).unwrap_err();
        assert!(err.to_string().contains("missing [body_item:"), "{err}");
    }

    #[test]
    fn request_payloads_rejects_unexpected_body_references() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let body_hash = object::put_object(&conn, b"{}", ObjectCompression::none())
            .unwrap()
            .hash()
            .to_string();
        let extra_hash = object::put_object(&conn, b"[]", ObjectCompression::none())
            .unwrap()
            .hash()
            .to_string();
        conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
            .unwrap();
        let request_id = conn.last_insert_rowid();
        insert_request_ref(&conn, request_id, &body_hash, RequestObjectRole::BodyJson).unwrap();
        insert_request_ref(&conn, request_id, &extra_hash, RequestObjectRole::BodyItem).unwrap();

        let err = request_payloads(&conn, request_id).unwrap_err();
        assert!(err.to_string().contains("unexpected [body_item:"), "{err}");
    }

    #[test]
    fn identical_bytes_can_serve_different_request_roles() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let hash = object::put_object(&conn, b"{}", ObjectCompression::none())
            .unwrap()
            .hash()
            .to_string();
        conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
            .unwrap();
        let request_id = conn.last_insert_rowid();
        insert_request_ref(&conn, request_id, &hash, RequestObjectRole::BodyJson).unwrap();
        insert_request_ref(&conn, request_id, &hash, RequestObjectRole::Response).unwrap();

        let payloads = request_payloads(&conn, request_id).unwrap().unwrap();
        assert_eq!(payloads.body, Some(serde_json::json!({})));
        assert_eq!(payloads.response, Some(serde_json::json!({})));
        assert!(object::object_meta(&conn, &hash).unwrap().is_some());
    }
}
