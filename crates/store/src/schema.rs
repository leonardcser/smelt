use std::sync::OnceLock;

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Result, StoreError};

pub const LINEAGE_SCHEMA_VERSION: i32 = 5;

const LINEAGE_SCHEMA: &str = include_str!("lineage_schema.sql");
// COMPAT(lineage-schema-v3): retained databases migrate atomically; reads remain write-free.
const LINEAGE_SCHEMA_V3: &str = include_str!("lineage_v3.sql");
// COMPAT(lineage-schema-v4): retained databases keep their original response roles.
const LINEAGE_SCHEMA_V4: &str = include_str!("lineage_v4.sql");

// COMPAT(lineage-schema-v3): shared storage was introduced in v4.
pub(crate) fn has_shared_storage(conn: &Connection) -> Result<bool> {
    Ok(user_version(conn)? >= 4)
}

pub(crate) fn initialize_lineage_schema(conn: &mut Connection) -> Result<()> {
    if user_version(conn)? == LINEAGE_SCHEMA_VERSION {
        return validate_lineage_schema(conn);
    }
    // Table replacement requires foreign keys off outside the write transaction.
    // Validate every foreign key before publication and restore the caller's mode
    // after either commit or rollback.
    let foreign_keys: bool = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = (|| {
        if conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))? {
            return Err(StoreError::Integrity(
                "schema migration requires a top-level transaction".into(),
            ));
        }
        // Another branch may initialize this database while we wait.
        let tx = crate::write_transaction::begin_write(conn, "initialize schema")?;
        match user_version(&tx)? {
            0 => {
                tx.execute_batch(LINEAGE_SCHEMA)?;
                set_user_version(&tx, LINEAGE_SCHEMA_VERSION)?;
                write_store_meta(&tx)?;
            }
            3 => {
                validate_schema_shape(&tx, lineage_schema_shape(3)?)?;
                migrate_lineage_schema_v3(&tx)?;
                set_user_version(&tx, LINEAGE_SCHEMA_VERSION)?;
                write_store_meta(&tx)?;
            }
            4 => {
                validate_schema_shape(&tx, lineage_schema_shape(4)?)?;
                migrate_lineage_schema_v4(&tx)?;
                set_user_version(&tx, LINEAGE_SCHEMA_VERSION)?;
                write_store_meta(&tx)?;
            }
            LINEAGE_SCHEMA_VERSION => {}
            found => {
                return Err(StoreError::UnsupportedSchema {
                    found,
                    expected: LINEAGE_SCHEMA_VERSION,
                });
            }
        }
        validate_lineage_schema(&tx)?;
        let violation = tx
            .prepare("PRAGMA foreign_key_check")?
            .query_map([], |row| row.get::<_, String>(0))?
            .next()
            .transpose()?;
        if let Some(table) = violation {
            return Err(StoreError::Integrity(format!(
                "schema migration encountered a foreign key violation in {table}"
            )));
        }
        tx.commit()?;
        Ok(())
    })();
    conn.pragma_update(None, "foreign_keys", foreign_keys)?;
    result
}

pub(crate) fn validate_lineage_schema(conn: &Connection) -> Result<()> {
    let version = user_version(conn)?;
    if !matches!(version, 3 | 4 | LINEAGE_SCHEMA_VERSION) {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            expected: LINEAGE_SCHEMA_VERSION,
        });
    }
    validate_schema_shape(conn, lineage_schema_shape(version)?)
}

fn migrate_lineage_schema_v3(conn: &Connection) -> Result<()> {
    let legacy = lineage_schema_shape(3)?;
    let current = lineage_schema_shape(LINEAGE_SCHEMA_VERSION)?;
    let tables = [
        "lineage_payload_object_refs",
        "lineage_sequence_nodes",
        "lineage_sequence_roots",
        "request_object_refs",
    ];
    let guards = [
        "lineage_sequence_entry_insert",
        "lineage_sequence_root_insert",
    ];
    let mut migration = String::new();
    for name in tables {
        migration.push_str(&replacement_table_sql(current, name)?);
    }
    for table in &current.tables {
        if !legacy.tables.iter().any(|old| old.name == table.name) {
            migration.push_str(&table.sql);
            migration.push_str(";\n");
        }
    }
    for object in &current.objects {
        if guards.contains(&object.name.as_str())
            || !legacy.objects.iter().any(|old| old.name == object.name)
        {
            migration.push_str(&object.sql);
            migration.push_str(";\n");
        }
    }
    replace_lineage_tables(conn, &migration, &tables, &guards)?;
    backfill_completed_sequence_nodes(conn)?;
    crate::lineage::backfill_history_indexes(conn)
}

fn migrate_lineage_schema_v4(conn: &Connection) -> Result<()> {
    let tables = ["request_object_refs"];
    let migration =
        replacement_table_sql(lineage_schema_shape(LINEAGE_SCHEMA_VERSION)?, tables[0])?;
    replace_lineage_tables(conn, &migration, &tables, &[])
}

fn replacement_table_sql(shape: &SchemaShape, name: &str) -> Result<String> {
    let table = shape
        .tables
        .iter()
        .find(|table| table.name == name)
        .ok_or_else(|| StoreError::Integrity(format!("canonical schema missing table {name}")))?;
    let definition = table
        .sql
        .strip_prefix(&format!("CREATE TABLE \"{name}\""))
        .ok_or_else(|| StoreError::Integrity(format!("cannot stage canonical table {name}")))?;
    Ok(format!(
        "CREATE TABLE \"{name}_migration\"{definition};
         INSERT INTO \"{name}_migration\" SELECT * FROM \"{name}\";
         DROP TABLE \"{name}\";
         ALTER TABLE \"{name}_migration\" RENAME TO \"{name}\";\n"
    ))
}

fn backfill_completed_sequence_nodes(conn: &Connection) -> Result<()> {
    let levels = conn
        .prepare("SELECT DISTINCT level FROM lineage_sequence_nodes ORDER BY level")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for level in levels {
        conn.execute(
            "INSERT INTO lineage_completed_sequence_nodes (lineage_id, node_id)
             SELECT node.lineage_id, node.node_id FROM lineage_sequence_nodes node
             WHERE node.level = ?1
               AND node.entry_count = (
                   SELECT count(*) FROM lineage_sequence_entries entry
                   WHERE entry.lineage_id = node.lineage_id AND entry.node_id = node.node_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lineage_sequence_entries entry
                   WHERE entry.lineage_id = node.lineage_id AND entry.node_id = node.node_id
                     AND entry.entry_kind = 'child'
                     AND NOT EXISTS (
                         SELECT 1 FROM lineage_completed_sequence_nodes complete
                         WHERE complete.lineage_id = entry.lineage_id
                           AND complete.node_id = entry.child_node_id
                     )
               )",
            [level],
        )?;
    }
    let incomplete_root = conn
        .query_row(
            "SELECT root.root_id FROM lineage_sequence_roots root
             WHERE root.root_node_id IS NOT NULL AND NOT EXISTS (
                 SELECT 1 FROM lineage_completed_sequence_nodes complete
                 WHERE complete.lineage_id = root.lineage_id
                   AND complete.node_id = root.root_node_id
             ) LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(root) = incomplete_root {
        return Err(StoreError::Integrity(format!(
            "cannot migrate incomplete sequence root {root}"
        )));
    }
    Ok(())
}

fn replace_lineage_tables(
    conn: &Connection,
    migration: &str,
    tables: &[&str],
    replaced_objects: &[&str],
) -> Result<()> {
    // Capture installed guards rather than reconstructing them from source.
    // All triggers must be suspended while table names are temporarily absent.
    let objects = conn
        .prepare(
            "SELECT type, name, sql, tbl_name FROM sqlite_schema
             WHERE sql IS NOT NULL AND type IN ('trigger', 'index')
             ORDER BY type, name",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let objects: Vec<_> = objects
        .into_iter()
        .filter(|(kind, _, _, table)| kind == "trigger" || tables.contains(&table.as_str()))
        .collect();
    for (kind, name, _, _) in &objects {
        if kind == "trigger" {
            let name = name.replace('"', "\"\"");
            conn.execute_batch(&format!("DROP TRIGGER \"{name}\""))?;
        }
    }
    conn.execute_batch(migration)?;
    for (_, name, sql, _) in objects {
        if !replaced_objects.contains(&name.as_str()) {
            conn.execute_batch(&sql)?;
        }
    }
    Ok(())
}

pub(crate) fn user_version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn write_store_meta(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO store_meta (key, value, updated_at)
         VALUES ('schema_version', ?1, unixepoch()), ('app_version', ?2, unixepoch())
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
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

fn lineage_schema_shape(version: i32) -> Result<&'static SchemaShape> {
    static LEGACY: OnceLock<std::result::Result<SchemaShape, String>> = OnceLock::new();
    static V4: OnceLock<std::result::Result<SchemaShape, String>> = OnceLock::new();
    static CURRENT: OnceLock<std::result::Result<SchemaShape, String>> = OnceLock::new();
    let cache = match version {
        3 => &LEGACY,
        4 => &V4,
        LINEAGE_SCHEMA_VERSION => &CURRENT,
        found => {
            return Err(StoreError::UnsupportedSchema {
                found,
                expected: LINEAGE_SCHEMA_VERSION,
            });
        }
    };
    match cache.get_or_init(|| load_lineage_schema_shape(version)) {
        Ok(shape) => Ok(shape),
        Err(message) => Err(StoreError::Integrity(message.clone())),
    }
}

fn load_lineage_schema_shape(version: i32) -> std::result::Result<SchemaShape, String> {
    let conn = Connection::open_in_memory().map_err(|error| error.to_string())?;
    conn.pragma_update(None, "foreign_keys", false)
        .map_err(|error| error.to_string())?;
    let sql = match version {
        3 => LINEAGE_SCHEMA_V3,
        4 => LINEAGE_SCHEMA_V4,
        LINEAGE_SCHEMA_VERSION => LINEAGE_SCHEMA,
        found => return Err(format!("unsupported canonical schema version {found}")),
    };
    conn.execute_batch(sql).map_err(|error| error.to_string())?;
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
pub(crate) mod tests {
    use super::*;
    use rusqlite::TransactionBehavior;

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

    pub(crate) fn v3_connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute_batch(LINEAGE_SCHEMA_V3).unwrap();
        set_user_version(&conn, 3).unwrap();
        conn.execute(
            "INSERT INTO store_meta (key, value, updated_at) VALUES ('schema_version', '3', 1)",
            [],
        )
        .unwrap();
        conn
    }

    pub(crate) fn v4_connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute_batch(LINEAGE_SCHEMA_V4).unwrap();
        set_user_version(&conn, 4).unwrap();
        conn.execute(
            "INSERT INTO store_meta (key, value, updated_at) VALUES ('schema_version', '4', 1)",
            [],
        )
        .unwrap();
        conn
    }

    fn current_connection() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        initialize_lineage_schema(&mut conn).unwrap();
        conn
    }

    fn schema_definitions(conn: &Connection) -> Vec<(String, String, String)> {
        let shape = load_schema_shape(conn).unwrap();
        let mut definitions = shape
            .tables
            .into_iter()
            .map(|table| ("table".into(), table.name, table.sql))
            .chain(
                shape
                    .objects
                    .into_iter()
                    .map(|object| (object.kind, object.name, object.sql)),
            )
            .collect::<Vec<_>>();
        definitions.sort();
        definitions
    }

    fn migration_boundaries() -> Vec<String> {
        let legacy = lineage_schema_shape(3).unwrap();
        let current = lineage_schema_shape(LINEAGE_SCHEMA_VERSION).unwrap();
        let conn = v3_connection();
        let mut boundaries = Vec::new();
        for name in [
            "lineage_payload_object_refs",
            "lineage_sequence_nodes",
            "lineage_sequence_roots",
            "request_object_refs",
        ] {
            boundaries.extend([
                format!("table:{name}_migration"),
                format!("drop:{name}"),
                format!("rename:{name}_migration"),
            ]);
            for index in conn.prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL"
            ).unwrap().query_map([name], |row| row.get::<_, String>(0)).unwrap() {
                boundaries.push(format!("index:{}", index.unwrap()));
            }
        }
        for table in &current.tables {
            if !legacy.tables.iter().any(|old| old.name == table.name) {
                boundaries.push(format!("table:{}", table.name));
            }
        }
        for object in &current.objects {
            if object.kind == "trigger" || !legacy.objects.iter().any(|old| old.name == object.name)
            {
                boundaries.push(format!("{}:{}", object.kind, object.name));
            }
        }
        boundaries.extend([
            "completed_nodes".into(),
            "history_index".into(),
            "published".into(),
            "store_meta".into(),
            "foreign_key_check".into(),
            "commit".into(),
        ]);
        boundaries.sort();
        let count = boundaries.len();
        boundaries.dedup();
        assert_eq!(boundaries.len(), count, "duplicate migration boundary");
        assert_eq!(current.tables.len() + current.objects.len(), 136);
        assert_eq!(legacy.tables.len() + legacy.objects.len(), 76);
        boundaries
    }

    fn migration_boundary(context: rusqlite::hooks::AuthContext<'_>) -> Option<String> {
        use rusqlite::hooks::AuthAction;
        match context.action {
            AuthAction::CreateTable { table_name } => Some(format!("table:{table_name}")),
            AuthAction::DropTable { table_name } => Some(format!("drop:{table_name}")),
            AuthAction::AlterTable { table_name, .. } => Some(format!("rename:{table_name}")),
            AuthAction::CreateIndex { index_name, .. } => Some(format!("index:{index_name}")),
            AuthAction::CreateTrigger { trigger_name, .. } => {
                Some(format!("trigger:{trigger_name}"))
            }
            AuthAction::Insert {
                table_name: "lineage_completed_sequence_nodes",
            } => Some("completed_nodes".into()),
            AuthAction::Insert {
                table_name: "lineage_history_indexes",
            } => Some("history_index".into()),
            AuthAction::Insert {
                table_name: "store_meta",
            } => Some("store_meta".into()),
            AuthAction::Pragma {
                pragma_name: "foreign_key_check",
                ..
            } => Some("foreign_key_check".into()),
            AuthAction::Pragma {
                pragma_name: "user_version",
                pragma_value: Some(value),
            } if value == LINEAGE_SCHEMA_VERSION.to_string() => Some("published".into()),
            _ => None,
        }
    }

    fn assert_retained_rows(
        conn: &Connection,
        expected: &[(String, Vec<Vec<rusqlite::types::Value>>)],
    ) {
        let actual = retained_data(conn);
        for (name, rows) in expected {
            assert_eq!(
                &actual.iter().find(|(table, _)| table == name).unwrap().1,
                rows,
                "retained rows differ for {name}",
            );
        }
    }

    #[test]
    fn validates_current_schema_without_requiring_a_write() {
        let conn = current_connection();
        conn.pragma_update(None, "query_only", true).unwrap();
        validate_lineage_schema(&conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
        assert!(
            schema_object_sql(&conn, "index", "lineage_receipts_prior_idx")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn current_schema_reopen_preserves_snapshots_forks_and_receipts() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, BranchId, LineageId};
        for foreign_keys in [false, true] {
            let mut conn = current_connection();
            let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
            let branch = BranchId::new("a".repeat(64)).unwrap();
            let fork = BranchId::new("b".repeat(64)).unwrap();
            lineage::create_lineage(&conn, &lineage, 1).unwrap();
            let command = archived_commit(&branch);
            let receipt = lineage::apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none(),
            )
            .unwrap();
            let fork_receipt = lineage::fork_branch(&mut conn, &lineage, &branch, &fork, None, 2)
                .unwrap()
                .0;
            let snapshots = [&branch, &fork]
                .map(|id| lineage::lineage_session_snapshot(&conn, &lineage, id).unwrap());
            let before = retained_data(&conn);
            let definitions = schema_definitions(&conn);
            conn.pragma_update(None, "foreign_keys", foreign_keys)
                .unwrap();
            initialize_lineage_schema(&mut conn).unwrap();
            assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
            assert_eq!(retained_data(&conn), before);
            let after_definitions = schema_definitions(&conn);
            assert_eq!(after_definitions, definitions);
            assert_eq!(
                [&branch, &fork]
                    .map(|id| { lineage::lineage_session_snapshot(&conn, &lineage, id).unwrap() }),
                snapshots,
            );
            assert_eq!(
                lineage::apply_lineage_session_commit(
                    &mut conn,
                    &lineage,
                    &branch,
                    &command,
                    ObjectCompression::none(),
                )
                .unwrap(),
                receipt
            );
            assert_eq!(
                lineage::fork_branch(&mut conn, &lineage, &branch, &fork, None, 2,)
                    .unwrap()
                    .0,
                fork_receipt
            );
            assert_eq!(retained_data(&conn), before);
            initialize_lineage_schema(&mut conn).unwrap();
            assert_eq!(retained_data(&conn), before);
            assert_eq!(schema_definitions(&conn), after_definitions);
            assert_eq!(
                conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                    .unwrap(),
                foreign_keys
            );
        }
    }

    #[test]
    fn migrated_reverse_reference_lookups_use_indexed_searches() {
        let mut conn = v3_connection();
        initialize_lineage_schema(&mut conn).unwrap();
        for (table, predicate, index) in [
            (
                "lineage_branches",
                "lineage_id = 'x' AND initial_revision_id = 'y'",
                "lineage_branches_initial_revision_idx",
            ),
            (
                "lineage_branches",
                "lineage_id = 'x' AND head_revision_id = 'y'",
                "lineage_branches_head_revision_idx",
            ),
            (
                "lineage_revisions",
                "lineage_id = 'x' AND history_root_id = 'y'",
                "lineage_revisions_history_root_idx",
            ),
            (
                "lineage_revisions",
                "lineage_id = 'x' AND transcript_root_id = 'y'",
                "lineage_revisions_transcript_root_idx",
            ),
            (
                "lineage_revisions",
                "lineage_id = 'x' AND state_payload_id = 'y'",
                "lineage_revisions_state_payload_idx",
            ),
            (
                "lineage_commit_receipts",
                "lineage_id = 'x' AND prior_revision_id = 'y'",
                "lineage_receipts_prior_idx",
            ),
            (
                "lineage_turns",
                "lineage_id = 'x' AND session_id = 'y' AND continuation_of = 1",
                "lineage_turns_continuation_idx",
            ),
            (
                "lineage_payload_object_refs",
                "object_hash = 'x'",
                "lineage_payload_objects_global_idx",
            ),
            (
                "lineage_payload_nested_object_refs",
                "object_hash = 'x'",
                "lineage_payload_nested_objects_global_idx",
            ),
        ] {
            let details = conn
                .prepare(&format!(
                    "EXPLAIN QUERY PLAN SELECT 1 FROM {table} WHERE {predicate}"
                ))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                details
                    .iter()
                    .any(|detail| detail.starts_with("SEARCH ") && detail.contains(index)),
                "reverse lookup did not seek through {index}: {details:?}"
            );
        }
    }

    #[test]
    fn read_only_v4_doctor_checks_shared_objects_without_upgrading() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, BranchId, LineageId};
        let mut conn = v4_connection();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        let branch = BranchId::new("a".repeat(64)).unwrap();
        lineage::create_lineage(&conn, &lineage, 1).unwrap();
        lineage::apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &archived_commit(&branch),
            ObjectCompression::none(),
        )
        .unwrap();
        let hash = crate::object::put_object(&conn, b"{}", ObjectCompression::none())
            .unwrap()
            .hash()
            .to_string();
        let root = tempfile::tempdir().unwrap();
        let path = crate::SessionStoreLayout::from_sessions_root(root.path())
            .lineage_database_path(lineage.as_str());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::diagnostics::backup_connection_to(&conn, &path).unwrap();
        let reader = crate::LineageSessionReader::open_existing_in_lineage(
            root.path(),
            lineage.as_str(),
            branch.as_str(),
        )
        .unwrap();
        let report = reader.doctor_report().unwrap();
        assert!(report.healthy, "{:?}", report.issues);
        assert_eq!(report.schema_version, 4);
        let corrupt = Connection::open(&path).unwrap();
        let guard = schema_object_sql(&corrupt, "trigger", "object_data_root_insert")
            .unwrap()
            .unwrap();
        // Simulate persisted corruption while retaining the exact schema doctor validates.
        corrupt
            .execute_batch("DROP TRIGGER object_data_root_insert")
            .unwrap();
        corrupt
            .execute(
                "INSERT INTO object_data_roots (object_hash, lineage_id, root_id)
             SELECT ?1, lineage_id, root_id FROM lineage_sequence_roots LIMIT 1",
                [&hash],
            )
            .unwrap();
        corrupt.execute_batch(&guard).unwrap();
        let report = reader.doctor_report().unwrap();
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.starts_with(&format!("shared object {hash}:"))),
            "{:?}",
            report.issues
        );
        assert_eq!(user_version(&corrupt).unwrap(), 4);
    }

    fn v4_migration_boundaries() -> Vec<String> {
        [
            "table:request_object_refs_migration",
            "drop:request_object_refs",
            "rename:request_object_refs_migration",
            "published",
            "store_meta",
            "foreign_key_check",
            "commit",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    #[test]
    fn v4_migration_preserves_shared_archives_response_roles_and_external_guards() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, BranchId, LineageId};
        use rusqlite::hooks::{AuthContext, Authorization};
        for foreign_keys in [false, true] {
            for stage in v4_migration_boundaries().into_iter().chain([
                "index:audit role index".into(),
                "trigger:audit guard".into(),
                "success".into(),
            ]) {
                let mut conn = v4_connection();
                let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
                let branch = BranchId::new("a".repeat(64)).unwrap();
                lineage::create_lineage(&conn, &lineage, 1).unwrap();
                let command = archived_commit(&branch);
                let receipt = lineage::apply_lineage_session_commit(
                    &mut conn,
                    &lineage,
                    &branch,
                    &command,
                    ObjectCompression::none(),
                )
                .unwrap();
                let hash = crate::object::put_object(&conn, b"{}", ObjectCompression::none())
                    .unwrap()
                    .hash()
                    .to_string();
                conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
                    .unwrap();
                let request_id = conn.last_insert_rowid();
                conn.execute(
                    "INSERT INTO request_object_refs VALUES (?1, ?2, 'response')",
                    (request_id, &hash),
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO lineage_request_attempts VALUES (?1, ?2, ?3)",
                    (lineage.as_str(), branch.as_str(), request_id),
                )
                .unwrap();
                conn.execute_batch(
                    "CREATE INDEX \"audit role index\" ON request_object_refs(role);
                     CREATE TRIGGER \"audit guard\" BEFORE INSERT ON request_attempts
                     WHEN EXISTS (SELECT 1 FROM request_object_refs WHERE role = 'blocked')
                     BEGIN SELECT RAISE(ABORT, 'blocked response'); END;",
                )
                .unwrap();
                let before = retained_data(&conn);
                let definitions = schema_definitions(&conn);
                let snapshot = lineage::lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
                conn.pragma_update(None, "query_only", true).unwrap();
                validate_lineage_schema(&conn).unwrap();
                assert_eq!(
                    crate::request_audit::lineage_request_stats(
                        &conn,
                        lineage.as_str(),
                        branch.as_str()
                    )
                    .unwrap()
                    .raw_response_count,
                    1
                );
                let attempts = crate::request_audit::lineage_request_attempts(
                    &conn,
                    lineage.as_str(),
                    branch.as_str(),
                    &crate::RequestAuditQuery::default(),
                )
                .unwrap();
                assert_eq!(
                    attempts[0].response_payload_kind,
                    Some(crate::ResponsePayloadKind::Full)
                );
                assert_eq!(
                    lineage::lineage_session_snapshot(&conn, &lineage, &branch).unwrap(),
                    snapshot
                );
                conn.pragma_update(None, "query_only", false).unwrap();
                conn.pragma_update(None, "foreign_keys", foreign_keys)
                    .unwrap();
                let boundary = stage.clone();
                conn.authorizer(Some(move |context: AuthContext<'_>| {
                    if migration_boundary(context).as_deref() == Some(boundary.as_str()) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
                let deny_commit = stage == "commit";
                conn.commit_hook(Some(move || deny_commit)).unwrap();
                let result = initialize_lineage_schema(&mut conn);
                conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
                conn.commit_hook(None::<fn() -> bool>).unwrap();
                if stage != "success" {
                    assert!(result.is_err(), "boundary not reached: {stage}");
                    assert_eq!(user_version(&conn).unwrap(), 4);
                    assert_eq!(schema_definitions(&conn), definitions);
                    assert_eq!(retained_data(&conn), before);
                    validate_lineage_schema(&conn).unwrap();
                    initialize_lineage_schema(&mut conn).unwrap();
                } else {
                    result.unwrap();
                }
                assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
                assert_eq!(retained_data(&conn), before);
                assert_eq!(
                    conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                        .unwrap(),
                    foreign_keys
                );
                assert_eq!(
                    lineage::lineage_session_snapshot(&conn, &lineage, &branch).unwrap(),
                    snapshot
                );
                assert_eq!(
                    lineage::apply_lineage_session_commit(
                        &mut conn,
                        &lineage,
                        &branch,
                        &command,
                        ObjectCompression::none()
                    )
                    .unwrap(),
                    receipt
                );
                for (kind, name, sql) in definitions
                    .iter()
                    .filter(|(_, name, _)| name.starts_with("audit"))
                {
                    assert_eq!(
                        schema_object_sql(&conn, kind, name).unwrap().as_ref(),
                        Some(sql)
                    );
                }
                initialize_lineage_schema(&mut conn).unwrap();
                assert_eq!(retained_data(&conn), before);
            }
        }
    }

    #[test]
    fn v3_migration_errors_restore_exact_schema_data_and_foreign_key_mode() {
        use rusqlite::hooks::{AuthContext, Authorization};
        let stages = migration_boundaries()
            .into_iter()
            .chain(["conflicting_index".into()])
            .collect::<Vec<_>>();
        for foreign_keys in [false, true] {
            for stage in &stages {
                let mut conn = v3_connection();
                let lineage = crate::lineage::LineageId::from_hex("1".repeat(32)).unwrap();
                let branch = crate::lineage::BranchId::new("a".repeat(64)).unwrap();
                crate::lineage::create_lineage(&conn, &lineage, 1).unwrap();
                let command = archived_commit(&branch);
                let receipt = crate::lineage::apply_lineage_session_commit(
                    &mut conn,
                    &lineage,
                    &branch,
                    &command,
                    crate::compression::ObjectCompression::none(),
                )
                .unwrap();
                if stage == "conflicting_index" {
                    conn.execute_batch(
                        "CREATE INDEX lineage_receipts_prior_idx
                         ON lineage_commit_receipts(lineage_id, result_revision_id)",
                    )
                    .unwrap();
                }
                let before = retained_data(&conn);
                let definitions = schema_definitions(&conn);
                conn.pragma_update(None, "foreign_keys", foreign_keys)
                    .unwrap();
                let boundary = stage.clone();
                conn.authorizer(Some(move |context: AuthContext<'_>| {
                    if migration_boundary(context).as_deref() == Some(boundary.as_str()) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
                let deny_commit = stage == "commit";
                conn.commit_hook(Some(move || deny_commit)).unwrap();
                assert!(
                    initialize_lineage_schema(&mut conn).is_err(),
                    "boundary not reached: {stage}"
                );
                conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
                conn.commit_hook(None::<fn() -> bool>).unwrap();
                assert!(conn.is_autocommit());
                assert_eq!(user_version(&conn).unwrap(), 3);
                assert_eq!(retained_data(&conn), before, "data drift at {stage}");
                assert_eq!(
                    schema_definitions(&conn),
                    definitions,
                    "schema drift at {stage}"
                );
                assert_eq!(
                    conn.query_row(
                        "SELECT value FROM store_meta WHERE key = 'schema_version'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
                    "3"
                );
                assert_eq!(
                    conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                        .unwrap(),
                    foreign_keys
                );
                validate_lineage_schema(&conn).unwrap();
                if stage == "conflicting_index" {
                    conn.execute_batch("DROP INDEX lineage_receipts_prior_idx")
                        .unwrap();
                }
                initialize_lineage_schema(&mut conn).unwrap();
                assert_retained_rows(&conn, &before);
                assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
                assert_eq!(
                    crate::lineage::apply_lineage_session_commit(
                        &mut conn,
                        &lineage,
                        &branch,
                        &command,
                        crate::compression::ObjectCompression::none(),
                    )
                    .unwrap(),
                    receipt
                );
                assert_retained_rows(&conn, &before);
            }
        }
    }

    fn archived_commit(branch: &crate::lineage::BranchId) -> crate::SessionCommit {
        use crate::{
            HistoryIndex, HistoryLen, HistorySuffix, SessionIdentity, SideTableSuffixes, StoreHead,
        };
        let message = "synthetic first message α 日本語\n\0".repeat(4096);
        let checkpoint = serde_json::json!({
            "summary": "synthetic archive α".repeat(4096), "kind": "auto",
            "first_live_index": 0, "completed_at_history_len": 1, "created_at_ms": 1,
            "unknown": [null, true, {"nested": 1.25}]
        });
        crate::SessionCommit {
            session_id: branch.as_str().into(),
            expected: StoreHead::default(),
            identity: SessionIdentity {
                id: branch.as_str().into(),
                created_at: 1,
                parent_id: None,
            },
            metadata: serde_json::from_value(serde_json::json!({
                "title": "synthetic title", "first_user_message": message,
                "checkpoint_json": checkpoint, "checkpoint_events_json": [checkpoint],
                "session_cost_usd": 0.0, "updated_at": 1
            }))
            .unwrap(),
            history: HistorySuffix {
                start: HistoryIndex::ZERO,
                final_len: HistoryLen::new(1),
                items: vec![protocol::HistoryItem::system("synthetic history")],
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::ZERO,
                metadata_snapshots: vec![(
                    HistoryIndex::ZERO,
                    serde_json::json!({
                        "first_user_message": message, "unknown": [null, "α"]
                    }),
                )],
                turn_metas: vec![(HistoryIndex::ZERO, serde_json::json!({"unknown": 42}))],
                context_snapshots: vec![(HistoryIndex::ZERO, serde_json::json!({"tokens": 12}))],
            },
            transcript_records: None,
        }
    }

    fn retained_data(conn: &Connection) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
        schema_object_names(conn, "table")
            .unwrap()
            .into_iter()
            .filter(|name| name != "store_meta" && !name.starts_with("sqlite_"))
            .map(|name| {
                let order = (1..=table_columns(conn, &name).unwrap().len())
                    .map(|index| index.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT * FROM \"{}\" ORDER BY {order}",
                        name.replace('"', "\"\"")
                    ))
                    .unwrap();
                let count = stmt.column_count();
                let rows = stmt
                    .query_map([], |row| {
                        (0..count)
                            .map(|index| row.get(index))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                (name, rows)
            })
            .collect()
    }

    #[test]
    fn v3_migration_preserves_archives_receipt_replay_and_external_guards() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, BranchId, LineageId};
        for foreign_keys in [false, true] {
            let mut conn = v3_connection();
            let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
            let branch = BranchId::new("a".repeat(64)).unwrap();
            lineage::create_lineage(&conn, &lineage, 1).unwrap();
            let command = archived_commit(&branch);
            let receipt = lineage::apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none(),
            )
            .unwrap();
            let snapshot = lineage::lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
            let data = retained_data(&conn);
            conn.execute_batch(
                "CREATE INDEX \"extra archive index\" ON lineage_sequence_roots(root_kind);
                 CREATE TRIGGER \"extra archive guard\" BEFORE INSERT ON lineage_sequence_roots
                 WHEN NEW.root_kind = 'invalid' BEGIN SELECT RAISE(ABORT, 'invalid kind'); END;
                 CREATE TRIGGER \"extra external guard\" BEFORE INSERT ON objects
                 WHEN EXISTS (SELECT 1 FROM lineage_sequence_roots WHERE root_kind = 'invalid')
                 BEGIN SELECT RAISE(ABORT, 'invalid owner'); END;",
            )
            .unwrap();
            let extra: Vec<_> = [
                ("index", "extra archive index"),
                ("trigger", "extra archive guard"),
                ("trigger", "extra external guard"),
            ]
            .into_iter()
            .map(|(kind, name)| (kind, name, schema_object_sql(&conn, kind, name).unwrap()))
            .collect();
            conn.pragma_update(None, "foreign_keys", foreign_keys)
                .unwrap();
            initialize_lineage_schema(&mut conn).unwrap();
            assert_retained_rows(&conn, &data);
            assert_eq!(
                lineage::lineage_session_snapshot(&conn, &lineage, &branch).unwrap(),
                snapshot
            );
            assert_eq!(
                lineage::apply_lineage_session_commit(
                    &mut conn,
                    &lineage,
                    &branch,
                    &command,
                    ObjectCompression::none()
                )
                .unwrap(),
                receipt
            );
            assert_retained_rows(&conn, &data);
            for (kind, name, sql) in extra {
                assert_eq!(schema_object_sql(&conn, kind, name).unwrap(), sql);
            }
            assert_eq!(
                conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                    .unwrap(),
                foreign_keys
            );
            let migrated = retained_data(&conn);
            initialize_lineage_schema(&mut conn).unwrap();
            assert_eq!(
                retained_data(&conn),
                migrated,
                "migration must be idempotent"
            );
        }
    }

    #[test]
    fn schema_upgrade_is_crash_atomic_while_replacing_tables_and_installing_layouts() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, LineageId, SequenceKind};
        use rusqlite::hooks::{AuthContext, Authorization};
        const ROLE: &str = "SMELT_SCHEMA_CRASH_ROLE";
        const DB: &str = "SMELT_SCHEMA_CRASH_DB";
        if let (Ok(role), Ok(path)) = (std::env::var(ROLE), std::env::var(DB)) {
            let mut conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
            )
            .unwrap();
            let crash_commit = role == "commit";
            let crash_after_commit = role == "after_commit";
            conn.commit_hook(Some(move || {
                if crash_commit {
                    std::process::abort();
                }
                false
            }))
            .unwrap();
            conn.authorizer(Some(move |context: AuthContext<'_>| {
                if migration_boundary(context).as_deref() == Some(role.as_str()) {
                    std::process::abort();
                }
                Authorization::Allow
            }))
            .unwrap();
            let result = initialize_lineage_schema(&mut conn);
            if crash_after_commit {
                result.unwrap();
                std::process::abort();
            }
            panic!("schema crash boundary was not reached: {result:?}");
        }
        fn definitions(conn: &Connection) -> Vec<(String, String, String)> {
            conn.prepare("SELECT type, name, sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY type, name").unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap()
        }
        fn objects(conn: &Connection) -> Vec<(String, String)> {
            conn.prepare("SELECT hash, hex(bytes) FROM objects ORDER BY hash")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        }
        let dir = tempfile::tempdir().unwrap();
        for version in [3, 4] {
            let boundaries = if version == 3 {
                migration_boundaries()
            } else {
                v4_migration_boundaries()
            };
            let stages = boundaries
                .into_iter()
                .chain(["after_commit".into()])
                .collect::<Vec<_>>();
            for stage in &stages {
                let mut source = if version == 3 {
                    v3_connection()
                } else {
                    v4_connection()
                };
                let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
                lineage::create_lineage(&source, &lineage, 1).unwrap();
                let branch = lineage::BranchId::new("a".repeat(64)).unwrap();
                let command = archived_commit(&branch);
                let receipt = lineage::apply_lineage_session_commit(
                    &mut source,
                    &lineage,
                    &branch,
                    &command,
                    ObjectCompression::none(),
                )
                .unwrap();
                let empty =
                    lineage::empty_sequence(&source, &lineage, SequenceKind::History).unwrap();
                let items = (0..64)
                    .map(|index| format!("legacy-{index}").into_bytes())
                    .collect::<Vec<_>>();
                let (root, _) = lineage::append_sequence(
                    &mut source,
                    &lineage,
                    &empty,
                    &items,
                    ObjectCompression::none(),
                )
                .unwrap();
                let before_definitions = definitions(&source);
                let before_objects = objects(&source);
                let before_data = retained_data(&source);
                let path = dir.path().join(format!("schema-v{version}-{stage}.db"));
                crate::diagnostics::backup_connection_to(&source, &path).unwrap();
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("--exact")
                    .arg("schema::tests::schema_upgrade_is_crash_atomic_while_replacing_tables_and_installing_layouts")
                    .arg("--nocapture")
                    .env(ROLE, stage).env(DB, &path)
                    .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                    .status().unwrap();
                assert!(
                    !status.success(),
                    "child did not crash at v{version}/{stage}"
                );
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    assert_eq!(
                        status.signal(),
                        Some(libc::SIGABRT),
                        "wrong failure at v{version}/{stage}"
                    );
                }
                let mut conn = Connection::open(&path).unwrap();
                conn.pragma_update(None, "foreign_keys", true).unwrap();
                if stage == "after_commit" {
                    assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
                    assert_eq!(
                        definitions(&conn).len(),
                        before_definitions.len() + if version == 3 { 60 } else { 0 }
                    );
                } else {
                    assert_eq!(user_version(&conn).unwrap(), version);
                    assert_eq!(
                        definitions(&conn),
                        before_definitions,
                        "schema drift at v{version}/{stage}"
                    );
                }
                assert_eq!(
                    objects(&conn),
                    before_objects,
                    "object drift at v{version}/{stage}"
                );
                if stage == "after_commit" {
                    assert_retained_rows(&conn, &before_data);
                } else {
                    assert_eq!(
                        retained_data(&conn),
                        before_data,
                        "retained data drift at v{version}/{stage}"
                    );
                }
                assert_eq!(
                    conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                        .unwrap(),
                    "ok"
                );
                assert!(conn
                    .prepare("PRAGMA foreign_key_check")
                    .unwrap()
                    .query([])
                    .unwrap()
                    .next()
                    .unwrap()
                    .is_none());
                validate_lineage_schema(&conn).unwrap();
                assert_eq!(
                    lineage::sequence_range(&conn, &lineage, &root, 0, 64)
                        .unwrap()
                        .0,
                    items
                );
                initialize_lineage_schema(&mut conn).unwrap();
                assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
                assert_eq!(objects(&conn), before_objects);
                assert_retained_rows(&conn, &before_data);
                assert_eq!(
                    lineage::apply_lineage_session_commit(
                        &mut conn,
                        &lineage,
                        &branch,
                        &command,
                        ObjectCompression::none(),
                    )
                    .unwrap(),
                    receipt
                );
                assert_retained_rows(&conn, &before_data);
                lineage::validate_sequence(&conn, &lineage, &root).unwrap();
                validate_lineage_schema(&conn).unwrap();
            }
        }
    }

    #[test]
    fn v3_migration_preserves_payloads_roots_and_installed_guards() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, LineageId, SequenceKind};

        let mut conn = v3_connection();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        lineage::create_lineage(&conn, &lineage, 1).unwrap();
        let empty = lineage::empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let items = (0..2048)
            .map(|index| format!("legacy-{index}").into_bytes())
            .collect::<Vec<_>>();
        let (root, _) =
            lineage::append_sequence(&mut conn, &lineage, &empty, &items, ObjectCompression::None)
                .unwrap();
        let objects = conn
            .prepare("SELECT hash, hex(bytes) FROM objects ORDER BY hash")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER \"extra guard \"\"quoted\" BEFORE INSERT ON objects
             WHEN NEW.raw_size = -1 BEGIN SELECT RAISE(ABORT, 'invalid size'); END;",
        )
        .unwrap();
        let guard = schema_object_sql(&conn, "trigger", "extra guard \"quoted").unwrap();
        let item_guard = schema_object_sql(&conn, "trigger", "lineage_sequence_entry_insert")
            .unwrap()
            .unwrap();
        conn.execute_batch("DROP TRIGGER lineage_sequence_entry_insert")
            .unwrap();
        conn.execute_batch(
            &item_guard.replace("('history', 'transcript')", "('history',\n 'transcript')"),
        )
        .unwrap();
        validate_lineage_schema(&conn).unwrap();

        initialize_lineage_schema(&mut conn).unwrap();
        assert!(conn
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
            .unwrap());
        assert_eq!(
            schema_object_sql(&conn, "trigger", "extra guard \"quoted").unwrap(),
            guard
        );
        assert_eq!(
            lineage::load_root(&conn, &lineage, root.id()).unwrap(),
            root
        );
        assert_eq!(
            lineage::sequence_range(&conn, &lineage, &root, 0, 2048)
                .unwrap()
                .0,
            items
        );
        let migrated_objects = conn
            .prepare("SELECT hash, hex(bytes) FROM objects ORDER BY hash")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(migrated_objects, objects);
        lineage::validate_sequence(&conn, &lineage, &root).unwrap();
        validate_lineage_schema(&conn).unwrap();
        initialize_lineage_schema(&mut conn).unwrap();
    }

    #[test]
    fn v3_migration_failure_restores_tables_guards_and_foreign_keys() {
        let mut conn = v3_connection();
        conn.execute_batch("CREATE TABLE lineage_sequence_nodes_migration (collision INTEGER)")
            .unwrap();
        let guard = schema_object_sql(&conn, "trigger", "lineage_sequence_entry_insert").unwrap();
        assert!(initialize_lineage_schema(&mut conn).is_err());
        assert_eq!(user_version(&conn).unwrap(), 3);
        assert!(conn
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
            .unwrap());
        assert_eq!(
            schema_object_sql(&conn, "trigger", "lineage_sequence_entry_insert").unwrap(),
            guard
        );
        assert!(
            schema_object_sql(&conn, "table", "lineage_payload_object_refs_migration")
                .unwrap()
                .is_none()
        );
        validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn v3_migration_foreign_key_failure_rolls_back_replaced_tables() {
        for foreign_keys in [false, true] {
            let mut conn = v3_connection();
            conn.pragma_update(None, "foreign_keys", false).unwrap();
            conn.execute(
                "INSERT INTO lineage_sequence_roots VALUES (?1, ?2, 'history', NULL, 0, 0, 0)",
                ("1".repeat(32), "a".repeat(64)),
            )
            .unwrap();
            conn.pragma_update(None, "foreign_keys", foreign_keys)
                .unwrap();
            let definition = schema_object_sql(&conn, "table", "lineage_sequence_roots").unwrap();
            assert!(matches!(initialize_lineage_schema(&mut conn),
                Err(StoreError::Integrity(message)) if message.contains("foreign key violation")));
            assert_eq!(user_version(&conn).unwrap(), 3);
            assert!(conn.is_autocommit());
            assert_eq!(
                conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                    .unwrap(),
                foreign_keys,
            );
            assert_eq!(
                schema_object_sql(&conn, "table", "lineage_sequence_roots").unwrap(),
                definition
            );
            validate_lineage_schema(&conn).unwrap();
        }
    }

    #[test]
    fn data_sequences_share_unchanged_binary_payloads_and_validate_completion() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, LineageId, SequenceKind};

        let mut conn = v3_connection();
        initialize_lineage_schema(&mut conn).unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        lineage::create_lineage(&conn, &lineage, 1).unwrap();
        let empty = lineage::empty_sequence(&conn, &lineage, SequenceKind::Data).unwrap();
        let items = (0..512_u64)
            .map(|index| {
                let mut bytes = vec![0xff; 4096];
                bytes[..8].copy_from_slice(&index.to_le_bytes());
                bytes
            })
            .collect::<Vec<_>>();
        let (root, _) =
            lineage::append_sequence(&mut conn, &lineage, &empty, &items, ObjectCompression::None)
                .unwrap();
        let tx = conn.transaction().unwrap();
        let ((prefix, _), split_stats) =
            lineage::split_sequence_in(&tx, &lineage, &root, 511).unwrap();
        let (tail, append_stats) = lineage::append_sequence_in(
            &tx,
            &lineage,
            &prefix,
            &[b"replacement".to_vec()],
            ObjectCompression::None,
        )
        .unwrap();
        assert_eq!(
            split_stats.payloads_read + append_stats.payloads_read,
            0,
            "tail edits must not hydrate old binary payloads"
        );
        assert_eq!(append_stats.payloads_written, 1);
        assert!(split_stats.nodes_written + append_stats.nodes_written <= 32);
        tx.commit().unwrap();
        assert_eq!(
            lineage::sequence_range(&conn, &lineage, &root, 0, 512)
                .unwrap()
                .0,
            items
        );
        assert_eq!(
            lineage::sequence_range(&conn, &lineage, &tail, 511, 512)
                .unwrap()
                .0,
            [b"replacement".to_vec()]
        );
        for root in [&root, &tail] {
            lineage::validate_sequence(&conn, &lineage, root).unwrap();
        }
        conn.execute_batch("DELETE FROM lineage_sequence_entries WHERE entry_index = 0")
            .expect_err("completed data sequences must retain immutable entries");
    }

    #[test]
    fn validates_v3_without_requiring_a_write() {
        let conn = v3_connection();
        conn.pragma_update(None, "query_only", true).unwrap();
        validate_lineage_schema(&conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 3);
        assert!(
            schema_object_sql(&conn, "table", "lineage_completed_sequence_nodes")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn upgrades_v3_nodes_bottom_up_without_changing_roots() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, LineageId, SequenceKind};

        let mut conn = v3_connection();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        lineage::create_lineage(&conn, &lineage, 1).unwrap();
        let empty = lineage::empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
        let items = (0..2048)
            .map(|index| format!("item-{index}").into_bytes())
            .collect::<Vec<_>>();
        let (root, _) =
            lineage::append_sequence(&mut conn, &lineage, &empty, &items, ObjectCompression::None)
                .unwrap();
        assert!(root.depth() > 1);
        initialize_lineage_schema(&mut conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
        assert_eq!(
            conn.query_row(
                "SELECT value FROM store_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            LINEAGE_SCHEMA_VERSION.to_string()
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM lineage_sequence_nodes", [], |row| row
                .get::<_, i64>(0),)
                .unwrap(),
            conn.query_row(
                "SELECT count(*) FROM lineage_completed_sequence_nodes",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        );
        assert_eq!(
            lineage::load_root(&conn, &lineage, root.id()).unwrap(),
            root
        );
        assert_eq!(
            lineage::sequence_range(&conn, &lineage, &root, 0, 2048)
                .unwrap()
                .0,
            items
        );
        lineage::validate_sequence(&conn, &lineage, &root).unwrap();
        initialize_lineage_schema(&mut conn).unwrap();
    }

    #[test]
    fn unpublished_incomplete_v3_nodes_remain_uncompleted() {
        use crate::lineage::{self, LineageId};

        let mut conn = v3_connection();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        lineage::create_lineage(&conn, &lineage, 1).unwrap();
        conn.execute(
            "INSERT INTO lineage_sequence_nodes VALUES (?1, ?2, 'history', 'leaf', 0, 1, 1, 0)",
            (lineage.as_str(), "a".repeat(64)),
        )
        .unwrap();
        initialize_lineage_schema(&mut conn).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM lineage_completed_sequence_nodes",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn incomplete_published_v3_tree_rolls_back_migration() {
        use crate::lineage::{self, LineageId};

        let mut conn = v3_connection();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        lineage::create_lineage(&conn, &lineage, 1).unwrap();
        conn.execute_batch("DROP TRIGGER lineage_sequence_root_insert")
            .unwrap();
        conn.execute(
            "INSERT INTO lineage_sequence_nodes VALUES (?1, ?2, 'history', 'leaf', 0, 1, 1, 0)",
            (lineage.as_str(), "a".repeat(64)),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO lineage_sequence_roots VALUES (?1, ?2, 'history', ?3, 1, 1, 0)",
            (lineage.as_str(), "b".repeat(64), "a".repeat(64)),
        )
        .unwrap();
        conn.execute_batch(LINEAGE_SCHEMA_V3).unwrap();
        validate_lineage_schema(&conn).unwrap();
        assert!(
            matches!(initialize_lineage_schema(&mut conn), Err(StoreError::Integrity(message))
            if message.contains("incomplete sequence root"))
        );
        assert_eq!(user_version(&conn).unwrap(), 3);
        assert!(
            schema_object_sql(&conn, "table", "lineage_completed_sequence_nodes")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            conn.query_row(
                "SELECT value FROM store_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "3"
        );
        validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn concurrent_lineage_schema_initialization_rechecks_version_after_waiting() {
        use std::cell::RefCell;
        use std::sync::mpsc;
        use std::time::Duration;

        thread_local! {
            static WAITING: RefCell<Option<mpsc::Sender<()>>> = const { RefCell::new(None) };
        }
        fn wait_for_writer(_attempt: i32) -> bool {
            WAITING.with_borrow_mut(|sender| {
                if let Some(sender) = sender.take() {
                    sender.send(()).unwrap();
                }
            });
            std::thread::sleep(Duration::from_millis(1));
            true
        }

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lineage.db");
        let mut conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        let blocker = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let (waiting, receivers) = mpsc::channel();
        std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..2 {
                let waiting = waiting.clone();
                let path = &path;
                workers.push(scope.spawn(move || {
                    WAITING.with_borrow_mut(|sender| *sender = Some(waiting));
                    let mut conn = Connection::open(path).unwrap();
                    conn.busy_handler(Some(wait_for_writer)).unwrap();
                    initialize_lineage_schema(&mut conn)
                }));
            }
            for _ in 0..2 {
                receivers.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            blocker.commit().unwrap();
            for worker in workers {
                worker.join().unwrap().unwrap();
            }
        });
        validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn fresh_schema_creation_errors_are_atomic() {
        use rusqlite::hooks::{AuthContext, Authorization};
        let current = lineage_schema_shape(LINEAGE_SCHEMA_VERSION).unwrap();
        let stages = current
            .tables
            .iter()
            .map(|table| format!("table:{}", table.name))
            .chain(
                current
                    .objects
                    .iter()
                    .map(|object| format!("{}:{}", object.kind, object.name)),
            )
            .chain([
                "published".into(),
                "store_meta".into(),
                "foreign_key_check".into(),
                "commit".into(),
            ])
            .collect::<Vec<_>>();
        assert_eq!(stages.len(), 140);
        for foreign_keys in [false, true] {
            for stage in &stages {
                let mut conn = Connection::open_in_memory().unwrap();
                conn.pragma_update(None, "foreign_keys", foreign_keys)
                    .unwrap();
                let boundary = stage.clone();
                conn.authorizer(Some(move |context: AuthContext<'_>| {
                    if migration_boundary(context).as_deref() == Some(boundary.as_str()) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
                let deny_commit = stage == "commit";
                conn.commit_hook(Some(move || deny_commit)).unwrap();
                assert!(
                    initialize_lineage_schema(&mut conn).is_err(),
                    "boundary not reached: {stage}"
                );
                conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
                conn.commit_hook(None::<fn() -> bool>).unwrap();
                assert!(conn.is_autocommit());
                assert_eq!(user_version(&conn).unwrap(), 0);
                assert!(schema_object_names(&conn, "table").unwrap().is_empty());
                assert!(schema_object_names(&conn, "index").unwrap().is_empty());
                assert!(schema_object_names(&conn, "trigger").unwrap().is_empty());
                assert_eq!(
                    conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                        .unwrap(),
                    foreign_keys
                );
                initialize_lineage_schema(&mut conn).unwrap();
                validate_lineage_schema(&conn).unwrap();
            }
        }
    }

    #[test]
    fn rejects_unknown_schema_versions_without_mutation() {
        for version in [
            -1,
            1,
            2,
            LINEAGE_SCHEMA_VERSION + 1,
            LINEAGE_SCHEMA_VERSION + 2,
            i32::MAX,
        ] {
            let mut conn = Connection::open_in_memory().unwrap();
            set_user_version(&conn, version).unwrap();
            assert!(matches!(
                initialize_lineage_schema(&mut conn),
                Err(StoreError::UnsupportedSchema { found, expected })
                    if found == version && expected == LINEAGE_SCHEMA_VERSION
            ));
            assert_eq!(user_version(&conn).unwrap(), version);
            assert!(schema_object_names(&conn, "table").unwrap().is_empty());
        }
    }

    #[test]
    fn rejects_legacy_layout_with_current_marker_without_mutation() {
        for foreign_keys in [false, true] {
            let mut conn = v3_connection();
            crate::object::put_object(
                &conn,
                b"retained legacy object",
                crate::compression::ObjectCompression::none(),
            )
            .unwrap();
            set_user_version(&conn, LINEAGE_SCHEMA_VERSION).unwrap();
            conn.execute(
                "UPDATE store_meta SET value = ?1 WHERE key = 'schema_version'",
                [LINEAGE_SCHEMA_VERSION.to_string()],
            )
            .unwrap();
            conn.pragma_update(None, "foreign_keys", foreign_keys)
                .unwrap();
            conn.pragma_update(None, "query_only", true).unwrap();
            let definitions = schema_definitions(&conn);
            let data = retained_data(&conn);
            assert!(matches!(
                validate_lineage_schema(&conn),
                Err(StoreError::Integrity(_))
            ));
            assert!(matches!(
                initialize_lineage_schema(&mut conn),
                Err(StoreError::Integrity(_))
            ));
            assert_eq!(schema_definitions(&conn), definitions);
            assert_eq!(retained_data(&conn), data);
            assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
            assert_eq!(
                conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                    .unwrap(),
                foreign_keys
            );
            assert!(conn.is_autocommit());
        }
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
