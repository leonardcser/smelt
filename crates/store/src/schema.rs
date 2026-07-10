use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::sync::OnceLock;

use crate::error::{Result, StoreError};

pub const SCHEMA_VERSION: i32 = 3;

pub(crate) fn migrate(conn: &mut Connection, app_version: &str) -> Result<()> {
    let current = user_version(conn)?;
    let rebuild = matches!(current, 1 | 2);
    if rebuild {
        conn.pragma_update(None, "foreign_keys", false)?;
    }
    let result = (|| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        migrate_inner(&tx, current, app_version)?;
        tx.commit()?;
        Ok(())
    })();
    if rebuild {
        conn.pragma_update(None, "foreign_keys", true)?;
    }
    result
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

fn migrate_inner(conn: &Connection, current: i32, app_version: &str) -> Result<()> {
    if current > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: current,
            expected: SCHEMA_VERSION,
        });
    }
    let mut current = current;
    if current == 1 {
        migrate_v1_to_v2(conn)?;
        conn.execute_batch(SCHEMA)?;
        current = 2;
    }
    if current == 2 {
        migrate_v2_to_v3(conn)?;
    }
    if (1..=2).contains(&current) {
        migrate_v2_to_v3(conn)?;
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
    let shape = canonical_schema_shape()?;
    for table in &shape.tables {
        let Some(actual_sql) = schema_object_sql(conn, "table", &table.name)? else {
            return Err(StoreError::Integrity(format!(
                "sqlite schema missing table {}",
                table.name
            )));
        };
        let actual = table_columns(conn, &table.name)?;
        if actual != table.columns {
            return Err(StoreError::Integrity(format!(
                "sqlite schema columns differ for {}: expected {:?}, found {:?}",
                table.name, table.columns, actual
            )));
        }
        if normalized_sql(&actual_sql) != normalized_sql(&table.sql) {
            return Err(StoreError::Integrity(format!(
                "sqlite schema definition differs for table {}",
                table.name
            )));
        }
        let actual_foreign_keys = table_foreign_keys(conn, &table.name)?;
        if actual_foreign_keys != table.foreign_keys {
            return Err(StoreError::Integrity(format!(
                "sqlite foreign keys differ for table {}",
                table.name
            )));
        }
    }
    for object in &shape.objects {
        let Some(actual_sql) = schema_object_sql(conn, &object.kind, &object.name)? else {
            return Err(StoreError::Integrity(format!(
                "sqlite schema missing {} {}",
                object.kind, object.name
            )));
        };
        if normalized_sql(&actual_sql) != normalized_sql(&object.sql) {
            return Err(StoreError::Integrity(format!(
                "sqlite schema definition differs for {} {}",
                object.kind, object.name
            )));
        }
    }
    Ok(())
}

struct SchemaTable {
    name: String,
    columns: Vec<String>,
    foreign_keys: Vec<SchemaForeignKey>,
    sql: String,
}

#[derive(Debug, Eq, PartialEq)]
struct SchemaForeignKey {
    id: i64,
    sequence: i64,
    target_table: String,
    source_column: String,
    target_column: Option<String>,
    on_update: String,
    on_delete: String,
    match_kind: String,
}

struct SchemaObject {
    kind: String,
    name: String,
    sql: String,
}

struct SchemaShape {
    tables: Vec<SchemaTable>,
    objects: Vec<SchemaObject>,
}

fn canonical_schema_shape() -> Result<&'static SchemaShape> {
    static SHAPE: OnceLock<std::result::Result<SchemaShape, String>> = OnceLock::new();
    match SHAPE.get_or_init(load_canonical_schema_shape) {
        Ok(shape) => Ok(shape),
        Err(message) => Err(StoreError::Integrity(message.clone())),
    }
}

fn load_canonical_schema_shape() -> std::result::Result<SchemaShape, String> {
    let conn = Connection::open_in_memory().map_err(|err| err.to_string())?;
    conn.execute_batch(SCHEMA).map_err(|err| err.to_string())?;
    let names = schema_object_names(&conn, "table").map_err(|err| err.to_string())?;
    let mut tables = Vec::new();
    for name in names {
        let columns = table_columns(&conn, &name).map_err(|err| err.to_string())?;
        let foreign_keys = table_foreign_keys(&conn, &name).map_err(|err| err.to_string())?;
        let sql = schema_object_sql(&conn, "table", &name)
            .map_err(|err| err.to_string())?
            .ok_or_else(|| format!("canonical schema table {name} has no SQL"))?;
        tables.push(SchemaTable {
            name,
            columns,
            foreign_keys,
            sql,
        });
    }
    let mut objects = Vec::new();
    for kind in ["index", "trigger"] {
        for name in schema_object_names(&conn, kind).map_err(|err| err.to_string())? {
            let sql = schema_object_sql(&conn, kind, &name)
                .map_err(|err| err.to_string())?
                .ok_or_else(|| format!("canonical schema {kind} {name} has no SQL"))?;
            objects.push(SchemaObject {
                kind: kind.to_string(),
                name,
                sql,
            });
        }
    }
    Ok(SchemaShape { tables, objects })
}

fn schema_object_names(conn: &Connection, kind: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = ?1 AND name NOT LIKE 'sqlite_%' AND sql IS NOT NULL
         ORDER BY name",
    )?;
    let rows = stmt.query_map([kind], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
fn schema_object_exists(conn: &Connection, kind: &str, name: &str) -> Result<bool> {
    Ok(schema_object_sql(conn, kind, name)?.is_some())
}

fn schema_object_sql(conn: &Connection, kind: &str, name: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = ?1 AND name = ?2 AND sql IS NOT NULL
             LIMIT 1",
            (kind, name),
            |row| row.get(0),
        )
        .optional()?)
}

fn normalized_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn table_foreign_keys(conn: &Connection, table: &str) -> Result<Vec<SchemaForeignKey>> {
    let mut stmt = conn.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
    let rows = stmt.query_map([], |row| {
        Ok(SchemaForeignKey {
            id: row.get(0)?,
            sequence: row.get(1)?,
            target_table: row.get(2)?,
            source_column: row.get(3)?,
            target_column: row.get(4)?,
            on_update: row.get(5)?,
            on_delete: row.get(6)?,
            match_kind: row.get(7)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    schema_object_exists(conn, "table", table)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(table_columns(conn, table)?
        .iter()
        .any(|name| name == column))
}

fn set_user_version(conn: &Connection, version: i32) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
    Ok(())
}

fn migrate_v2_to_v3(conn: &Connection) -> Result<()> {
    if table_exists(conn, "session_state")? && !column_exists(conn, "session_state", "fast_mode")? {
        conn.execute_batch("ALTER TABLE session_state ADD COLUMN fast_mode INTEGER")?;
    }
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

fn migrate_v2_to_v3(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "legacy_alter_table", true)?;
    let result: Result<()> = (|| {
        conn.execute_batch(
            r#"
            DROP TRIGGER IF EXISTS transcript_search_ai;
            DROP TRIGGER IF EXISTS transcript_search_ad;
            DROP TRIGGER IF EXISTS transcript_search_au;
            DROP TABLE IF EXISTS transcript_search_fts;

            ALTER TABLE store_meta RENAME TO store_meta_v2;
            ALTER TABLE session_state RENAME TO session_state_v2;
            ALTER TABLE history_items RENAME TO history_items_v2;
            ALTER TABLE transcript_blocks RENAME TO transcript_blocks_v2;
            ALTER TABLE objects RENAME TO objects_v2;
            ALTER TABLE history_object_refs RENAME TO history_object_refs_v2;
            ALTER TABLE request_attempts RENAME TO request_attempts_v2;
            ALTER TABLE request_object_refs RENAME TO request_object_refs_v2;
            ALTER TABLE request_stats RENAME TO request_stats_v2;
            ALTER TABLE turn_metas RENAME TO turn_metas_v2;
            ALTER TABLE metadata_snapshots RENAME TO metadata_snapshots_v2;
            ALTER TABLE accounting_snapshots RENAME TO accounting_snapshots_v2;
            ALTER TABLE transcript_search RENAME TO transcript_search_v2;
            DROP TABLE IF EXISTS turn_tool_elapsed;
            "#,
        )?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(
            r#"
            INSERT INTO store_meta SELECT * FROM store_meta_v2;
            INSERT INTO session_state SELECT * FROM session_state_v2;
            INSERT INTO history_items (idx, kind, json, hash, search_text, created_at)
                SELECT idx, kind, json, hash, search_text, created_at FROM history_items_v2;
            INSERT INTO transcript_blocks (
                block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name,
                content_hash, estimated_text_bytes, estimated_rows, preview_text,
                descriptor_json, origin_json, tool_state_json
            )
                SELECT block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name,
                       content_hash, estimated_text_bytes, estimated_rows, preview_text,
                       descriptor_json, origin_json, tool_state_json
                FROM transcript_blocks_v2;
            INSERT INTO objects SELECT * FROM objects_v2;
            INSERT INTO history_object_refs SELECT * FROM history_object_refs_v2;
            INSERT INTO request_attempts SELECT * FROM request_attempts_v2;
            INSERT INTO request_object_refs SELECT * FROM request_object_refs_v2;
            INSERT INTO request_stats SELECT * FROM request_stats_v2;
            INSERT INTO turn_metas SELECT * FROM turn_metas_v2;
            INSERT INTO metadata_snapshots SELECT * FROM metadata_snapshots_v2;
            INSERT INTO accounting_snapshots SELECT * FROM accounting_snapshots_v2;
            INSERT INTO transcript_search SELECT * FROM transcript_search_v2;

            DROP TABLE transcript_search_v2;
            DROP TABLE accounting_snapshots_v2;
            DROP TABLE metadata_snapshots_v2;
            DROP TABLE turn_metas_v2;
            DROP TABLE request_stats_v2;
            DROP TABLE request_object_refs_v2;
            DROP TABLE request_attempts_v2;
            DROP TABLE history_object_refs_v2;
            DROP TABLE transcript_blocks_v2;
            DROP TABLE history_items_v2;
            DROP TABLE objects_v2;
            DROP TABLE session_state_v2;
            DROP TABLE store_meta_v2;
            "#,
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(())
    })();
    let restore = conn.pragma_update(None, "legacy_alter_table", false);
    result?;
    restore?;
    Ok(())
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS store_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (updated_at >= 0)
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
    fast_mode INTEGER,
    parent_id TEXT,
    accounting_json TEXT,
    checkpoint_json TEXT,
    context_tokens INTEGER CHECK (context_tokens IS NULL OR context_tokens >= 0),
    context_tokens_history_len INTEGER CHECK (context_tokens_history_len IS NULL OR context_tokens_history_len >= 0),
    display_context_tokens INTEGER CHECK (display_context_tokens IS NULL OR display_context_tokens >= 0),
    session_cost_usd REAL NOT NULL DEFAULT 0 CHECK (session_cost_usd >= 0),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    history_len INTEGER NOT NULL DEFAULT 0 CHECK (history_len >= 0),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (updated_at >= 0)
);

CREATE TABLE IF NOT EXISTS history_items (
    idx INTEGER PRIMARY KEY CHECK (idx >= 0),
    kind TEXT NOT NULL,
    json TEXT NOT NULL,
    hash TEXT NOT NULL,
    search_text TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (created_at >= 0)
);
CREATE INDEX IF NOT EXISTS history_items_kind_idx ON history_items(kind, idx);
CREATE INDEX IF NOT EXISTS history_items_created_at_idx ON history_items(created_at, idx);

CREATE TABLE IF NOT EXISTS transcript_blocks (
    block_idx INTEGER PRIMARY KEY CHECK (block_idx >= 0),
    descriptor_idx INTEGER CHECK (descriptor_idx IS NULL OR descriptor_idx >= 0),
    history_idx INTEGER REFERENCES history_items(idx) ON DELETE CASCADE CHECK (history_idx IS NULL OR history_idx >= 0),
    kind TEXT NOT NULL,
    tool_call_id TEXT,
    tool_name TEXT,
    content_hash TEXT,
    estimated_text_bytes INTEGER NOT NULL DEFAULT 0 CHECK (estimated_text_bytes >= 0),
    estimated_rows INTEGER CHECK (estimated_rows IS NULL OR estimated_rows >= 0),
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
    hash TEXT PRIMARY KEY CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    kind TEXT NOT NULL,
    codec TEXT NOT NULL CHECK (codec IN ('none', 'zstd')),
    raw_size INTEGER NOT NULL CHECK (raw_size >= 0),
    stored_size INTEGER NOT NULL CHECK (stored_size >= 0 AND stored_size = length(bytes)),
    bytes BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (created_at >= 0)
);
CREATE INDEX IF NOT EXISTS objects_kind_idx ON objects(kind, raw_size DESC);

CREATE TABLE IF NOT EXISTS history_object_refs (
    history_idx INTEGER NOT NULL REFERENCES history_items(idx) ON DELETE CASCADE CHECK (history_idx >= 0),
    object_hash TEXT NOT NULL REFERENCES objects(hash) ON DELETE RESTRICT,
    role TEXT NOT NULL CHECK (role <> ''),
    PRIMARY KEY (history_idx, object_hash, role)
);

CREATE TABLE IF NOT EXISTS request_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT,
    turn_id TEXT,
    ask_id TEXT,
    started_at INTEGER NOT NULL CHECK (started_at >= 0),
    completed_at INTEGER CHECK (completed_at IS NULL OR completed_at >= 0),
    provider TEXT,
    model TEXT,
    history_len INTEGER CHECK (history_len IS NULL OR history_len >= 0),
    body_hash TEXT REFERENCES objects(hash) ON DELETE RESTRICT,
    response_hash TEXT REFERENCES objects(hash) ON DELETE RESTRICT,
    error_hash TEXT REFERENCES objects(hash) ON DELETE RESTRICT,
    error_summary TEXT,
    background INTEGER NOT NULL DEFAULT 0 CHECK (background IN (0, 1)),
    raw_body_size INTEGER NOT NULL DEFAULT 0 CHECK (raw_body_size >= 0),
    kind TEXT,
    api_base TEXT,
    url TEXT,
    http_status INTEGER CHECK (http_status IS NULL OR http_status BETWEEN 100 AND 599),
    prompt_cache_key TEXT,
    stream INTEGER NOT NULL DEFAULT 0 CHECK (stream IN (0, 1)),
    attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
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
    request_attempt_id INTEGER NOT NULL REFERENCES request_attempts(id) ON DELETE CASCADE CHECK (request_attempt_id > 0),
    object_hash TEXT NOT NULL REFERENCES objects(hash) ON DELETE RESTRICT,
    role TEXT NOT NULL CHECK (role <> ''),
    PRIMARY KEY (request_attempt_id, object_hash, role)
);

CREATE TABLE IF NOT EXISTS request_stats (
    request_attempt_id INTEGER PRIMARY KEY REFERENCES request_attempts(id) ON DELETE CASCADE CHECK (request_attempt_id > 0),
    input_tokens INTEGER CHECK (input_tokens IS NULL OR input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens IS NULL OR output_tokens >= 0),
    cached_input_tokens INTEGER CHECK (cached_input_tokens IS NULL OR cached_input_tokens >= 0),
    reasoning_tokens INTEGER CHECK (reasoning_tokens IS NULL OR reasoning_tokens >= 0),
    total_cost_micros INTEGER CHECK (total_cost_micros IS NULL OR total_cost_micros >= 0),
    stats_json TEXT,
    context_tokens INTEGER CHECK (context_tokens IS NULL OR context_tokens >= 0),
    cache_write_tokens INTEGER CHECK (cache_write_tokens IS NULL OR cache_write_tokens >= 0),
    tokens_per_sec REAL CHECK (tokens_per_sec IS NULL OR tokens_per_sec >= 0)
);
CREATE INDEX IF NOT EXISTS request_stats_input_tokens_idx ON request_stats(input_tokens DESC);
CREATE INDEX IF NOT EXISTS request_stats_output_tokens_idx ON request_stats(output_tokens DESC);
CREATE INDEX IF NOT EXISTS request_stats_total_cost_idx ON request_stats(total_cost_micros DESC);

CREATE TABLE IF NOT EXISTS turn_metas (
    turn_idx INTEGER PRIMARY KEY CHECK (turn_idx >= 0),
    meta_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS metadata_snapshots (
    history_idx INTEGER PRIMARY KEY CHECK (history_idx >= 0),
    metadata_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (created_at >= 0)
);

CREATE TABLE IF NOT EXISTS accounting_snapshots (
    history_idx INTEGER PRIMARY KEY CHECK (history_idx >= 0),
    accounting_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (created_at >= 0)
);

CREATE TABLE IF NOT EXISTS transcript_search (
    block_idx INTEGER PRIMARY KEY REFERENCES transcript_blocks(block_idx) ON DELETE CASCADE CHECK (block_idx >= 0),
    history_idx INTEGER CHECK (history_idx IS NULL OR history_idx >= 0),
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
    fn v1_to_v3_migration_moves_search_text_and_removes_dead_schema() {
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
        assert!(!column_exists(&conn, "history_items", "model_visible_hash").unwrap());
        assert!(!column_exists(&conn, "transcript_blocks", "search_text").unwrap());
        assert!(!column_exists(&conn, "transcript_blocks", "sidecar_hash").unwrap());
        assert!(!table_exists(&conn, "transcript_search_terms").unwrap());
        assert!(!table_exists(&conn, "turn_tool_elapsed").unwrap());
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
    fn v2_to_v3_migration_preserves_data_and_removes_dead_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, "test").unwrap();
        conn.execute_batch(
            r#"
            INSERT INTO history_items (idx, kind, json, hash, search_text)
            VALUES (0, 'user', '{"kind":"user","content":"hello"}',
                    'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'hello');
            ALTER TABLE history_items ADD COLUMN model_visible_hash TEXT;
            ALTER TABLE transcript_blocks ADD COLUMN sidecar_hash TEXT;
            CREATE TABLE turn_tool_elapsed (
                turn_idx INTEGER NOT NULL,
                tool_call_id TEXT NOT NULL,
                elapsed_ms INTEGER NOT NULL,
                PRIMARY KEY (turn_idx, tool_call_id)
            );
            INSERT INTO session_state (
                singleton, id, revision, history_len, created_at, updated_at
            ) VALUES (1, 'session', 4, 1, 10, 20);
            INSERT INTO transcript_blocks (
                block_idx, descriptor_idx, history_idx, kind, content_hash,
                estimated_text_bytes, descriptor_json
            ) VALUES (0, 0, 0, 'user', 'content', 5, '{}');
            INSERT INTO transcript_search (block_idx, history_idx, indexed_text)
                VALUES (0, 0, 'hello');
            INSERT INTO objects (hash, kind, codec, raw_size, stored_size, bytes, created_at)
                VALUES (
                    'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                    'test', 'none', 1, 1, x'00', 30
                );
            INSERT INTO history_object_refs (history_idx, object_hash, role)
                VALUES (
                    0,
                    'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                    'test'
                );
            INSERT INTO request_attempts (
                id, request_id, started_at, body_hash, raw_body_size, attempt
            ) VALUES (
                1, 'request', 40,
                'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                1, 1
            );
            INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
                VALUES (
                    1,
                    'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                    'body'
                );
            INSERT INTO request_stats (request_attempt_id, input_tokens)
                VALUES (1, 7);
            INSERT INTO turn_metas (turn_idx, meta_json) VALUES (1, '{}');
            INSERT INTO metadata_snapshots (history_idx, metadata_json, created_at)
                VALUES (1, '{}', 50);
            INSERT INTO accounting_snapshots (history_idx, accounting_json, created_at)
                VALUES (1, '{}', 60);
            INSERT INTO turn_tool_elapsed (turn_idx, tool_call_id, elapsed_ms)
                VALUES (1, 'tool', 5);
            PRAGMA user_version = 2;
            "#,
        )
        .unwrap();

        migrate(&mut conn, "test-v3").unwrap();

        assert_eq!(user_version(&conn).unwrap(), 3);
        assert!(!column_exists(&conn, "history_items", "model_visible_hash").unwrap());
        assert!(!column_exists(&conn, "transcript_blocks", "sidecar_hash").unwrap());
        assert!(!table_exists(&conn, "turn_tool_elapsed").unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT search_text FROM history_items WHERE idx = 0",
                [],
                |row| { row.get::<_, String>(0) }
            )
            .unwrap(),
            "hello"
        );
        for (table, expected) in [
            ("session_state", 1),
            ("transcript_blocks", 1),
            ("transcript_search", 1),
            ("objects", 1),
            ("history_object_refs", 1),
            ("request_attempts", 1),
            ("request_object_refs", 1),
            ("request_stats", 1),
            ("turn_metas", 1),
            ("metadata_snapshots", 1),
            ("accounting_snapshots", 1),
        ] {
            let count = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap();
            assert_eq!(count, expected, "migration lost rows from {table}");
        }
        assert_eq!(
            conn.query_row(
                "SELECT rowid FROM transcript_search_fts WHERE indexed_text MATCH '\"hello\"'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn schema_checks_reject_negative_coordinates_and_inconsistent_objects() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, "test").unwrap();

        assert!(conn
            .execute(
                "INSERT INTO history_items (idx, kind, json, hash) VALUES (-1, 'user', '{}', 'bad')",
                [],
            )
            .is_err());
        assert!(conn
            .execute(
                "INSERT INTO objects (hash, kind, codec, raw_size, stored_size, bytes)
                 VALUES (?1, 'test', 'none', 1, 2, x'00')",
                ["a".repeat(64)],
            )
            .is_err());
    }

    #[test]
    fn read_only_validation_requires_indexes_and_triggers() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, "test").unwrap();
        conn.execute_batch("DROP INDEX history_items_kind_idx")
            .unwrap();
        let err = validate_read_only_schema(&conn).unwrap_err();
        assert!(err
            .to_string()
            .contains("missing index history_items_kind_idx"));

        conn.execute_batch("CREATE INDEX history_items_kind_idx ON history_items(idx)")
            .unwrap();
        let err = validate_read_only_schema(&conn).unwrap_err();
        assert!(err
            .to_string()
            .contains("definition differs for index history_items_kind_idx"));

        conn.execute_batch(
            "DROP INDEX history_items_kind_idx;
             CREATE INDEX IF NOT EXISTS history_items_kind_idx ON history_items(kind, idx);
             DROP TRIGGER transcript_search_ai;",
        )
        .unwrap();
        let err = validate_read_only_schema(&conn).unwrap_err();
        assert!(err
            .to_string()
            .contains("missing trigger transcript_search_ai"));
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
            "#,
        )
        .unwrap();
        set_user_version(&conn, SCHEMA_VERSION).unwrap();

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
            err.to_string().contains(&format!(
                "unsupported schema version {}",
                SCHEMA_VERSION + 1
            )),
            "{err}"
        );
    }
}
