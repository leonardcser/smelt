use rusqlite::Connection;

use crate::error::{Result, StoreError};

pub const SCHEMA_VERSION: i32 = 5;

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
    let mut current = user_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: current,
            expected: SCHEMA_VERSION,
        });
    }
    if current < 1 {
        conn.execute_batch(MIGRATION_1)?;
        set_user_version(conn, 1)?;
        current = 1;
    }
    if current < 2 {
        conn.execute_batch(MIGRATION_2)?;
        set_user_version(conn, 2)?;
        current = 2;
    }
    if current < 3 {
        conn.execute_batch(MIGRATION_3)?;
        set_user_version(conn, 3)?;
        current = 3;
    }
    if current < 4 {
        conn.execute_batch(MIGRATION_4)?;
        set_user_version(conn, 4)?;
        current = 4;
    }
    if current < 5 {
        conn.execute_batch(MIGRATION_5)?;
        set_user_version(conn, 5)?;
    }
    conn.execute(
        "INSERT INTO store_meta (key, value, updated_at)
         VALUES ('schema_version', ?1, unixepoch()), ('app_version', ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (SCHEMA_VERSION.to_string(), app_version),
    )?;
    Ok(())
}

fn set_user_version(conn: &Connection, version: i32) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
    Ok(())
}

const MIGRATION_1: &str = r#"
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
    search_text TEXT
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
    raw_body_size INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS request_attempts_started_at_idx ON request_attempts(started_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS request_attempts_request_id_idx ON request_attempts(request_id);
CREATE INDEX IF NOT EXISTS request_attempts_turn_ask_idx ON request_attempts(turn_id, ask_id, id);
CREATE INDEX IF NOT EXISTS request_attempts_provider_model_idx ON request_attempts(provider, model, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_error_idx ON request_attempts(error_summary, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_background_idx ON request_attempts(background, started_at DESC);
CREATE INDEX IF NOT EXISTS request_attempts_body_size_idx ON request_attempts(raw_body_size DESC);

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
    stats_json TEXT
);

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
"#;

const MIGRATION_2: &str = r#"
ALTER TABLE request_attempts ADD COLUMN kind TEXT;
"#;

const MIGRATION_3: &str = r#"
ALTER TABLE request_attempts ADD COLUMN api_base TEXT;
ALTER TABLE request_attempts ADD COLUMN url TEXT;
ALTER TABLE request_attempts ADD COLUMN http_status INTEGER;
ALTER TABLE request_attempts ADD COLUMN prompt_cache_key TEXT;
ALTER TABLE request_attempts ADD COLUMN stream INTEGER NOT NULL DEFAULT 0;
ALTER TABLE request_attempts ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;
ALTER TABLE request_attempts ADD COLUMN response_summary TEXT;
ALTER TABLE request_stats ADD COLUMN context_tokens INTEGER;
ALTER TABLE request_stats ADD COLUMN cache_write_tokens INTEGER;
ALTER TABLE request_stats ADD COLUMN tokens_per_sec REAL;
CREATE INDEX IF NOT EXISTS request_attempts_url_idx ON request_attempts(url);
CREATE INDEX IF NOT EXISTS request_stats_input_tokens_idx ON request_stats(input_tokens DESC);
CREATE INDEX IF NOT EXISTS request_stats_output_tokens_idx ON request_stats(output_tokens DESC);
CREATE INDEX IF NOT EXISTS request_stats_total_cost_idx ON request_stats(total_cost_micros DESC);
"#;

const MIGRATION_4: &str = r#"
ALTER TABLE transcript_blocks ADD COLUMN descriptor_json TEXT;
ALTER TABLE transcript_blocks ADD COLUMN origin_json TEXT;
ALTER TABLE transcript_blocks ADD COLUMN tool_state_json TEXT;
"#;

const MIGRATION_5: &str = r#"
ALTER TABLE session_state ADD COLUMN first_user_message TEXT;
ALTER TABLE session_state ADD COLUMN reasoning_effort TEXT;
ALTER TABLE session_state ADD COLUMN parent_id TEXT;
ALTER TABLE session_state ADD COLUMN context_tokens INTEGER;
ALTER TABLE session_state ADD COLUMN context_tokens_history_len INTEGER;
ALTER TABLE session_state ADD COLUMN display_context_tokens INTEGER;
ALTER TABLE session_state ADD COLUMN session_cost_usd REAL NOT NULL DEFAULT 0;
"#;
