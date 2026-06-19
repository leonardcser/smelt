use rusqlite::Connection;

use crate::error::{Result, StoreError};

pub const SCHEMA_VERSION: i32 = 1;

pub(crate) fn migrate(conn: &mut Connection, app_version: &str) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = migrate_inner(conn, app_version);
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

pub(crate) fn validate_read_only_schema(conn: &Connection) -> Result<()> {
    let version = user_version(conn)?;
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(StoreError::UnsupportedSchema {
            found: version,
            expected: SCHEMA_VERSION,
        })
    }
}

pub(crate) fn user_version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn migrate_inner(conn: &Connection, app_version: &str) -> Result<()> {
    let current = user_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: current,
            expected: SCHEMA_VERSION,
        });
    }
    ensure_schema_shape(conn)?;
    set_user_version(conn, SCHEMA_VERSION)?;
    conn.execute(
        "INSERT INTO store_meta (key, value, updated_at)
         VALUES ('schema_version', ?1, unixepoch()), ('app_version', ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (SCHEMA_VERSION.to_string(), app_version),
    )?;
    Ok(())
}

fn ensure_schema_shape(conn: &Connection) -> Result<()> {
    // Version 1 is the plan baseline; keep same-version databases aligned with
    // the current in-tree table shape before creating indexes that reference
    // newly introduced columns.
    ensure_schema_columns(conn)?;
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

fn ensure_schema_columns(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "session_state", "first_user_message TEXT")?;
    add_column_if_missing(conn, "session_state", "reasoning_effort TEXT")?;
    add_column_if_missing(conn, "session_state", "parent_id TEXT")?;
    add_column_if_missing(conn, "session_state", "context_tokens INTEGER")?;
    add_column_if_missing(conn, "session_state", "context_tokens_history_len INTEGER")?;
    add_column_if_missing(conn, "session_state", "display_context_tokens INTEGER")?;
    add_column_if_missing(
        conn,
        "session_state",
        "session_cost_usd REAL NOT NULL DEFAULT 0",
    )?;

    add_column_if_missing(conn, "history_items", "model_visible_hash TEXT")?;
    add_column_if_missing(conn, "history_items", "search_text TEXT")?;
    add_column_if_missing(
        conn,
        "history_items",
        "created_at INTEGER NOT NULL DEFAULT 0",
    )?;

    add_column_if_missing(conn, "transcript_blocks", "content_hash TEXT")?;
    add_column_if_missing(conn, "transcript_blocks", "sidecar_hash TEXT")?;
    add_column_if_missing(
        conn,
        "transcript_blocks",
        "estimated_text_bytes INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "transcript_blocks", "estimated_rows INTEGER")?;
    add_column_if_missing(conn, "transcript_blocks", "preview_text TEXT")?;
    add_column_if_missing(conn, "transcript_blocks", "search_text TEXT")?;
    add_column_if_missing(conn, "transcript_blocks", "descriptor_json TEXT")?;
    add_column_if_missing(conn, "transcript_blocks", "origin_json TEXT")?;
    add_column_if_missing(conn, "transcript_blocks", "tool_state_json TEXT")?;

    add_column_if_missing(conn, "objects", "kind TEXT NOT NULL DEFAULT 'unknown'")?;
    add_column_if_missing(conn, "objects", "codec TEXT NOT NULL DEFAULT 'none'")?;
    add_column_if_missing(conn, "objects", "raw_size INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "objects", "stored_size INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "objects", "created_at INTEGER NOT NULL DEFAULT 0")?;

    add_column_if_missing(
        conn,
        "request_attempts",
        "background INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "request_attempts",
        "raw_body_size INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "request_attempts", "kind TEXT")?;
    add_column_if_missing(conn, "request_attempts", "api_base TEXT")?;
    add_column_if_missing(conn, "request_attempts", "url TEXT")?;
    add_column_if_missing(conn, "request_attempts", "http_status INTEGER")?;
    add_column_if_missing(conn, "request_attempts", "prompt_cache_key TEXT")?;
    add_column_if_missing(
        conn,
        "request_attempts",
        "stream INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "request_attempts",
        "attempt INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(conn, "request_attempts", "response_summary TEXT")?;

    add_column_if_missing(conn, "request_stats", "context_tokens INTEGER")?;
    add_column_if_missing(conn, "request_stats", "cache_write_tokens INTEGER")?;
    add_column_if_missing(conn, "request_stats", "tokens_per_sec REAL")?;

    add_column_if_missing(
        conn,
        "metadata_snapshots",
        "created_at INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "accounting_snapshots",
        "created_at INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "transcript_search", "history_idx INTEGER")?;
    Ok(())
}

fn add_column_if_missing(conn: &Connection, table: &str, column_def: &str) -> Result<()> {
    let Some(column) = column_def.split_whitespace().next() else {
        return Ok(());
    };
    if !table_exists(conn, table)? || column_exists(conn, table, column)? {
        return Ok(());
    }
    conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column_def}"), [])?;
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1")?;
    Ok(stmt.exists([table])?)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn set_user_version(conn: &Connection, version: i32) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
    Ok(())
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS store_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS session_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    id TEXT NOT NULL UNIQUE,
    title TEXT,
    slug TEXT,
    first_user_message TEXT,
    cwd TEXT,
    mode TEXT,
    reasoning_effort TEXT,
    model TEXT,
    parent_id TEXT,
    accounting_json TEXT,
    checkpoint_json TEXT,
    context_tokens INTEGER,
    context_tokens_history_len INTEGER,
    display_context_tokens INTEGER,
    session_cost_usd REAL NOT NULL DEFAULT 0,
    revision INTEGER NOT NULL DEFAULT 0,
    history_len INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS history_items (
    idx INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    json TEXT NOT NULL,
    hash TEXT NOT NULL,
    model_visible_hash TEXT,
    search_text TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS history_items_kind_idx ON history_items(kind, idx);
CREATE INDEX IF NOT EXISTS history_items_created_at_idx ON history_items(created_at, idx);

CREATE TABLE IF NOT EXISTS transcript_blocks (
    block_idx INTEGER PRIMARY KEY,
    history_idx INTEGER REFERENCES history_items(idx) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    tool_call_id TEXT,
    tool_name TEXT,
    content_hash TEXT,
    sidecar_hash TEXT,
    estimated_text_bytes INTEGER NOT NULL DEFAULT 0,
    estimated_rows INTEGER,
    preview_text TEXT,
    search_text TEXT,
    descriptor_json TEXT,
    origin_json TEXT,
    tool_state_json TEXT
);
CREATE INDEX IF NOT EXISTS transcript_blocks_history_idx ON transcript_blocks(history_idx, block_idx);
CREATE INDEX IF NOT EXISTS transcript_blocks_kind_idx ON transcript_blocks(kind, block_idx);
CREATE INDEX IF NOT EXISTS transcript_blocks_tool_call_id_idx ON transcript_blocks(tool_call_id);

CREATE TABLE IF NOT EXISTS objects (
    hash TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    codec TEXT NOT NULL CHECK (codec IN ('none', 'zstd')),
    raw_size INTEGER NOT NULL,
    stored_size INTEGER NOT NULL,
    bytes BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS objects_kind_idx ON objects(kind, raw_size DESC);

CREATE TABLE IF NOT EXISTS history_object_refs (
    history_idx INTEGER NOT NULL REFERENCES history_items(idx) ON DELETE CASCADE,
    object_hash TEXT NOT NULL REFERENCES objects(hash) ON DELETE RESTRICT,
    role TEXT NOT NULL,
    PRIMARY KEY (history_idx, object_hash, role)
);

CREATE TABLE IF NOT EXISTS request_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT,
    turn_id TEXT,
    ask_id TEXT,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    provider TEXT,
    model TEXT,
    history_len INTEGER,
    body_hash TEXT REFERENCES objects(hash) ON DELETE RESTRICT,
    response_hash TEXT REFERENCES objects(hash) ON DELETE RESTRICT,
    error_hash TEXT REFERENCES objects(hash) ON DELETE RESTRICT,
    error_summary TEXT,
    background INTEGER NOT NULL DEFAULT 0,
    raw_body_size INTEGER NOT NULL DEFAULT 0,
    kind TEXT,
    api_base TEXT,
    url TEXT,
    http_status INTEGER,
    prompt_cache_key TEXT,
    stream INTEGER NOT NULL DEFAULT 0,
    attempt INTEGER NOT NULL DEFAULT 1,
    response_summary TEXT
);
CREATE INDEX IF NOT EXISTS request_attempts_started_at_idx ON request_attempts(started_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS request_attempts_request_id_idx ON request_attempts(request_id);
CREATE INDEX IF NOT EXISTS request_attempts_turn_ask_idx ON request_attempts(turn_id, ask_id, id);
CREATE INDEX IF NOT EXISTS request_attempts_provider_model_idx ON request_attempts(provider, model, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_error_idx ON request_attempts(error_summary, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_background_idx ON request_attempts(background, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_body_size_idx ON request_attempts(raw_body_size DESC);
CREATE INDEX IF NOT EXISTS request_attempts_url_idx ON request_attempts(url);

CREATE TABLE IF NOT EXISTS request_object_refs (
    request_attempt_id INTEGER NOT NULL REFERENCES request_attempts(id) ON DELETE CASCADE,
    object_hash TEXT NOT NULL REFERENCES objects(hash) ON DELETE RESTRICT,
    role TEXT NOT NULL,
    PRIMARY KEY (request_attempt_id, object_hash, role)
);

CREATE TABLE IF NOT EXISTS request_stats (
    request_attempt_id INTEGER PRIMARY KEY REFERENCES request_attempts(id) ON DELETE CASCADE,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cached_input_tokens INTEGER,
    reasoning_tokens INTEGER,
    total_cost_micros INTEGER,
    stats_json TEXT,
    context_tokens INTEGER,
    cache_write_tokens INTEGER,
    tokens_per_sec REAL
);
CREATE INDEX IF NOT EXISTS request_stats_input_tokens_idx ON request_stats(input_tokens DESC);
CREATE INDEX IF NOT EXISTS request_stats_output_tokens_idx ON request_stats(output_tokens DESC);
CREATE INDEX IF NOT EXISTS request_stats_total_cost_idx ON request_stats(total_cost_micros DESC);

CREATE TABLE IF NOT EXISTS turn_metas (
    turn_idx INTEGER PRIMARY KEY,
    meta_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS turn_tool_elapsed (
    turn_idx INTEGER NOT NULL,
    tool_call_id TEXT NOT NULL,
    elapsed_ms INTEGER NOT NULL,
    PRIMARY KEY (turn_idx, tool_call_id)
);

CREATE TABLE IF NOT EXISTS metadata_snapshots (
    history_idx INTEGER PRIMARY KEY,
    metadata_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS accounting_snapshots (
    history_idx INTEGER PRIMARY KEY,
    accounting_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS transcript_search (
    block_idx INTEGER PRIMARY KEY REFERENCES transcript_blocks(block_idx) ON DELETE CASCADE,
    history_idx INTEGER,
    text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS transcript_search_history_idx ON transcript_search(history_idx, block_idx);

CREATE TABLE IF NOT EXISTS transcript_search_terms (
    term TEXT NOT NULL,
    block_idx INTEGER NOT NULL REFERENCES transcript_search(block_idx) ON DELETE CASCADE,
    PRIMARY KEY (term, block_idx)
);
CREATE INDEX IF NOT EXISTS transcript_search_terms_block_idx ON transcript_search_terms(block_idx);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_repairs_in_place_version_one_session_state_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE store_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE TABLE session_state (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                id TEXT NOT NULL UNIQUE,
                title TEXT,
                slug TEXT,
                cwd TEXT,
                mode TEXT,
                model TEXT,
                accounting_json TEXT,
                checkpoint_json TEXT,
                revision INTEGER NOT NULL DEFAULT 0,
                history_len INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            INSERT INTO session_state (
                singleton, id, title, slug, cwd, mode, model,
                accounting_json, checkpoint_json, revision, history_len,
                created_at, updated_at
            ) VALUES (
                1, 'old-session', 'Old Session', 'old-session', '/tmp',
                'normal', 'model', '{}', NULL, 7, 2, 1000, 2000
            );
            PRAGMA user_version = 1;
            "#,
        )
        .unwrap();

        migrate(&mut conn, "test-version").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        for column in [
            "first_user_message",
            "reasoning_effort",
            "parent_id",
            "context_tokens",
            "context_tokens_history_len",
            "display_context_tokens",
            "session_cost_usd",
        ] {
            assert!(
                column_exists(&conn, "session_state", column).unwrap(),
                "{column}"
            );
        }
        let (id, cost): (String, f64) = conn
            .query_row(
                "SELECT id, session_cost_usd FROM session_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(id, "old-session");
        assert_eq!(cost, 0.0);
    }
}
