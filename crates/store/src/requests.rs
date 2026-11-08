//! Provider request audit. Summary mode keeps metadata and stop diagnostics;
//! full mode also keeps compressed request and response bodies.

use protocol::request_log::{RequestError, RequestLogEntry, RequestResponse};
use protocol::TokenUsage;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde_json::Value;

use crate::db::{signed, unsigned};
use crate::document::{self, Owner, RowKey};
use crate::error::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayloadKind {
    Full,
    Diagnostics,
}

impl ResponsePayloadKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Diagnostics => "diagnostics",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "diagnostics" => Some(Self::Diagnostics),
            _ => None,
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
    pub has_body: bool,
    pub has_response: bool,
    pub response_payload_kind: Option<ResponsePayloadKind>,
    pub response_summary: Option<String>,
    pub has_error: bool,
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
    pub response_payload_kind: Option<ResponsePayloadKind>,
    pub response: Option<Value>,
    pub error: Option<Value>,
}

/// Stop reasons and error envelope only, never provider bodies.
fn stop_diagnostics(payload: Option<&Value>, error: bool) -> Option<Value> {
    let payload = payload?.as_object()?;
    let mut diagnostics = serde_json::Map::new();
    for key in ["finish_reason", "stop_reason", "system_fingerprint"] {
        if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
            diagnostics.insert(key.into(), value.clone());
        }
    }
    if diagnostics.is_empty() {
        return None;
    }
    if error {
        for key in ["kind", "status", "message"] {
            if let Some(value) = payload.get(key) {
                diagnostics.insert(key.into(), value.clone());
            }
        }
    }
    Some(Value::Object(diagnostics))
}

fn error_summary(error: &RequestError) -> String {
    if error.message.is_empty() {
        error.kind.clone()
    } else {
        error.message.clone()
    }
}

fn response_summary(response: &RequestResponse) -> Option<String> {
    response
        .content
        .as_deref()
        .or(response.reasoning.as_deref())
        .map(|text| smelt_text::grapheme_prefix(text, 512).to_string())
}

fn cost_micros(cost_usd: Option<f64>) -> Option<i64> {
    cost_usd.filter(|cost| cost.is_finite()).map(|cost| {
        (cost * 1_000_000.0)
            .round()
            .clamp(i64::MIN as f64, i64::MAX as f64) as i64
    })
}

pub(crate) fn append(
    conn: &Connection,
    sid: i64,
    entry: &RequestLogEntry,
    mode: RequestAuditPayloadMode,
) -> Result<i64> {
    let full = mode == RequestAuditPayloadMode::Full;
    let response = entry
        .response
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    let error = entry.error.as_ref().map(serde_json::to_value).transpose()?;
    let raw_body_size = match mode {
        RequestAuditPayloadMode::Summary {
            raw_body_size: Some(size),
        } => size,
        _ => serde_json::to_vec(&entry.body)?.len() as u64,
    };
    let (response, kind) = if full {
        (response, ResponsePayloadKind::Full)
    } else {
        let diagnostics = stop_diagnostics(response.as_ref(), false);
        (diagnostics, ResponsePayloadKind::Diagnostics)
    };
    let response_kind = response.as_ref().map(|_| kind);
    let error = if full {
        error
    } else {
        stop_diagnostics(error.as_ref(), true)
    };
    // Payloads are documents of this row, so its id comes first. Ids are the
    // next rowid, as SQLite would choose.
    let id: i64 = conn.query_row("SELECT coalesce(max(id), 0) + 1 FROM requests", [], |row| {
        row.get(0)
    })?;
    let row = RowKey {
        sid,
        owner: Owner::Request,
        key: id,
    };
    let store = |value: &Value| document::store_json(conn, row, value);
    let body = full
        .then(|| document::store_json(conn, row, &entry.body))
        .transpose()?;
    let response = response.as_ref().map(store).transpose()?;
    let error = error.as_ref().map(store).transpose()?;
    let usage = entry.usage.as_ref();
    let completed_at = entry
        .elapsed_ms
        .map(|elapsed| signed(entry.timestamp_ms.saturating_add(elapsed)));
    conn.prepare_cached(
        "INSERT INTO requests (id, sid, request_id, kind, turn_id, ask_id, started_at, completed_at,
             provider, model, api_base, url, http_status, history_len, attempt, stream,
             prompt_cache_key, background, raw_body_size, response_kind, response_summary,
             error_summary, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
             reasoning_tokens, context_tokens, cost_micros, tokens_per_sec, usage, body,
             response, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33,
             ?34)",
    )?
    .execute(params![
        id,
        sid,
        entry.request_id.to_string(),
        entry.kind,
        entry.turn_id.map(|id| id.to_string()),
        entry.ask_id.map(|id| id.to_string()),
        signed(entry.timestamp_ms),
        completed_at,
        entry.provider_kind,
        entry.model,
        entry.api_base,
        entry.url,
        entry.http_status.map(i64::from),
        entry.history_len.map(|len| len as i64),
        i64::from(entry.attempt),
        entry.stream,
        entry.prompt_cache_key,
        entry.background,
        signed(raw_body_size),
        response_kind.map(ResponsePayloadKind::as_str),
        entry.response.as_ref().and_then(response_summary),
        entry.error.as_ref().map(error_summary),
        usage.and_then(|usage| usage.prompt_tokens).map(i64::from),
        usage
            .and_then(|usage| usage.completion_tokens)
            .map(i64::from),
        usage
            .and_then(|usage| usage.cache_read_tokens)
            .map(i64::from),
        usage
            .and_then(|usage| usage.cache_write_tokens)
            .map(i64::from),
        usage
            .and_then(|usage| usage.reasoning_tokens)
            .map(i64::from),
        usage.and_then(|usage| usage.context_tokens).map(i64::from),
        cost_micros(entry.cost_usd),
        entry.tokens_per_sec,
        usage.map(serde_json::to_string).transpose()?,
        body,
        response,
        error,
    ])?;
    Ok(id)
}

pub(crate) fn query(
    conn: &Connection,
    sid: i64,
    query: &RequestAuditQuery,
) -> Result<Vec<RequestAuditSummary>> {
    let mut clauses = vec!["sid = ?"];
    let mut values: Vec<Box<dyn ToSql>> = vec![Box::new(sid)];
    let mut push = |clause: &'static str, value: Option<Box<dyn ToSql>>| {
        if let Some(value) = value {
            clauses.push(clause);
            values.push(value);
        }
    };
    let int = |value: Option<u64>| value.map(|value| Box::new(signed(value)) as Box<dyn ToSql>);
    let text =
        |value: &Option<String>| value.clone().map(|value| Box::new(value) as Box<dyn ToSql>);
    push("started_at >= ?", int(query.started_at_from_ms));
    push("started_at <= ?", int(query.started_at_to_ms));
    push("request_id = ?", text(&query.request_id));
    push("turn_id = ?", text(&query.turn_id));
    push("ask_id = ?", text(&query.ask_id));
    push("provider = ?", text(&query.provider));
    push("model = ?", text(&query.model));
    push("raw_body_size >= ?", int(query.min_body_size));
    push("input_tokens >= ?", int(query.min_input_tokens));
    push("output_tokens >= ?", int(query.min_output_tokens));
    push(
        "cost_micros >= ?",
        query
            .min_cost_micros
            .map(|value| Box::new(value) as Box<dyn ToSql>),
    );
    if query.errors_only {
        clauses.push("error_summary IS NOT NULL");
    }
    let order = match query.order {
        RequestAuditOrder::NewestFirst => "started_at DESC, id DESC",
        RequestAuditOrder::OldestFirst => "started_at ASC, id ASC",
        RequestAuditOrder::LargestBody => "raw_body_size DESC, started_at DESC, id DESC",
        RequestAuditOrder::Costliest => {
            "cost_micros IS NULL, cost_micros DESC, started_at DESC, id DESC"
        }
    };
    values.push(Box::new(i64::from(query.limit)));
    values.push(Box::new(i64::from(query.offset)));
    let sql = format!(
        "SELECT id, request_id, kind, turn_id, ask_id, started_at, completed_at, provider, model,
             api_base, url, http_status, history_len, attempt, stream, prompt_cache_key,
             background, raw_body_size, body IS NOT NULL, response IS NOT NULL, response_kind,
             response_summary, error IS NOT NULL, error_summary, usage, cost_micros,
             tokens_per_sec
         FROM requests WHERE {} ORDER BY {order} LIMIT ? OFFSET ?",
        clauses.join(" AND ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values.iter().map(AsRef::as_ref)), |row| {
        let usage: Option<String> = row.get(24)?;
        let response_kind: Option<String> = row.get(20)?;
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
            http_status: row.get::<_, Option<i64>>(11)?.map(|status| status as u16),
            history_len: row.get::<_, Option<i64>>(12)?.map(unsigned),
            attempt: row.get::<_, i64>(13)? as u32,
            stream: row.get(14)?,
            prompt_cache_key: row.get(15)?,
            background: row.get(16)?,
            raw_body_size: unsigned(row.get(17)?),
            has_body: row.get(18)?,
            has_response: row.get(19)?,
            response_payload_kind: response_kind
                .as_deref()
                .and_then(ResponsePayloadKind::parse),
            response_summary: row.get(21)?,
            has_error: row.get(22)?,
            error_summary: row.get(23)?,
            usage: usage
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(crate::error::to_sql_error)?,
            cost_usd: row
                .get::<_, Option<i64>>(25)?
                .map(|micros| micros as f64 / 1_000_000.0),
            tokens_per_sec: row.get(26)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn stats(conn: &Connection, sid: i64) -> Result<RequestAuditStats> {
    let mut stats = conn
        .prepare_cached(
            "SELECT count(*), count(error_summary), coalesce(sum(stream), 0),
                 coalesce(sum(response_kind = 'full'), 0), coalesce(sum(cost_micros), 0),
                 coalesce(sum(max(coalesce(completed_at - started_at, 0), 0)), 0),
                 coalesce(sum(input_tokens), 0), coalesce(sum(output_tokens), 0),
                 coalesce(sum(cache_read_tokens), 0), coalesce(sum(cache_write_tokens), 0),
                 coalesce(sum(reasoning_tokens), 0), min(started_at), max(started_at),
                 max(context_tokens)
             FROM requests WHERE sid = ?1",
        )?
        .query_row([sid], |row| {
            Ok(RequestAuditStats {
                request_count: unsigned(row.get(0)?),
                error_count: unsigned(row.get(1)?),
                streaming_count: unsigned(row.get(2)?),
                raw_response_count: unsigned(row.get(3)?),
                total_cost_usd: row.get::<_, i64>(4)? as f64 / 1_000_000.0,
                total_elapsed_ms: unsigned(row.get(5)?),
                total_prompt_tokens: unsigned(row.get(6)?),
                total_completion_tokens: unsigned(row.get(7)?),
                total_cache_read_tokens: unsigned(row.get(8)?),
                total_cache_write_tokens: unsigned(row.get(9)?),
                total_reasoning_tokens: unsigned(row.get(10)?),
                first_request_ms: row.get::<_, Option<i64>>(11)?.map(unsigned),
                latest_timestamp_ms: row.get::<_, Option<i64>>(12)?.map(unsigned),
                max_context_tokens: row
                    .get::<_, Option<i64>>(13)?
                    .map(|value| unsigned(value) as u32),
                ..RequestAuditStats::default()
            })
        })?;
    if let Some((provider, model)) = conn
        .prepare_cached(
            "SELECT provider, model FROM requests WHERE sid = ?1
             ORDER BY started_at DESC, id DESC LIMIT 1",
        )?
        .query_row([sid], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?
    {
        stats.latest_provider_kind = provider;
        stats.latest_model = model;
    }
    stats.latest_context_tokens = conn
        .prepare_cached(
            "SELECT context_tokens FROM requests WHERE sid = ?1 AND context_tokens IS NOT NULL
             ORDER BY started_at DESC, id DESC LIMIT 1",
        )?
        .query_row([sid], |row| row.get::<_, i64>(0))
        .optional()?
        .map(|value| unsigned(value) as u32);
    Ok(stats)
}

pub(crate) fn payloads(
    conn: &Connection,
    sid: i64,
    id: i64,
) -> Result<Option<RequestAuditPayloads>> {
    let row = conn
        .prepare_cached(
            "SELECT body, response, response_kind, error FROM requests WHERE sid = ?1 AND id = ?2",
        )?
        .query_row(params![sid, id], |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })
        .optional()?;
    let Some((body, response, response_kind, error)) = row else {
        return Ok(None);
    };
    let decode = |cell: Option<Vec<u8>>| {
        cell.map(|cell| document::load_json(conn, &cell))
            .transpose()
    };
    Ok(Some(RequestAuditPayloads {
        body: decode(body)?,
        response_payload_kind: response_kind
            .as_deref()
            .and_then(ResponsePayloadKind::parse),
        response: decode(response)?,
        error: decode(error)?,
    }))
}
