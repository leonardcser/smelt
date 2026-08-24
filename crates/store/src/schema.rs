use std::sync::OnceLock;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::error::{Result, StoreError};

pub const LINEAGE_SCHEMA_VERSION: i32 = 2;
const PREVIOUS_LINEAGE_SCHEMA_VERSION: i32 = 1;

const LINEAGE_SCHEMA: &str = include_str!("lineage_schema.sql");
const LINEAGE_SCHEMA_V1_EXTENT_TABLE: &str = r#"
CREATE TABLE lineage_transcript_extent_chunks (
    lineage_id TEXT NOT NULL,
    transcript_root_id TEXT NOT NULL,
    chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
    record_count INTEGER NOT NULL CHECK (record_count BETWEEN 1 AND 64),
    rows_20 INTEGER NOT NULL CHECK (rows_20 >= record_count),
    rows_40 INTEGER NOT NULL CHECK (rows_40 >= record_count AND rows_40 <= rows_20),
    rows_80 INTEGER NOT NULL CHECK (rows_80 >= record_count AND rows_80 <= rows_40),
    rows_120 INTEGER NOT NULL CHECK (rows_120 >= record_count AND rows_120 <= rows_80),
    rows_160 INTEGER NOT NULL CHECK (rows_160 >= record_count AND rows_160 <= rows_120),
    rows_240 INTEGER NOT NULL CHECK (rows_240 >= record_count AND rows_240 <= rows_160),
    PRIMARY KEY (lineage_id, transcript_root_id, chunk_index),
    FOREIGN KEY (lineage_id, transcript_root_id)
        REFERENCES lineage_sequence_roots(lineage_id, root_id)
) STRICT;
"#;
const LINEAGE_SCHEMA_V2_INDEXES: &str = r#"
CREATE TABLE lineage_transcript_record_profiles (
    lineage_id TEXT NOT NULL,
    payload_id TEXT NOT NULL,
    block_idx INTEGER NOT NULL CHECK (block_idx >= 0),
    history_idx INTEGER CHECK (history_idx IS NULL OR history_idx >= 0),
    kind TEXT NOT NULL CHECK (kind IN (
        'user', 'mode', 'process_status', 'thinking', 'assistant',
        'code', 'tool', 'exec', 'compacted', 'compaction_preview'
    )),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'mode', 'process_status')),
    first_line TEXT NOT NULL CHECK (length(first_line) <= 512),
    estimated_text_bytes INTEGER NOT NULL CHECK (estimated_text_bytes >= 0),
    rows_20 INTEGER NOT NULL CHECK (rows_20 >= 1),
    rows_40 INTEGER NOT NULL CHECK (rows_40 >= 1 AND rows_40 <= rows_20),
    rows_80 INTEGER NOT NULL CHECK (rows_80 >= 1 AND rows_80 <= rows_40),
    rows_120 INTEGER NOT NULL CHECK (rows_120 >= 1 AND rows_120 <= rows_80),
    rows_160 INTEGER NOT NULL CHECK (rows_160 >= 1 AND rows_160 <= rows_120),
    rows_240 INTEGER NOT NULL CHECK (rows_240 >= 1 AND rows_240 <= rows_160),
    PRIMARY KEY (lineage_id, payload_id),
    FOREIGN KEY (lineage_id, payload_id)
        REFERENCES lineage_payload_object_refs(lineage_id, payload_id) ON DELETE CASCADE
) STRICT;
CREATE TABLE lineage_transcript_extent_nodes (
    lineage_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    record_count INTEGER NOT NULL CHECK (record_count > 0),
    first_block_idx INTEGER NOT NULL CHECK (first_block_idx >= 0),
    last_block_idx INTEGER NOT NULL CHECK (last_block_idx >= first_block_idx),
    kind_mask INTEGER NOT NULL CHECK (kind_mask > 0),
    role_mask INTEGER NOT NULL CHECK (role_mask > 0),
    rows_20 INTEGER NOT NULL CHECK (rows_20 >= record_count),
    rows_40 INTEGER NOT NULL CHECK (rows_40 >= record_count AND rows_40 <= rows_20),
    rows_80 INTEGER NOT NULL CHECK (rows_80 >= record_count AND rows_80 <= rows_40),
    rows_120 INTEGER NOT NULL CHECK (rows_120 >= record_count AND rows_120 <= rows_80),
    rows_160 INTEGER NOT NULL CHECK (rows_160 >= record_count AND rows_160 <= rows_120),
    rows_240 INTEGER NOT NULL CHECK (rows_240 >= record_count AND rows_240 <= rows_160),
    PRIMARY KEY (lineage_id, node_id),
    FOREIGN KEY (lineage_id, node_id)
        REFERENCES lineage_sequence_nodes(lineage_id, node_id) ON DELETE CASCADE
) STRICT;
CREATE INDEX lineage_transcript_profiles_block_idx
    ON lineage_transcript_record_profiles(lineage_id, block_idx, payload_id);
CREATE TRIGGER lineage_transcript_record_profile_update
BEFORE UPDATE ON lineage_transcript_record_profiles
BEGIN
    SELECT RAISE(ABORT, 'transcript record profiles are immutable');
END;
CREATE TRIGGER lineage_transcript_extent_node_update
BEFORE UPDATE ON lineage_transcript_extent_nodes
BEGIN
    SELECT RAISE(ABORT, 'transcript extent nodes are immutable');
END;
"#;

pub(crate) fn initialize_lineage_schema(conn: &mut Connection) -> Result<()> {
    let version = user_version(conn)?;
    match version {
        0 => {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(LINEAGE_SCHEMA)?;
            set_user_version(&tx, LINEAGE_SCHEMA_VERSION)?;
            write_store_meta(&tx)?;
            tx.commit()?;
        }
        PREVIOUS_LINEAGE_SCHEMA_VERSION => migrate_lineage_schema_v1_to_v2(conn)?,
        LINEAGE_SCHEMA_VERSION => {}
        found => {
            return Err(StoreError::UnsupportedSchema {
                found,
                expected: LINEAGE_SCHEMA_VERSION,
            });
        }
    }
    validate_lineage_schema(conn)
}

fn migrate_lineage_schema_v1_to_v2(conn: &mut Connection) -> Result<()> {
    validate_schema_shape(conn, canonical_lineage_schema_v1_shape()?)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(LINEAGE_SCHEMA_V2_INDEXES)?;
    crate::lineage::backfill_transcript_indexes(&tx)?;
    tx.execute_batch("DROP TABLE lineage_transcript_extent_chunks")?;
    let updated_schema = tx.execute(
        "UPDATE store_meta SET value = ?1, updated_at = unixepoch()
         WHERE key = 'schema_version'",
        [LINEAGE_SCHEMA_VERSION.to_string()],
    )?;
    let updated_app = tx.execute(
        "UPDATE store_meta SET value = ?1, updated_at = unixepoch()
         WHERE key = 'app_version'",
        [env!("CARGO_PKG_VERSION")],
    )?;
    if updated_schema != 1 || updated_app != 1 {
        return Err(StoreError::Integrity(
            "lineage schema migration found invalid store metadata".into(),
        ));
    }
    set_user_version(&tx, LINEAGE_SCHEMA_VERSION)?;
    validate_lineage_schema(&tx)?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn validate_lineage_schema(conn: &Connection) -> Result<()> {
    let version = user_version(conn)?;
    if version != LINEAGE_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            expected: LINEAGE_SCHEMA_VERSION,
        });
    }
    validate_schema_shape(conn, canonical_lineage_schema_shape()?)
}

pub(crate) fn user_version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn write_store_meta(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO store_meta (key, value, updated_at)
         VALUES ('schema_version', ?1, unixepoch()), ('app_version', ?2, unixepoch())",
        (
            LINEAGE_SCHEMA_VERSION.to_string(),
            env!("CARGO_PKG_VERSION"),
        ),
    )?;
    Ok(())
}

fn set_user_version(conn: &Connection, version: i32) -> Result<()> {
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))?;
    Ok(())
}

fn validate_schema_shape(conn: &Connection, shape: &SchemaShape) -> Result<()> {
    for table in &shape.tables {
        let Some(actual_sql) = schema_object_sql(conn, "table", &table.name)? else {
            return Err(StoreError::Integrity(format!(
                "sqlite schema missing table {}",
                table.name
            )));
        };
        let actual_columns = table_columns(conn, &table.name)?;
        if actual_columns != table.columns {
            return Err(StoreError::Integrity(format!(
                "sqlite schema columns differ for {}: expected {:?}, found {:?}",
                table.name, table.columns, actual_columns
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

fn canonical_lineage_schema_shape() -> Result<&'static SchemaShape> {
    static SHAPE: OnceLock<std::result::Result<SchemaShape, String>> = OnceLock::new();
    match SHAPE.get_or_init(load_canonical_lineage_schema_shape) {
        Ok(shape) => Ok(shape),
        Err(message) => Err(StoreError::Integrity(message.clone())),
    }
}

fn canonical_lineage_schema_v1_shape() -> Result<&'static SchemaShape> {
    static SHAPE: OnceLock<std::result::Result<SchemaShape, String>> = OnceLock::new();
    match SHAPE.get_or_init(load_canonical_lineage_schema_v1_shape) {
        Ok(shape) => Ok(shape),
        Err(message) => Err(StoreError::Integrity(message.clone())),
    }
}

fn load_canonical_lineage_schema_v1_shape() -> std::result::Result<SchemaShape, String> {
    let conn = Connection::open_in_memory().map_err(|error| error.to_string())?;
    conn.execute_batch(LINEAGE_SCHEMA)
        .map_err(|error| error.to_string())?;
    conn.execute_batch(
        "DROP TRIGGER lineage_transcript_extent_node_update;
         DROP TRIGGER lineage_transcript_record_profile_update;
         DROP INDEX lineage_transcript_profiles_block_idx;
         DROP TABLE lineage_transcript_extent_nodes;
         DROP TABLE lineage_transcript_record_profiles;",
    )
    .map_err(|error| error.to_string())?;
    conn.execute_batch(LINEAGE_SCHEMA_V1_EXTENT_TABLE)
        .map_err(|error| error.to_string())?;
    load_schema_shape(&conn)
}

fn load_canonical_lineage_schema_shape() -> std::result::Result<SchemaShape, String> {
    let conn = Connection::open_in_memory().map_err(|error| error.to_string())?;
    conn.execute_batch(LINEAGE_SCHEMA)
        .map_err(|error| error.to_string())?;
    load_schema_shape(&conn)
}

fn load_schema_shape(conn: &Connection) -> std::result::Result<SchemaShape, String> {
    let names = schema_object_names(conn, "table").map_err(|error| error.to_string())?;
    let mut tables = Vec::with_capacity(names.len());
    for name in names {
        let columns = table_columns(conn, &name).map_err(|error| error.to_string())?;
        let foreign_keys = table_foreign_keys(conn, &name).map_err(|error| error.to_string())?;
        let sql = schema_object_sql(conn, "table", &name)
            .map_err(|error| error.to_string())?
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
        for name in schema_object_names(conn, kind).map_err(|error| error.to_string())? {
            let sql = schema_object_sql(conn, kind, &name)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("canonical schema {kind} {name} has no SQL"))?;
            objects.push(SchemaObject {
                kind: kind.to_owned(),
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

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populate_transcript(conn: &Connection) {
        let lineage = crate::lineage::LineageId::from_hex("1".repeat(32)).unwrap();
        crate::lineage::create_lineage(conn, &lineage, 1).unwrap();
        let root = crate::lineage::empty_sequence(
            conn,
            &lineage,
            crate::lineage::SequenceKind::Transcript,
        )
        .unwrap();
        let record = crate::StoredTranscriptBlock {
            block_idx: 7,
            history_idx: Some(3),
            kind: "assistant".into(),
            tool_call_id: None,
            tool_name: None,
            content_hash: "a".repeat(64),
            estimated_text_bytes: 11,
            preview_text: "hello world".into(),
            indexed_text: "hello world".into(),
            block_json: "{}".into(),
            origin_json: None,
            tool_state_json: None,
            tool_render_revision: 0,
        };
        let mut second = record.clone();
        second.block_idx = 9;
        second.content_hash = "b".repeat(64);
        second.preview_text = "goodbye world".into();
        second.indexed_text = "goodbye world".into();
        crate::lineage::append_sequence_in(
            conn,
            &lineage,
            &root,
            &[
                serde_json::to_vec(&record).unwrap(),
                serde_json::to_vec(&second).unwrap(),
            ],
            crate::compression::ObjectCompression::none(),
        )
        .unwrap();
    }

    fn downgrade_to_v1(conn: &Connection) {
        conn.execute_batch(
            "DROP TRIGGER lineage_transcript_extent_node_update;
             DROP TRIGGER lineage_transcript_record_profile_update;
             DROP INDEX lineage_transcript_profiles_block_idx;
             DROP TABLE lineage_transcript_extent_nodes;
             DROP TABLE lineage_transcript_record_profiles;",
        )
        .unwrap();
        conn.execute_batch(LINEAGE_SCHEMA_V1_EXTENT_TABLE).unwrap();
        conn.execute(
            "UPDATE store_meta SET value = '1' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
        set_user_version(conn, PREVIOUS_LINEAGE_SCHEMA_VERSION).unwrap();
        validate_schema_shape(conn, canonical_lineage_schema_v1_shape().unwrap()).unwrap();
    }

    #[test]
    fn creates_and_validates_lineage_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_lineage_schema(&mut conn).unwrap();

        assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
        validate_lineage_schema(&conn).unwrap();
        assert!(schema_object_sql(&conn, "table", "lineage_branches")
            .unwrap()
            .is_some());
        assert!(schema_object_sql(&conn, "table", "session_state")
            .unwrap()
            .is_none());
        assert!(schema_object_sql(&conn, "table", "transcript_search")
            .unwrap()
            .is_none());
        assert!(
            schema_object_sql(&conn, "table", "lineage_transcript_record_profiles")
                .unwrap()
                .is_some()
        );
        assert!(
            schema_object_sql(&conn, "table", "lineage_transcript_extent_chunks")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn migrates_populated_v1_lineage_indexes_transactionally() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_lineage_schema(&mut conn).unwrap();
        populate_transcript(&conn);
        downgrade_to_v1(&conn);

        initialize_lineage_schema(&mut conn).unwrap();

        assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM lineage_transcript_record_profiles",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
        assert!(
            conn.query_row(
                "SELECT COUNT(*) FROM lineage_transcript_extent_nodes",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
                > 0
        );
        assert!(
            schema_object_sql(&conn, "table", "lineage_transcript_extent_chunks")
                .unwrap()
                .is_none()
        );
        validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn rolls_back_v1_migration_when_payload_content_is_corrupt() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_lineage_schema(&mut conn).unwrap();
        populate_transcript(&conn);
        downgrade_to_v1(&conn);
        let corrupt_object = conn
            .query_row(
                "SELECT object_hash
                 FROM lineage_payload_object_refs
                 WHERE payload_kind = 'transcript'
                 ORDER BY payload_id
                 LIMIT 1 OFFSET 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
            [corrupt_object],
        )
        .unwrap();

        assert!(initialize_lineage_schema(&mut conn).is_err());
        assert_eq!(
            user_version(&conn).unwrap(),
            PREVIOUS_LINEAGE_SCHEMA_VERSION
        );
        assert!(
            schema_object_sql(&conn, "table", "lineage_transcript_extent_chunks")
                .unwrap()
                .is_some()
        );
        assert!(
            schema_object_sql(&conn, "table", "lineage_transcript_record_profiles")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_unknown_schema_versions_without_mutation() {
        let mut conn = Connection::open_in_memory().unwrap();
        set_user_version(&conn, LINEAGE_SCHEMA_VERSION + 1).unwrap();

        assert!(matches!(
            initialize_lineage_schema(&mut conn),
            Err(StoreError::UnsupportedSchema { found, expected })
                if found == LINEAGE_SCHEMA_VERSION + 1 && expected == LINEAGE_SCHEMA_VERSION
        ));
        assert!(schema_object_names(&conn, "table").unwrap().is_empty());
    }

    #[test]
    fn rejects_shape_drift() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_lineage_schema(&mut conn).unwrap();
        conn.execute_batch("DROP INDEX lineage_branches_updated_idx")
            .unwrap();

        assert!(matches!(
            validate_lineage_schema(&conn),
            Err(StoreError::Integrity(message))
                if message.contains("lineage_branches_updated_idx")
        ));
    }
}
