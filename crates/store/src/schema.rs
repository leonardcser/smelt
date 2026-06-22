use rusqlite::Connection;
use std::sync::OnceLock;

use crate::error::{Result, StoreError};

pub const SCHEMA_VERSION: i32 = 1;

const COMPAT_PRE_SQUASH_MIN_SCHEMA_VERSION: i32 = 2;
const COMPAT_PRE_SQUASH_MAX_SCHEMA_VERSION: i32 = 6;

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
        if is_pre_squash_schema_version(current) {
            ensure_schema_shape(conn)?;
            set_user_version(conn, SCHEMA_VERSION)?;
            write_store_meta(conn, SCHEMA_VERSION, app_version)?;
            return Ok(());
        }
        return Err(StoreError::UnsupportedSchema {
            found: current,
            expected: SCHEMA_VERSION,
        });
    }
    ensure_schema_shape(conn)?;
    set_user_version(conn, SCHEMA_VERSION)?;
    write_store_meta(conn, SCHEMA_VERSION, app_version)?;
    Ok(())
}

fn is_supported_schema_version(version: i32) -> bool {
    version == SCHEMA_VERSION || is_pre_squash_schema_version(version)
}

// COMPAT(branch-sqlite-schema-shape-repair): versions 2-6 were used by
// local databases before the transcript-storage revisions were squashed into
// the version 1 branch baseline. Read-only open accepts them for previews;
// writable migration normalizes them to the current baseline.
fn is_pre_squash_schema_version(version: i32) -> bool {
    (COMPAT_PRE_SQUASH_MIN_SCHEMA_VERSION..=COMPAT_PRE_SQUASH_MAX_SCHEMA_VERSION).contains(&version)
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
    // COMPAT(branch-sqlite-schema-shape-repair): version 1 is the plan baseline,
    // but local databases from earlier iterations of this unreleased branch may
    // have the same user_version with older table shapes.
    ensure_schema_columns(conn)?;
    conn.execute_batch(SCHEMA)?;
    backfill_transcript_descriptor_indexes(conn)?;
    Ok(())
}

fn ensure_schema_columns(conn: &Connection) -> Result<()> {
    for column in SAME_VERSION_REPAIR_COLUMNS {
        add_column_if_missing(conn, column.table, column.definition)?;
    }
    Ok(())
}

fn backfill_transcript_descriptor_indexes(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "transcript_blocks")?
        || !column_exists(conn, "transcript_blocks", "descriptor_idx")?
    {
        return Ok(());
    }
    conn.execute(
        "UPDATE transcript_blocks
         SET descriptor_idx = (
             SELECT COUNT(*)
             FROM transcript_blocks AS previous
             WHERE previous.descriptor_json IS NOT NULL
               AND previous.block_idx < transcript_blocks.block_idx
         )
         WHERE descriptor_json IS NOT NULL
           AND descriptor_idx IS NULL",
        [],
    )?;
    Ok(())
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

struct RepairColumn {
    table: &'static str,
    definition: &'static str,
}

struct SchemaTable {
    name: String,
    columns: Vec<String>,
}

const SAME_VERSION_REPAIR_COLUMNS: &[RepairColumn] = &[
    RepairColumn {
        table: "session_state",
        definition: "first_user_message TEXT",
    },
    RepairColumn {
        table: "session_state",
        definition: "reasoning_effort TEXT",
    },
    RepairColumn {
        table: "session_state",
        definition: "parent_id TEXT",
    },
    RepairColumn {
        table: "session_state",
        definition: "context_tokens INTEGER",
    },
    RepairColumn {
        table: "session_state",
        definition: "context_tokens_history_len INTEGER",
    },
    RepairColumn {
        table: "session_state",
        definition: "display_context_tokens INTEGER",
    },
    RepairColumn {
        table: "session_state",
        definition: "session_cost_usd REAL NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "history_items",
        definition: "model_visible_hash TEXT",
    },
    RepairColumn {
        table: "history_items",
        definition: "search_text TEXT",
    },
    RepairColumn {
        table: "history_items",
        definition: "created_at INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "descriptor_idx INTEGER",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "content_hash TEXT",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "sidecar_hash TEXT",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "estimated_text_bytes INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "estimated_rows INTEGER",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "preview_text TEXT",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "search_text TEXT",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "descriptor_json TEXT",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "origin_json TEXT",
    },
    RepairColumn {
        table: "transcript_blocks",
        definition: "tool_state_json TEXT",
    },
    RepairColumn {
        table: "objects",
        definition: "kind TEXT NOT NULL DEFAULT 'unknown'",
    },
    RepairColumn {
        table: "objects",
        definition: "codec TEXT NOT NULL DEFAULT 'none'",
    },
    RepairColumn {
        table: "objects",
        definition: "raw_size INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "objects",
        definition: "stored_size INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "objects",
        definition: "created_at INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "background INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "raw_body_size INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "kind TEXT",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "api_base TEXT",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "url TEXT",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "http_status INTEGER",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "prompt_cache_key TEXT",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "stream INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "attempt INTEGER NOT NULL DEFAULT 1",
    },
    RepairColumn {
        table: "request_attempts",
        definition: "response_summary TEXT",
    },
    RepairColumn {
        table: "request_stats",
        definition: "context_tokens INTEGER",
    },
    RepairColumn {
        table: "request_stats",
        definition: "cache_write_tokens INTEGER",
    },
    RepairColumn {
        table: "request_stats",
        definition: "tokens_per_sec REAL",
    },
    RepairColumn {
        table: "metadata_snapshots",
        definition: "created_at INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "accounting_snapshots",
        definition: "created_at INTEGER NOT NULL DEFAULT 0",
    },
    RepairColumn {
        table: "transcript_search",
        definition: "history_idx INTEGER",
    },
];

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

fn column_name(column_def: &str) -> Option<&str> {
    column_def.split_whitespace().next()
}

fn add_column_if_missing(conn: &Connection, table: &str, column_def: &str) -> Result<()> {
    let Some(column) = column_name(column_def) else {
        return Ok(());
    };
    if !table_exists(conn, table)? || column_exists(conn, table, column)? {
        return Ok(());
    }
    let column_def = alter_column_def(column_def);
    conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column_def}"), [])?;
    Ok(())
}

fn alter_column_def(column_def: &str) -> std::borrow::Cow<'_, str> {
    if column_def.contains("DEFAULT (unixepoch())") {
        return std::borrow::Cow::Owned(column_def.replace("DEFAULT (unixepoch())", "DEFAULT 0"));
    }
    std::borrow::Cow::Borrowed(column_def)
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
            CREATE TABLE history_items (
                idx INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                json TEXT NOT NULL,
                hash TEXT NOT NULL
            );
            INSERT INTO history_items (idx, kind, json, hash)
            VALUES (0, 'user', '{}', 'hash');
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
        let created_at: i64 = conn
            .query_row(
                "SELECT created_at FROM history_items WHERE idx = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created_at, 0);
        validate_read_only_schema(&conn).unwrap();
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
            PRAGMA user_version = 1;
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
    fn read_only_validation_accepts_pre_squash_user_version_when_shape_matches() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        set_user_version(&conn, COMPAT_PRE_SQUASH_MAX_SCHEMA_VERSION).unwrap();

        validate_read_only_schema(&conn).unwrap();
        assert_eq!(
            user_version(&conn).unwrap(),
            COMPAT_PRE_SQUASH_MAX_SCHEMA_VERSION
        );
    }

    #[test]
    fn migrate_normalizes_pre_squash_user_version_to_current_baseline() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        set_user_version(&conn, COMPAT_PRE_SQUASH_MAX_SCHEMA_VERSION).unwrap();

        migrate(&mut conn, "test-version").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let schema_version: String = conn
            .query_row(
                "SELECT value FROM store_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema_version, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn read_only_validation_rejects_unknown_future_version() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        set_user_version(&conn, COMPAT_PRE_SQUASH_MAX_SCHEMA_VERSION + 1).unwrap();

        let err = validate_read_only_schema(&conn).unwrap_err();
        assert!(
            err.to_string().contains("unsupported schema version 7"),
            "{err}"
        );
    }
}
