use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::sync::OnceLock;

use crate::error::{Result, StoreError};

pub const SCHEMA_VERSION: i32 = 10;

pub(crate) const CANONICAL_CONTENT_TABLES: &[&str] = &[
    "session_state",
    "history_items",
    "turns",
    "transcript_blocks",
    "transcript_extent_chunks",
    "history_object_refs",
    "turn_metas",
    "metadata_snapshots",
    "accounting_snapshots",
    "transcript_search",
    "transcript_search_chars",
];

pub(crate) fn migrate(conn: &mut Connection, app_version: &str) -> Result<()> {
    let current = user_version(conn)?;
    let rebuild = (1..SCHEMA_VERSION).contains(&current);
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
        return finish_with_cleanup(
            "schema migration",
            result,
            conn.pragma_update(None, "foreign_keys", true)
                .map_err(Into::into),
        );
    }
    result
}

fn finish_with_cleanup(
    operation: &'static str,
    primary: Result<()>,
    cleanup: Result<()>,
) -> Result<()> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => Err(StoreError::OperationCleanup {
            operation,
            primary: Box::new(primary),
            cleanup: vec![cleanup],
        }),
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

fn migrate_inner(conn: &Connection, current: i32, app_version: &str) -> Result<()> {
    if current > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: current,
            expected: SCHEMA_VERSION,
        });
    }
    if current < 0 {
        return Err(StoreError::UnsupportedSchema {
            found: current,
            expected: SCHEMA_VERSION,
        });
    }
    let mut current = current;
    if current == 1 {
        migrate_v1_to_v2(conn)?;
        conn.execute_batch(LEGACY_SCHEMA_V4)?;
        current = 2;
    }
    if (2..=4).contains(&current) {
        migrate_to_v5(conn, current < 4)?;
    }
    if (1..=6).contains(&current) {
        migrate_to_v7(conn)?;
    }
    if (1..=7).contains(&current) {
        migrate_to_v8(conn)?;
    }
    if (1..=8).contains(&current) {
        migrate_to_v9(conn)?;
        current = 9;
    }
    if (1..=9).contains(&current) {
        migrate_to_v10(conn)?;
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
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" ,", ",")
        .replace(", ", ",")
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

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(table_columns(conn, table)?
        .iter()
        .any(|name| name == column))
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

fn migrate_to_v7(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "session_state", "descriptor_len")? {
        conn.execute_batch(
            "ALTER TABLE session_state
             ADD COLUMN descriptor_len INTEGER NOT NULL DEFAULT 0 CHECK (descriptor_len >= 0)",
        )?;
    }
    conn.execute(
        "UPDATE session_state
         SET descriptor_len = (
             SELECT COUNT(*) FROM transcript_blocks WHERE descriptor_json IS NOT NULL
         )
         WHERE singleton = 1",
        [],
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS transcript_search_chars (
             block_idx INTEGER PRIMARY KEY REFERENCES transcript_search(block_idx) ON DELETE CASCADE CHECK (block_idx >= 0),
             mask_0 INTEGER NOT NULL CHECK (mask_0 >= 0),
             mask_1 INTEGER NOT NULL CHECK (mask_1 >= 0),
             mask_2 INTEGER NOT NULL CHECK (mask_2 >= 0),
             mask_3 INTEGER NOT NULL CHECK (mask_3 >= 0)
         )",
    )?;
    let mut select = conn.prepare("SELECT block_idx, indexed_text FROM transcript_search")?;
    let mut insert = conn.prepare(
        "INSERT OR REPLACE INTO transcript_search_chars (
             block_idx, mask_0, mask_1, mask_2, mask_3
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut rows = select.query([])?;
    while let Some(row) = rows.next()? {
        let block_idx = row.get::<_, i64>(0)?;
        let indexed_text = row.get::<_, String>(1)?;
        let masks = crate::history::transcript_search_char_masks(&indexed_text);
        insert.execute((block_idx, masks[0], masks[1], masks[2], masks[3]))?;
    }
    Ok(())
}

fn migrate_to_v8(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "session_state", "next_turn_id")? {
        conn.execute_batch(
            "ALTER TABLE session_state
             ADD COLUMN next_turn_id INTEGER NOT NULL DEFAULT 1 CHECK (next_turn_id > 0)",
        )?;
    }
    Ok(())
}

fn migrate_to_v9(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DROP INDEX IF EXISTS transcript_blocks_descriptor_idx;
         ALTER TABLE session_state RENAME COLUMN descriptor_len TO transcript_record_count;
         ALTER TABLE transcript_blocks RENAME COLUMN descriptor_idx TO record_idx;
         ALTER TABLE transcript_blocks RENAME COLUMN descriptor_json TO block_json;",
    )?;
    Ok(())
}

fn migrate_to_v10(conn: &Connection) -> Result<()> {
    for (column, definition) in [
        (
            "extent_profile_version",
            "INTEGER NOT NULL DEFAULT 0 CHECK (extent_profile_version >= 0)",
        ),
        (
            "extent_rows_20",
            "INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_20 >= 0)",
        ),
        (
            "extent_rows_40",
            "INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_40 >= 0)",
        ),
        (
            "extent_rows_80",
            "INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_80 >= 0)",
        ),
        (
            "extent_rows_120",
            "INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_120 >= 0)",
        ),
        (
            "extent_rows_160",
            "INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_160 >= 0)",
        ),
        (
            "extent_rows_240",
            "INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_240 >= 0)",
        ),
    ] {
        if !column_exists(conn, "transcript_blocks", column)? {
            conn.execute_batch(&format!(
                "ALTER TABLE transcript_blocks ADD COLUMN {column} {definition}"
            ))?;
        }
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS transcript_extent_chunks (
             chunk_idx INTEGER PRIMARY KEY CHECK (chunk_idx >= 0),
             record_count INTEGER NOT NULL CHECK (record_count > 0),
             rows_20 INTEGER NOT NULL CHECK (rows_20 >= 0),
             rows_40 INTEGER NOT NULL CHECK (rows_40 >= 0),
             rows_80 INTEGER NOT NULL CHECK (rows_80 >= 0),
             rows_120 INTEGER NOT NULL CHECK (rows_120 >= 0),
             rows_160 INTEGER NOT NULL CHECK (rows_160 >= 0),
             rows_240 INTEGER NOT NULL CHECK (rows_240 >= 0)
         );",
    )?;
    crate::history::backfill_transcript_extent_profiles(conn)
}

fn migrate_to_v5(conn: &Connection, increment_attempt: bool) -> Result<()> {
    if !column_exists(conn, "session_state", "fast_mode")? {
        conn.execute_batch("ALTER TABLE session_state ADD COLUMN fast_mode INTEGER")?;
    }
    {
        let mut stmt =
            conn.prepare("SELECT DISTINCT role FROM history_object_refs ORDER BY role")?;
        let roles = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for role in roles {
            crate::history::HistoryObjectRole::from_str(&role?)?;
        }
    }
    conn.pragma_update(None, "legacy_alter_table", true)?;
    let result: Result<()> = (|| {
        conn.execute_batch(
            r#"
            DROP TRIGGER IF EXISTS transcript_search_ai;
            DROP TRIGGER IF EXISTS transcript_search_ad;
            DROP TRIGGER IF EXISTS transcript_search_au;
            DROP TABLE IF EXISTS transcript_search_fts;

            ALTER TABLE store_meta RENAME TO store_meta_legacy;
            ALTER TABLE session_state RENAME TO session_state_legacy;
            ALTER TABLE history_items RENAME TO history_items_legacy;
            ALTER TABLE transcript_blocks RENAME TO transcript_blocks_legacy;
            ALTER TABLE objects RENAME TO objects_legacy;
            ALTER TABLE history_object_refs RENAME TO history_object_refs_legacy;
            ALTER TABLE request_attempts RENAME TO request_attempts_legacy;
            ALTER TABLE request_object_refs RENAME TO request_object_refs_legacy;
            ALTER TABLE request_stats RENAME TO request_stats_legacy;
            ALTER TABLE turn_metas RENAME TO turn_metas_legacy;
            ALTER TABLE metadata_snapshots RENAME TO metadata_snapshots_legacy;
            ALTER TABLE accounting_snapshots RENAME TO accounting_snapshots_legacy;
            ALTER TABLE transcript_search RENAME TO transcript_search_legacy;
            DROP TABLE IF EXISTS turn_tool_elapsed;
            "#,
        )?;
        conn.execute_batch(&legacy_schema_v8())?;
        conn.execute_batch(
            r#"
            INSERT INTO store_meta SELECT * FROM store_meta_legacy;
            INSERT INTO session_state (
                singleton, id, title, slug, first_user_message, cwd, mode, reasoning_effort,
                model, fast_mode, parent_id, accounting_json, checkpoint_json, context_tokens,
                context_tokens_history_len, display_context_tokens, session_cost_usd, revision,
                history_len, created_at, updated_at
            )
                SELECT singleton, id, title, slug, first_user_message, cwd, mode,
                       reasoning_effort, model, fast_mode, parent_id, accounting_json,
                       checkpoint_json, context_tokens, context_tokens_history_len,
                       display_context_tokens, session_cost_usd, revision, history_len,
                       created_at, updated_at
                FROM session_state_legacy;
            INSERT INTO history_items (idx, kind, json, hash, search_text, created_at)
                SELECT idx, kind, json, hash, search_text, created_at FROM history_items_legacy;
            INSERT INTO transcript_blocks (
                block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name,
                content_hash, estimated_text_bytes, estimated_rows, preview_text,
                descriptor_json, origin_json, tool_state_json
            )
                SELECT block_idx, descriptor_idx, history_idx, kind, tool_call_id, tool_name,
                       content_hash, estimated_text_bytes, estimated_rows, preview_text,
                       descriptor_json, origin_json, tool_state_json
                FROM transcript_blocks_legacy;
            INSERT INTO objects (hash, codec, raw_size, stored_size, bytes)
                SELECT hash, codec, raw_size, stored_size, bytes FROM objects_legacy;
            INSERT INTO history_object_refs
                SELECT * FROM history_object_refs_legacy;
            INSERT INTO request_stats SELECT * FROM request_stats_legacy;
            INSERT INTO turn_metas SELECT * FROM turn_metas_legacy;
            INSERT INTO metadata_snapshots SELECT * FROM metadata_snapshots_legacy;
            INSERT INTO accounting_snapshots SELECT * FROM accounting_snapshots_legacy;
            -- COMPAT(storage-v2-wide-transcript-search): early v2 databases
            -- carried three byte-count columns that were never part of the
            -- canonical search row. Copy only the retained columns.
            INSERT INTO transcript_search (block_idx, history_idx, indexed_text)
                SELECT block_idx, history_idx, indexed_text FROM transcript_search_legacy;
            "#,
        )?;
        // COMPAT(request-audit-zero-based-attempts): v2/v3 producers wrote zero-based attempts.
        let attempt_adjustment = if increment_attempt { "+ 1" } else { "" };
        conn.execute_batch(&format!(
            r#"
            INSERT INTO request_attempts (
                id, request_id, turn_id, ask_id, started_at, completed_at, provider, model,
                history_len, error_summary, background, raw_body_size, kind, api_base, url,
                http_status, prompt_cache_key, stream, attempt, response_summary
            )
                SELECT id, request_id, turn_id, ask_id, started_at, completed_at, provider, model,
                       history_len, error_summary, background, raw_body_size, kind, api_base, url,
                       http_status, prompt_cache_key, stream, attempt {attempt_adjustment},
                       response_summary
                FROM request_attempts_legacy;
            "#
        ))?;
        crate::request_audit::migrate_legacy_request_refs(conn)?;
        conn.execute_batch(
            r#"
            DROP TABLE transcript_search_legacy;
            DROP TABLE accounting_snapshots_legacy;
            DROP TABLE metadata_snapshots_legacy;
            DROP TABLE turn_metas_legacy;
            DROP TABLE request_stats_legacy;
            DROP TABLE request_object_refs_legacy;
            DROP TABLE request_attempts_legacy;
            DROP TABLE history_object_refs_legacy;
            DROP TABLE transcript_blocks_legacy;
            DROP TABLE history_items_legacy;
            DROP TABLE objects_legacy;
            DROP TABLE session_state_legacy;
            DROP TABLE store_meta_legacy;
            "#,
        )?;
        validate_migrated_foreign_keys(conn)?;
        Ok(())
    })();
    finish_with_cleanup(
        "schema legacy_alter_table restoration",
        result,
        conn.pragma_update(None, "legacy_alter_table", false)
            .map_err(Into::into),
    )
}

fn validate_migrated_foreign_keys(conn: &Connection) -> Result<()> {
    let violation = conn
        .query_row(
            "SELECT * FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((table, rowid, parent, foreign_key)) = violation {
        return Err(StoreError::Integrity(format!(
            "migrated foreign key violation in {table} row {rowid:?}: constraint {foreign_key} references {parent}"
        )));
    }
    Ok(())
}

const LEGACY_SCHEMA_V4: &str = r#"
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
    fast_mode INTEGER,
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
CREATE TABLE IF NOT EXISTS objects (
    hash TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    codec TEXT NOT NULL,
    raw_size INTEGER NOT NULL,
    stored_size INTEGER NOT NULL,
    bytes BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
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
CREATE TABLE IF NOT EXISTS turn_metas (
    turn_idx INTEGER PRIMARY KEY,
    meta_json TEXT NOT NULL
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
"#;

fn legacy_schema_v8() -> String {
    legacy_schema_v9()
        .replace("transcript_record_count", "descriptor_len")
        .replace("record_idx", "descriptor_idx")
        .replace("block_json", "descriptor_json")
}

fn legacy_schema_v9() -> String {
    SCHEMA
        .replace(
            "    tool_state_json TEXT,\n    extent_profile_version INTEGER NOT NULL DEFAULT 0 CHECK (extent_profile_version >= 0),\n    extent_rows_20 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_20 >= 0),\n    extent_rows_40 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_40 >= 0),\n    extent_rows_80 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_80 >= 0),\n    extent_rows_120 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_120 >= 0),\n    extent_rows_160 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_160 >= 0),\n    extent_rows_240 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_240 >= 0)\n",
            "    tool_state_json TEXT\n",
        )
        .replace(
            "CREATE TABLE IF NOT EXISTS transcript_extent_chunks (\n    chunk_idx INTEGER PRIMARY KEY CHECK (chunk_idx >= 0),\n    record_count INTEGER NOT NULL CHECK (record_count > 0),\n    rows_20 INTEGER NOT NULL CHECK (rows_20 >= 0),\n    rows_40 INTEGER NOT NULL CHECK (rows_40 >= 0),\n    rows_80 INTEGER NOT NULL CHECK (rows_80 >= 0),\n    rows_120 INTEGER NOT NULL CHECK (rows_120 >= 0),\n    rows_160 INTEGER NOT NULL CHECK (rows_160 >= 0),\n    rows_240 INTEGER NOT NULL CHECK (rows_240 >= 0)\n);\n\n",
            "",
        )
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
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()) CHECK (updated_at >= 0),
    transcript_record_count INTEGER NOT NULL DEFAULT 0 CHECK (transcript_record_count >= 0),
    next_turn_id INTEGER NOT NULL DEFAULT 1 CHECK (next_turn_id > 0)
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

CREATE TABLE IF NOT EXISTS turns (
    turn_id INTEGER PRIMARY KEY CHECK (turn_id > 0),
    submitted_history_idx INTEGER NOT NULL CHECK (submitted_history_idx >= 0),
    submitted_history_hash TEXT NOT NULL
        CHECK (length(submitted_history_hash) = 64
               AND submitted_history_hash NOT GLOB '*[^0-9a-f]*'),
    submitted_revision INTEGER NOT NULL UNIQUE CHECK (submitted_revision > 0),
    kind TEXT NOT NULL CHECK (kind IN ('user', 'command', 'continuation', 'note')),
    state TEXT NOT NULL CHECK (
        state IN ('ready', 'running', 'completed', 'interrupted', 'failed', 'cancelled')
    ),
    continuation_of INTEGER REFERENCES turns(turn_id) ON DELETE RESTRICT,
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    started_at INTEGER CHECK (started_at IS NULL OR started_at >= created_at),
    finished_at INTEGER CHECK (finished_at IS NULL OR finished_at >= created_at),
    terminal_reason TEXT,
    CHECK (
        (state = 'ready' AND started_at IS NULL AND finished_at IS NULL)
        OR (state = 'running' AND started_at IS NOT NULL AND finished_at IS NULL)
        OR (state IN ('completed', 'interrupted', 'failed', 'cancelled')
            AND finished_at IS NOT NULL)
    )
);
CREATE INDEX IF NOT EXISTS turns_state_idx ON turns(state, turn_id);
CREATE INDEX IF NOT EXISTS turns_history_idx ON turns(submitted_history_idx, turn_id);

CREATE TABLE IF NOT EXISTS transcript_blocks (
    block_idx INTEGER PRIMARY KEY CHECK (block_idx >= 0),
    record_idx INTEGER CHECK (record_idx IS NULL OR record_idx >= 0),
    history_idx INTEGER REFERENCES history_items(idx) ON DELETE CASCADE CHECK (history_idx IS NULL OR history_idx >= 0),
    kind TEXT NOT NULL,
    tool_call_id TEXT,
    tool_name TEXT,
    content_hash TEXT,
    estimated_text_bytes INTEGER NOT NULL DEFAULT 0 CHECK (estimated_text_bytes >= 0),
    estimated_rows INTEGER CHECK (estimated_rows IS NULL OR estimated_rows >= 0),
    preview_text TEXT,
    block_json TEXT,
    origin_json TEXT,
    tool_state_json TEXT,
    extent_profile_version INTEGER NOT NULL DEFAULT 0 CHECK (extent_profile_version >= 0),
    extent_rows_20 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_20 >= 0),
    extent_rows_40 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_40 >= 0),
    extent_rows_80 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_80 >= 0),
    extent_rows_120 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_120 >= 0),
    extent_rows_160 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_160 >= 0),
    extent_rows_240 INTEGER NOT NULL DEFAULT 0 CHECK (extent_rows_240 >= 0)
);
CREATE INDEX IF NOT EXISTS transcript_blocks_history_idx ON transcript_blocks(history_idx, block_idx);
CREATE UNIQUE INDEX IF NOT EXISTS transcript_blocks_record_idx
    ON transcript_blocks(record_idx)
    WHERE block_json IS NOT NULL;
CREATE INDEX IF NOT EXISTS transcript_blocks_kind_idx
    ON transcript_blocks(kind, record_idx)
    WHERE block_json IS NOT NULL;
CREATE INDEX IF NOT EXISTS transcript_blocks_tool_call_id_idx ON transcript_blocks(tool_call_id);
CREATE INDEX IF NOT EXISTS transcript_blocks_extent_idx
    ON transcript_blocks(record_idx, kind, estimated_rows, estimated_text_bytes, preview_text)
    WHERE block_json IS NOT NULL;

CREATE TABLE IF NOT EXISTS transcript_extent_chunks (
    chunk_idx INTEGER PRIMARY KEY CHECK (chunk_idx >= 0),
    record_count INTEGER NOT NULL CHECK (record_count > 0),
    rows_20 INTEGER NOT NULL CHECK (rows_20 >= 0),
    rows_40 INTEGER NOT NULL CHECK (rows_40 >= 0),
    rows_80 INTEGER NOT NULL CHECK (rows_80 >= 0),
    rows_120 INTEGER NOT NULL CHECK (rows_120 >= 0),
    rows_160 INTEGER NOT NULL CHECK (rows_160 >= 0),
    rows_240 INTEGER NOT NULL CHECK (rows_240 >= 0)
);

CREATE TABLE IF NOT EXISTS objects (
    hash TEXT PRIMARY KEY CHECK (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*'),
    codec TEXT NOT NULL CHECK (codec IN ('none', 'zstd')),
    raw_size INTEGER NOT NULL CHECK (raw_size >= 0),
    stored_size INTEGER NOT NULL CHECK (stored_size >= 0 AND stored_size = length(bytes)),
    bytes BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS history_object_refs (
    history_idx INTEGER NOT NULL REFERENCES history_items(idx) ON DELETE CASCADE CHECK (history_idx >= 0),
    object_hash TEXT NOT NULL REFERENCES objects(hash) ON DELETE RESTRICT,
    role TEXT NOT NULL CHECK (role IN ('attachment_image', 'metadata')),
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
    role TEXT NOT NULL CHECK (role IN (
        'body_json', 'body_manifest', 'body_top', 'body_item', 'body_parent', 'response', 'error'
    )),
    PRIMARY KEY (request_attempt_id, object_hash, role)
);
CREATE UNIQUE INDEX IF NOT EXISTS request_object_refs_body_root_idx
    ON request_object_refs(request_attempt_id)
    WHERE role IN ('body_json', 'body_manifest');
CREATE UNIQUE INDEX IF NOT EXISTS request_object_refs_response_idx
    ON request_object_refs(request_attempt_id)
    WHERE role = 'response';
CREATE UNIQUE INDEX IF NOT EXISTS request_object_refs_error_idx
    ON request_object_refs(request_attempt_id)
    WHERE role = 'error';
CREATE INDEX IF NOT EXISTS request_object_refs_object_idx
    ON request_object_refs(object_hash, request_attempt_id);

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

CREATE TABLE IF NOT EXISTS transcript_search_chars (
    block_idx INTEGER PRIMARY KEY REFERENCES transcript_search(block_idx) ON DELETE CASCADE CHECK (block_idx >= 0),
    mask_0 INTEGER NOT NULL CHECK (mask_0 >= 0),
    mask_1 INTEGER NOT NULL CHECK (mask_1 >= 0),
    mask_2 INTEGER NOT NULL CHECK (mask_2 >= 0),
    mask_3 INTEGER NOT NULL CHECK (mask_3 >= 0)
);

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

    fn install_legacy_v4_schema(conn: &Connection) {
        conn.execute_batch(&legacy_schema_v8()).unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute_batch(
            "DROP TABLE request_stats;
             DROP TABLE request_object_refs;
             DROP TABLE request_attempts;
             DROP TABLE history_object_refs;
             DROP TABLE objects;",
        )
        .unwrap();
        conn.execute_batch(LEGACY_SCHEMA_V4).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
    }

    fn schema_v7() -> String {
        let mut schema = legacy_schema_v8().replace(
            ",\n    next_turn_id INTEGER NOT NULL DEFAULT 1 CHECK (next_turn_id > 0)",
            "",
        );
        let turns_start = schema
            .find("\nCREATE TABLE IF NOT EXISTS turns (")
            .expect("canonical turns table");
        let turns_end = schema
            .find("\nCREATE TABLE IF NOT EXISTS transcript_blocks (")
            .expect("canonical table after turns");
        schema.replace_range(turns_start..turns_end, "");
        schema
    }

    fn insert_legacy_object(conn: &Connection, kind: &str, bytes: &[u8]) -> String {
        let hash = crate::object::sha256_hex(bytes);
        conn.execute(
            "INSERT INTO objects (hash, kind, codec, raw_size, stored_size, bytes)
             VALUES (?1, ?2, 'none', ?3, ?3, ?4)",
            rusqlite::params![&hash, kind, bytes.len() as i64, bytes],
        )
        .unwrap();
        hash
    }

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
    fn v9_to_v10_migration_backfills_extent_profiles_and_chunks() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&legacy_schema_v9()).unwrap();
        assert!(!column_exists(&conn, "transcript_blocks", "extent_profile_version").unwrap());
        assert!(!table_exists(&conn, "transcript_extent_chunks").unwrap());

        for record_idx in 0..530i64 {
            let indexed_text = if record_idx % 2 == 0 {
                format!("record {record_idx}\nwith several hard lines\nand unicode 界")
            } else {
                format!("record {record_idx} {}", "wrapped content ".repeat(12))
            };
            conn.execute(
                "INSERT INTO transcript_blocks (
                     block_idx, record_idx, kind, estimated_text_bytes, preview_text, block_json
                 ) VALUES (?1, ?1, 'text', ?2, ?3, '{}')",
                rusqlite::params![record_idx, indexed_text.len() as i64, &indexed_text],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO transcript_search (block_idx, indexed_text) VALUES (?1, ?2)",
                rusqlite::params![record_idx, indexed_text],
            )
            .unwrap();
        }
        set_user_version(&conn, 9).unwrap();

        migrate(&mut conn, "test-v10").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM transcript_blocks
                 WHERE extent_profile_version = 1
                   AND extent_rows_20 > 0
                   AND extent_rows_240 > 0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            530
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*), SUM(record_count) FROM transcript_extent_chunks",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap(),
            (9, 530)
        );
        assert_eq!(
            conn.query_row(
                "SELECT record_count FROM transcript_extent_chunks WHERE chunk_idx = 8",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            18
        );
        validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn v1_to_v10_migration_moves_search_text_and_removes_dead_schema() {
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
    fn v2_wide_transcript_search_migration_preserves_data_and_removes_dead_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        install_legacy_v4_schema(&conn);
        conn.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        conn.execute_batch(
            r#"
            ALTER TABLE transcript_search ADD COLUMN indexed_text_bytes INTEGER;
            ALTER TABLE transcript_search ADD COLUMN full_text_bytes INTEGER;
            ALTER TABLE transcript_search ADD COLUMN omitted_text_bytes INTEGER;
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
                    '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',
                    'request_body', 'none', 2, 2, x'7b7d', 30
                );
            INSERT INTO history_object_refs (history_idx, object_hash, role)
                VALUES (
                    0,
                    '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',
                    'metadata'
                );
            INSERT INTO request_attempts (
                id, request_id, started_at, body_hash, raw_body_size, attempt
            ) VALUES
                (
                    1, 'request', 40,
                    '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',
                    1, 0
                ),
                (2, 'request', 41, NULL, 0, 1);
            INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
                VALUES (
                    1,
                    '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a',
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
        conn.pragma_update(None, "ignore_check_constraints", false)
            .unwrap();

        migrate(&mut conn, "test-v4").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
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
        assert_eq!(
            conn.query_row(
                "SELECT MIN(attempt), MAX(attempt) FROM request_attempts",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            )
            .unwrap(),
            (1, 2)
        );
        for (table, expected) in [
            ("session_state", 1),
            ("transcript_blocks", 1),
            ("transcript_search", 1),
            ("objects", 1),
            ("history_object_refs", 1),
            ("request_attempts", 2),
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
    fn v3_to_v10_migration_preserves_fast_mode_appended_by_v3() {
        let mut conn = Connection::open_in_memory().unwrap();
        install_legacy_v4_schema(&conn);
        conn.execute_batch(
            r#"
            ALTER TABLE session_state RENAME TO session_state_v4;
            CREATE TABLE session_state (
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
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                fast_mode INTEGER
            );
            INSERT INTO session_state (
                singleton, id, model, parent_id, session_cost_usd, revision, history_len,
                created_at, updated_at, fast_mode
            ) VALUES (1, 'session', 'model', 'parent', 1.5, 4, 7, 10, 20, 1);
            INSERT INTO request_attempts (id, started_at, attempt) VALUES (1, 30, 0);
            DROP TABLE session_state_v4;
            PRAGMA user_version = 3;
            "#,
        )
        .unwrap();

        migrate(&mut conn, "test-v4").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT id, fast_mode, parent_id, updated_at FROM session_state",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                }
            )
            .unwrap(),
            ("session".to_string(), true, "parent".to_string(), 20)
        );
        assert_eq!(
            conn.query_row("SELECT attempt FROM request_attempts", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
        validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn v4_to_v10_migration_backfills_typed_refs_and_removes_semantic_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        install_legacy_v4_schema(&conn);
        let hash = insert_legacy_object(&conn, "request_body", b"{}");
        conn.execute(
            "INSERT INTO request_attempts (id, started_at, body_hash) VALUES (1, 10, ?1)",
            [&hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
             VALUES (1, ?1, 'body')",
            [&hash],
        )
        .unwrap();
        set_user_version(&conn, 4).unwrap();

        migrate(&mut conn, "test-v5").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            table_columns(&conn, "objects").unwrap(),
            ["hash", "codec", "raw_size", "stored_size", "bytes"]
        );
        assert!(!column_exists(&conn, "request_attempts", "body_hash").unwrap());
        assert!(!column_exists(&conn, "request_attempts", "response_hash").unwrap());
        assert!(!column_exists(&conn, "request_attempts", "error_hash").unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT object_hash, role FROM request_object_refs WHERE request_attempt_id = 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            )
            .unwrap(),
            (hash, "body_json".to_string())
        );
        validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn v5_to_v10_migration_preserves_canonical_session_state() {
        let mut conn = Connection::open_in_memory().unwrap();
        install_legacy_v4_schema(&conn);
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        migrate_to_v5(&conn, false).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute(
            "INSERT INTO session_state (
                 singleton, id, title, revision, history_len, created_at, updated_at
             ) VALUES (1, 'session', 'kept', 3, 0, 10, 20)",
            [],
        )
        .unwrap();
        set_user_version(&conn, 5).unwrap();

        migrate(&mut conn, "test-v10").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT id, title, revision, history_len FROM session_state",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap(),
            ("session".to_string(), "kept".to_string(), 3, 0)
        );
        validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn v6_to_v10_migration_caches_transcript_record_count_without_rebuilding() {
        let mut conn = Connection::open_in_memory().unwrap();
        let v6_schema = schema_v7()
            .replace(
                ",\n    descriptor_len INTEGER NOT NULL DEFAULT 0 CHECK (descriptor_len >= 0)",
                "",
            )
            .replace(
                r#"
CREATE TABLE IF NOT EXISTS transcript_search_chars (
    block_idx INTEGER PRIMARY KEY REFERENCES transcript_search(block_idx) ON DELETE CASCADE CHECK (block_idx >= 0),
    mask_0 INTEGER NOT NULL CHECK (mask_0 >= 0),
    mask_1 INTEGER NOT NULL CHECK (mask_1 >= 0),
    mask_2 INTEGER NOT NULL CHECK (mask_2 >= 0),
    mask_3 INTEGER NOT NULL CHECK (mask_3 >= 0)
);
"#,
                "",
            );
        conn.execute_batch(&v6_schema).unwrap();
        conn.execute(
            "INSERT INTO store_meta (key, value) VALUES ('sentinel', 'kept')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_state (singleton, id) VALUES (1, 'session')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transcript_blocks (
                 block_idx, descriptor_idx, kind, descriptor_json
             ) VALUES (0, 0, 'text', '{}'), (1, 1, 'text', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transcript_search (block_idx, indexed_text)
             VALUES (0, 'contains §')",
            [],
        )
        .unwrap();
        set_user_version(&conn, 6).unwrap();

        migrate(&mut conn, "test-v7").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT value FROM store_meta WHERE key = 'sentinel'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "kept"
        );
        assert_eq!(
            conn.query_row(
                "SELECT transcript_record_count FROM session_state",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
        let masks = crate::history::transcript_search_char_masks("contains §");
        assert_eq!(
            conn.query_row(
                "SELECT mask_0, mask_1, mask_2, mask_3
                 FROM transcript_search_chars WHERE block_idx = 0",
                [],
                |row| Ok([
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ])
            )
            .unwrap(),
            masks
        );
        validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn v7_to_v10_migration_adds_empty_canonical_turn_state() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&schema_v7()).unwrap();
        conn.execute(
            "INSERT INTO session_state (
                 singleton, id, title, revision, history_len, descriptor_len, created_at, updated_at
             ) VALUES (1, 'session', 'kept', 4, 1, 1, 10, 20)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO history_items (idx, kind, json, hash, search_text, created_at)
             VALUES (
                 0, 'user', '{}',
                 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                 'hello', 15
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transcript_blocks (
                 block_idx, descriptor_idx, history_idx, kind, descriptor_json
             ) VALUES (0, 0, 0, 'user', '{}')",
            [],
        )
        .unwrap();
        set_user_version(&conn, 7).unwrap();
        assert!(!column_exists(&conn, "session_state", "next_turn_id").unwrap());
        assert!(!table_exists(&conn, "turns").unwrap());

        migrate(&mut conn, "test-v8").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT id, title, revision, history_len, transcript_record_count, next_turn_id
                 FROM session_state",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                }
            )
            .unwrap(),
            ("session".into(), "kept".into(), 4, 1, 1, 1)
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(schema_object_exists(&conn, "index", "turns_state_idx").unwrap());
        assert!(schema_object_exists(&conn, "index", "turns_history_idx").unwrap());
        validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn v8_to_v10_migration_renames_transcript_storage_vocabulary() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&legacy_schema_v8()).unwrap();
        conn.execute(
            "INSERT INTO session_state (
                 singleton, id, descriptor_len
             ) VALUES (1, 'session', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transcript_blocks (
                 block_idx, descriptor_idx, kind, descriptor_json
             ) VALUES (4, 0, 'text', '{\"kind\":\"text\"}')",
            [],
        )
        .unwrap();
        set_user_version(&conn, 8).unwrap();

        migrate(&mut conn, "test-v9").unwrap();

        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT transcript_record_count FROM session_state",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT record_idx, block_json FROM transcript_blocks WHERE block_idx = 4",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap(),
            (0, "{\"kind\":\"text\"}".to_string())
        );
        assert!(!column_exists(&conn, "session_state", "descriptor_len").unwrap());
        assert!(!column_exists(&conn, "transcript_blocks", "descriptor_idx").unwrap());
        assert!(!column_exists(&conn, "transcript_blocks", "descriptor_json").unwrap());
        assert!(schema_object_exists(&conn, "index", "transcript_blocks_record_idx").unwrap());
        assert!(!schema_object_exists(&conn, "index", "transcript_blocks_descriptor_idx").unwrap());
        validate_read_only_schema(&conn).unwrap();
    }

    #[test]
    fn migration_rejects_disagreement_between_payload_hashes_and_legacy_refs() {
        let mut conn = Connection::open_in_memory().unwrap();
        install_legacy_v4_schema(&conn);
        let body_hash = insert_legacy_object(&conn, "request_body", b"{}");
        let wrong_hash = insert_legacy_object(&conn, "request_body", b"[]");
        conn.execute(
            "INSERT INTO request_attempts (id, started_at, body_hash) VALUES (1, 10, ?1)",
            [&body_hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
             VALUES (1, ?1, 'body')",
            [&wrong_hash],
        )
        .unwrap();
        set_user_version(&conn, 4).unwrap();

        let err = migrate(&mut conn, "test-v5").unwrap_err();

        assert!(
            err.to_string().contains("disagree with payload hashes"),
            "{err}"
        );
        assert_eq!(user_version(&conn).unwrap(), 4);
        assert!(conn
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
            .unwrap());
    }

    #[test]
    fn migration_rejects_missing_legacy_request_references() {
        let mut conn = Connection::open_in_memory().unwrap();
        install_legacy_v4_schema(&conn);
        let body_hash = insert_legacy_object(&conn, "request_body", b"{}");
        conn.execute(
            "INSERT INTO request_attempts (id, started_at, body_hash) VALUES (1, 10, ?1)",
            [&body_hash],
        )
        .unwrap();
        set_user_version(&conn, 4).unwrap();

        let err = migrate(&mut conn, "test-v5").unwrap_err();

        assert!(
            err.to_string().contains("disagree with payload hashes")
                && err.to_string().contains("missing Some"),
            "{err}"
        );
        assert_eq!(user_version(&conn).unwrap(), 4);
    }

    #[test]
    fn migration_rejects_corrupt_objects_marked_as_manifests() {
        let mut conn = Connection::open_in_memory().unwrap();
        install_legacy_v4_schema(&conn);
        insert_legacy_object(&conn, "request_body_manifest", b"{}");
        set_user_version(&conn, 4).unwrap();

        let err = migrate(&mut conn, "test-v5").unwrap_err();

        assert!(
            err.to_string()
                .contains("marked request_body_manifest is invalid"),
            "{err}"
        );
        assert_eq!(user_version(&conn).unwrap(), 4);
    }

    #[test]
    fn migration_rejects_unknown_history_object_roles() {
        let mut conn = Connection::open_in_memory().unwrap();
        install_legacy_v4_schema(&conn);
        let hash = insert_legacy_object(&conn, "attachment_image", b"image");
        conn.execute(
            "INSERT INTO history_items (idx, kind, json, hash) VALUES (0, 'user', '{}', 'row')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO history_object_refs (history_idx, object_hash, role)
             VALUES (0, ?1, 'unknown')",
            [&hash],
        )
        .unwrap();
        set_user_version(&conn, 4).unwrap();

        let err = migrate(&mut conn, "test-v5").unwrap_err();

        assert!(
            err.to_string().contains("unknown history object role"),
            "{err}"
        );
        assert_eq!(user_version(&conn).unwrap(), 4);
    }

    #[test]
    fn cleanup_error_preserves_the_primary_migration_error() {
        let result = finish_with_cleanup(
            "schema migration",
            Err(StoreError::Integrity("primary migration failure".into())),
            Err(StoreError::Integrity("pragma restoration failure".into())),
        );

        let err = result.unwrap_err();
        let StoreError::OperationCleanup {
            operation,
            primary,
            cleanup,
        } = &err
        else {
            panic!("unexpected error: {err}");
        };
        assert_eq!(*operation, "schema migration");
        assert!(primary.to_string().contains("primary migration failure"));
        assert_eq!(cleanup.len(), 1);
        assert!(cleanup[0]
            .to_string()
            .contains("pragma restoration failure"));
        let message = err.to_string();
        assert!(message.contains("primary migration failure"));
        assert!(message.contains("pragma restoration failure"));
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
                "INSERT INTO objects (hash, codec, raw_size, stored_size, bytes)
                 VALUES (?1, 'none', 1, 2, x'00')",
                ["a".repeat(64)],
            )
            .is_err());

        let first =
            crate::object::put_object(&conn, b"{}", crate::compression::ObjectCompression::none())
                .unwrap()
                .hash()
                .to_string();
        let second =
            crate::object::put_object(&conn, b"[]", crate::compression::ObjectCompression::none())
                .unwrap()
                .hash()
                .to_string();
        conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
            .unwrap();
        assert!(conn
            .execute(
                "INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
                 VALUES (1, ?1, 'unknown')",
                [&first],
            )
            .is_err());
        conn.execute(
            "INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
             VALUES (1, ?1, 'body_json')",
            [&first],
        )
        .unwrap();
        assert!(conn
            .execute(
                "INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
                 VALUES (1, ?1, 'body_manifest')",
                [&second],
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
