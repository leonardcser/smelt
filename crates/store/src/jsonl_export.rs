use std::io::Write;

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::error::Result;
use crate::request_audit;

pub(crate) fn export_lineage_requests_jsonl(
    conn: &Connection,
    lineage_id: &str,
    session_id: &str,
    mut out: impl Write,
) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT a.id, request_id, kind, turn_id, ask_id, started_at, completed_at, provider,
                model, history_len, error_summary, background, api_base, url, http_status,
                prompt_cache_key, stream, attempt, s.stats_json, s.total_cost_micros,
                s.tokens_per_sec
         FROM request_attempts a
         JOIN lineage_request_attempts branch
           ON branch.request_attempt_id = a.id
          AND branch.lineage_id = ?1
          AND branch.session_id = ?2
         LEFT JOIN request_stats s ON s.request_attempt_id = a.id
         ORDER BY started_at, a.id",
    )?;
    let rows = stmt.query_map(params![lineage_id, session_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<String>>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, i64>(11)?,
            row.get::<_, Option<String>>(12)?,
            row.get::<_, Option<String>>(13)?,
            row.get::<_, Option<i64>>(14)?,
            row.get::<_, Option<String>>(15)?,
            row.get::<_, i64>(16)?,
            row.get::<_, i64>(17)?,
            row.get::<_, Option<String>>(18)?,
            row.get::<_, Option<i64>>(19)?,
            row.get::<_, Option<f64>>(20)?,
        ))
    })?;

    for row in rows {
        let (
            id,
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
        if let Some(payloads) = request_audit::request_payloads(conn, id)? {
            if let Some(body) = payloads.body {
                value["body"] = body;
            }
            if let Some(response) = payloads.response {
                value["response"] = response;
            }
            if let Some(error) = payloads.error {
                value["error"] = error;
            } else if let Some(summary) = error_summary {
                value["error"] = json!({ "message": summary });
            }
        } else if let Some(summary) = error_summary {
            value["error"] = json!({ "message": summary });
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
