use rusqlite::Connection;
use std::sync::OnceLock;

use crate::error::{Result, StoreError};

pub const SCHEMA_VERSION: i32 = 2;

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
    if !is_supported_schema_version(version) {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            expected: SCHEMA_VERSION,
        });
    }
    validate_schema_shape(conn)
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
    if current == 1 {
        migrate_v1_to_v2(conn)?;
    }
    ensure_schema_shape(conn)?;
    set_user_version(conn, SCHEMA_VERSION)?;
    write_store_meta(conn, SCHEMA_VERSION, app_version)?;
    Ok(())
}

fn is_supported_schema_version(version: i32) -> bool {
    version == SCHEMA_VERSION
}

fn write_store_meta(conn: &Connection, schema_version: i32, app_version: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO store_meta (key, value, updated_at)
         VALUES ('schema_version', ?1, unixepoch()), ('app_version', ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (schema_version.to_string(), app_version),
    )?;
    Ok(())
}

fn ensure_schema_shape(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    validate_schema_shape(conn)
}

fn validate_schema_shape(conn: &Connection) -> Result<()> {
    for table in canonical_schema_shape()? {
        if !table_exists(conn, &table.name)? {
            return Err(StoreError::Integrity(format!(
                "sqlite schema missing table {}",
                table.name
            )));
        }
        for column in &table.columns {
            if !column_exists(conn, &table.name, column)? {
                return Err(StoreError::Integrity(format!(
                    "sqlite schema missing column {}.{column}",
                    table.name
                )));
            }
        }
    }
    Ok(())
}

struct SchemaTable {
    name: String,
    columns: Vec<String>,
}

fn canonical_schema_shape() -> Result<&'static [SchemaTable]> {
    static SHAPE: OnceLock<std::result::Result<Vec<SchemaTable>, String>> = OnceLock::new();
    match SHAPE.get_or_init(load_canonical_schema_shape) {
        Ok(shape) => Ok(shape.as_slice()),
        Err(message) => Err(StoreError::Integrity(message.clone())),
    }
}

fn load_canonical_schema_shape() -> std::result::Result<Vec<SchemaTable>, String> {
    let conn = Connection::open_in_memory().map_err(|err| err.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|err| err.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .map_err(|err| err.to_string())?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|err| err.to_string())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    let mut tables = Vec::new();
    for name in names {
        let mut columns = Vec::new();
        let mut info = conn
            .prepare(&format!("PRAGMA table_info({name})"))
            .map_err(|err| err.to_string())?;
        let mut rows = info.query([]).map_err(|err| err.to_string())?;
        while let Some(row) = rows.next().map_err(|err| err.to_string())? {
            columns.push(row.get(1).map_err(|err| err.to_string())?);
        }
        tables.push(SchemaTable { name, columns });
    }
    Ok(tables)
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

fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        DROP INDEX IF EXISTS transcript_search_history_idx;
        DROP INDEX IF EXISTS transcript_search_terms_block_idx;
        DROP TABLE IF EXISTS transcript_search_terms;

        ALTER TABLE transcript_search RENAME TO transcript_search_old;
        ALTER TABLE transcript_blocks RENAME TO transcript_blocks_old;

        CREATE TABLE transcript_blocks (
            block_idx INTEGER PRIMARY KEY,
            descriptor_idx INTEGER,
            history_idx INTEGER REFERENCES history_items(idx) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            tool_call_id TEXT,
            tool_name TEXT,
            content_hash TEXT,
            sidecar_hash TEXT,
            estimated_text_bytes INTEGER NOT NULL DEFAULT 0,
            estimated_rows INTEGER,
            preview_text TEXT,
            descriptor_json TEXT,
            origin_json TEXT,
            tool_state_json TEXT
        );

        CREATE TABLE transcript_search (
            block_idx INTEGER PRIMARY KEY REFERENCES transcript_blocks(block_idx) ON DELETE CASCADE,
            history_idx INTEGER,
            indexed_text TEXT NOT NULL
        );

        CREATE VIRTUAL TABLE transcript_search_fts USING fts5(
            indexed_text,
            content='transcript_search',
            content_rowid='block_idx',
            tokenize='trigram'
        );

        CREATE TRIGGER transcript_search_ai AFTER INSERT ON transcript_search BEGIN
            INSERT INTO transcript_search_fts(rowid, indexed_text)
            VALUES (new.block_idx, new.indexed_text);
        END;
        CREATE TRIGGER transcript_search_ad AFTER DELETE ON transcript_search BEGIN
            INSERT INTO transcript_search_fts(transcript_search_fts, rowid, indexed_text)
            VALUES ('delete', old.block_idx, old.indexed_text);
        END;
        CREATE TRIGGER transcript_search_au AFTER UPDATE OF indexed_text ON transcript_search BEGIN
            INSERT INTO transcript_search_fts(transcript_search_fts, rowid, indexed_text)
            VALUES ('delete', old.block_idx, old.indexed_text);
            INSERT INTO transcript_search_fts(rowid, indexed_text)
            VALUES (new.block_idx, new.indexed_text);
        END;

        INSERT INTO transcript_blocks (
            block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
            sidecar_hash, estimated_text_bytes, estimated_rows, preview_text, descriptor_json,
            origin_json, tool_state_json
        )
        SELECT block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name, content_hash,
               sidecar_hash, estimated_text_bytes, estimated_rows, preview_text, descriptor_json,
               origin_json, tool_state_json
        FROM transcript_blocks_old
        ORDER BY block_idx;

        INSERT INTO transcript_search (block_idx, history_idx, indexed_text)
        SELECT block_idx, history_idx, text
        FROM transcript_search_old
        ORDER BY block_idx;

        DROP TABLE transcript_search_old;
        DROP TABLE transcript_blocks_old;
        "#,
    )?;
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
    descriptor_idx INTEGER,
    history_idx INTEGER REFERENCES history_items(idx) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    tool_call_id TEXT,
    tool_name TEXT,
    content_hash TEXT,
    sidecar_hash TEXT,
    estimated_text_bytes INTEGER NOT NULL DEFAULT 0,
    estimated_rows INTEGER,
    preview_text TEXT,
    descriptor_json TEXT,
    origin_json TEXT,
    tool_state_json TEXT
);
CREATE INDEX IF NOT EXISTS transcript_blocks_history_idx ON transcript_blocks(history_idx, block_idx);
CREATE UNIQUE INDEX IF NOT EXISTS transcript_blocks_descriptor_idx
    ON transcript_blocks(descriptor_idx)
    WHERE descriptor_json IS NOT NULL;
CREATE INDEX IF NOT EXISTS transcript_blocks_kind_idx
    ON transcript_blocks(kind, descriptor_idx)
    WHERE descriptor_json IS NOT NULL;
CREATE INDEX IF NOT EXISTS transcript_blocks_tool_call_id_idx ON transcript_blocks(tool_call_id);
CREATE INDEX IF NOT EXISTS transcript_blocks_extent_idx
    ON transcript_blocks(descriptor_idx, kind, estimated_rows, estimated_text_bytes, preview_text)
    WHERE descriptor_json IS NOT NULL;

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
    indexed_text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS transcript_search_history_idx ON transcript_search(history_idx, block_idx);

CREATE VIRTUAL TABLE IF NOT EXISTS transcript_search_fts USING fts5(
    indexed_text,
    content='transcript_search',
    content_rowid='block_idx',
    tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS transcript_search_ai AFTER INSERT ON transcript_search BEGIN
    INSERT INTO transcript_search_fts(rowid, indexed_text)
    VALUES (new.block_idx, new.indexed_text);
END;
CREATE TRIGGER IF NOT EXISTS transcript_search_ad AFTER DELETE ON transcript_search BEGIN
    INSERT INTO transcript_search_fts(transcript_search_fts, rowid, indexed_text)
    VALUES ('delete', old.block_idx, old.indexed_text);
END;
CREATE TRIGGER IF NOT EXISTS transcript_search_au AFTER UPDATE OF indexed_text ON transcript_search BEGIN
    INSERT INTO transcript_search_fts(transcript_search_fts, rowid, indexed_text)
    VALUES ('delete', old.block_idx, old.indexed_text);
    INSERT INTO transcript_search_fts(rowid, indexed_text)
    VALUES (new.block_idx, new.indexed_text);
END;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_sqlite_supports_fts5_trigram_search() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE VIRTUAL TABLE transcript_search_fts_probe USING fts5(
                indexed_text,
                tokenize='trigram'
            );
            INSERT INTO transcript_search_fts_probe(rowid, indexed_text)
            VALUES (7, 'alpha needle omega');
            "#,
        )
        .unwrap();

        let rowids = conn
            .prepare(
                "SELECT rowid FROM transcript_search_fts_probe
                 WHERE indexed_text MATCH 'needle'",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(rowids, vec![7]);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM transcript_search_fts_probe
                 WHERE indexed_text MATCH 'zzzz'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn v1_to_v2_migration_moves_search_text_to_fts_indexed_text() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE history_items (
                idx INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                json TEXT NOT NULL,
                hash TEXT NOT NULL,
                model_visible_hash TEXT,
                search_text TEXT,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            );
            CREATE TABLE transcript_blocks (
                block_idx INTEGER PRIMARY KEY,
                descriptor_idx INTEGER,
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
            CREATE TABLE transcript_search (
                block_idx INTEGER PRIMARY KEY REFERENCES transcript_blocks(block_idx) ON DELETE CASCADE,
                history_idx INTEGER,
                text TEXT NOT NULL
            );
            CREATE TABLE transcript_search_terms (
                term TEXT NOT NULL,
                block_idx INTEGER NOT NULL REFERENCES transcript_search(block_idx) ON DELETE CASCADE,
                PRIMARY KEY (term, block_idx)
            );
            CREATE INDEX transcript_search_terms_block_idx ON transcript_search_terms(block_idx);
            INSERT INTO transcript_blocks (
                block_idx, descriptor_idx, kind, content_hash, estimated_text_bytes,
                preview_text, search_text, descriptor_json
            ) VALUES (7, 0, 'text', 'hash', 12, 'preview', 'needle text', '{}');
            INSERT INTO transcript_search (block_idx, history_idx, text)
            VALUES (7, NULL, 'needle text');
            INSERT INTO transcript_search_terms (term, block_idx) VALUES ('nee', 7);
            PRAGMA user_version = 1;
            "#,
        )
        .unwrap();

        migrate(&mut conn, "test").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(!column_exists(&conn, "transcript_blocks", "search_text").unwrap());
        assert!(!table_exists(&conn, "transcript_search_terms").unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT indexed_text FROM transcript_search WHERE block_idx = 7",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "needle text"
        );
        assert_eq!(
            conn.query_row(
                "SELECT rowid FROM transcript_search_fts WHERE indexed_text MATCH '\"needle\"'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            7
        );
    }

    #[test]
    fn read_only_validation_rejects_same_version_wrong_shape() {
        let conn = Connection::open_in_memory().unwrap();
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
                slug TEXT
            );
            PRAGMA user_version = 2;
            "#,
        )
        .unwrap();

        let err = validate_read_only_schema(&conn).unwrap_err();
        assert!(
            err.to_string()
                .contains("sqlite schema missing table accounting_snapshots"),
            "{err}"
        );
    }

    #[test]
    fn read_only_validation_rejects_unknown_future_version() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        set_user_version(&conn, SCHEMA_VERSION + 1).unwrap();

        let err = validate_read_only_schema(&conn).unwrap_err();
        assert!(
            err.to_string().contains("unsupported schema version 3"),
            "{err}"
        );
    }
}
