use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, ToSql};

use crate::db::unsigned;
use crate::error::Result;

/// A session as the session list shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSession {
    pub id: String,
    pub parent_id: Option<String>,
    pub title: Option<String>,
    pub slug: Option<String>,
    pub first_user_message: Option<String>,
    pub cwd: Option<String>,
    pub mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    pub fast_mode: Option<bool>,
    pub context_tokens: Option<u64>,
    pub history_len: u64,
    pub text_bytes: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogCursor {
    pub updated_at: i64,
    pub id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogQuery {
    pub limit: u32,
    pub cursor: Option<CatalogCursor>,
    pub cwd: Option<String>,
    pub parent_id: Option<String>,
}

impl Default for CatalogQuery {
    fn default() -> Self {
        Self {
            limit: 200,
            cursor: None,
            cwd: None,
            parent_id: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogPage {
    pub sessions: Vec<CatalogSession>,
    pub next_cursor: Option<CatalogCursor>,
}

// The session list shows the first 512 characters of the first message.
const COLUMNS: &str = "s.id, s.parent_id, s.title, s.slug, substr(v.value, 1, 512), s.cwd,
    s.mode, s.reasoning_effort, s.model, s.fast_mode,
    coalesce(s.display_context_tokens, s.context_tokens), s.history_len, s.text_bytes,
    s.created_at, s.updated_at";

const FROM: &str = "FROM sessions s LEFT JOIN session_values v
    ON v.sid = s.sid AND v.name = 'first_user_message'";

fn from_row(row: &Row<'_>) -> rusqlite::Result<CatalogSession> {
    Ok(CatalogSession {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        title: row.get(2)?,
        slug: row.get(3)?,
        first_user_message: row.get(4)?,
        cwd: row.get(5)?,
        mode: row.get(6)?,
        reasoning_effort: row.get(7)?,
        model: row.get(8)?,
        fast_mode: row.get(9)?,
        context_tokens: row.get::<_, Option<i64>>(10)?.map(unsigned),
        history_len: unsigned(row.get(11)?),
        text_bytes: unsigned(row.get(12)?),
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

/// Sessions newest first, optionally only those started in `cwd` or forked from `parent_id`.
pub(crate) fn list(conn: &Connection, query: &CatalogQuery) -> Result<CatalogPage> {
    let limit = query.limit.clamp(1, 1000);
    let fetch = i64::from(limit) + 1;
    let (sql, mut values) = list_statement(query);
    values.push(&fetch);
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), from_row)?;
    let mut sessions = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let next_cursor = if sessions.len() > limit as usize {
        sessions.truncate(limit as usize);
        sessions.last().map(|session| CatalogCursor {
            updated_at: session.updated_at,
            id: session.id.clone(),
        })
    } else {
        None
    };
    Ok(CatalogPage {
        sessions,
        next_cursor,
    })
}

/// The shortest query [`search`] matches: the trigram index needs three characters.
pub const SEARCH_MIN_CHARS: usize = 3;

/// Ids of the sessions whose conversation text contains `query`, ignoring case.
pub(crate) fn search(conn: &Connection, query: &str) -> Result<Vec<String>> {
    if query.chars().count() < SEARCH_MIN_CHARS {
        return Ok(Vec::new());
    }
    let _perf = smelt_perf::perf::begin("store:search_sessions");
    // One quoted phrase, which the trigram index matches as a substring.
    let phrase = format!("\"{}\"", query.replace('"', "\"\""));
    let mut stmt = conn.prepare_cached(
        "SELECT s.id FROM sessions s WHERE s.sid IN (
             SELECT t.sid FROM search JOIN search_text t ON t.id = search.rowid
             WHERE search MATCH ?1)",
    )?;
    let ids = stmt
        .query_map([phrase], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    smelt_perf::perf::record_value("store:search_sessions:matches", ids.len() as u64);
    Ok(ids)
}

/// The list query and its filter values, ending with the row limit parameter.
/// Only the filters in use appear, so each shape seeks its index.
fn list_statement(query: &CatalogQuery) -> (String, Vec<&dyn ToSql>) {
    let mut clauses = Vec::new();
    let mut values: Vec<&dyn ToSql> = Vec::new();
    if let Some(cwd) = &query.cwd {
        clauses.push("s.cwd = ?");
        values.push(cwd);
    }
    if let Some(parent_id) = &query.parent_id {
        clauses.push("s.parent_id = ?");
        values.push(parent_id);
    }
    if let Some(cursor) = &query.cursor {
        clauses.push("(s.updated_at, s.id) < (?, ?)");
        values.push(&cursor.updated_at);
        values.push(&cursor.id);
    }
    let filter = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let sql =
        format!("SELECT {COLUMNS} {FROM} {filter} ORDER BY s.updated_at DESC, s.id DESC LIMIT ?");
    (sql, values)
}

pub(crate) fn session(conn: &Connection, id: &str) -> Result<Option<CatalogSession>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {COLUMNS} {FROM} WHERE s.id = ?1"))?
        .query_row([id], from_row)
        .optional()?)
}

pub(crate) fn ids_with_prefix(conn: &Connection, prefix: &str, limit: u32) -> Result<Vec<String>> {
    // `id >= prefix AND id < prefix + U+10FFFF` uses the unique index on id.
    let upper = format!("{prefix}\u{10FFFF}");
    let mut stmt = conn.prepare_cached(
        "SELECT id FROM sessions WHERE id >= ?1 AND id < ?2 ORDER BY id LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![prefix, upper, i64::from(limit)], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_session_lists_seek_an_index() {
        let cursor = CatalogCursor {
            updated_at: 1,
            id: "id".into(),
        };
        for query in [
            CatalogQuery {
                cwd: Some("/work".into()),
                ..CatalogQuery::default()
            },
            CatalogQuery {
                cwd: Some("/work".into()),
                cursor: Some(cursor.clone()),
                ..CatalogQuery::default()
            },
            CatalogQuery {
                cursor: Some(cursor),
                ..CatalogQuery::default()
            },
            CatalogQuery {
                parent_id: Some("parent".into()),
                ..CatalogQuery::default()
            },
        ] {
            crate::db::assert_query_seeks(&list_statement(&query).0);
        }
    }
}
