use std::sync::OnceLock;

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Result, StoreError};

pub const LINEAGE_SCHEMA_VERSION: i32 = 8;

const LINEAGE_SCHEMA: &str = include_str!("lineage_schema.sql");

pub(crate) fn initialize_lineage_schema(conn: &mut Connection) -> Result<()> {
    if user_version(conn)? == LINEAGE_SCHEMA_VERSION {
        return validate_lineage_schema(conn);
    }
    // Another branch may initialize this database while we wait.
    let tx = crate::write_transaction::begin_write(conn, "initialize schema")?;
    match user_version(&tx)? {
        0 => {
            tx.execute_batch(LINEAGE_SCHEMA)?;
            set_user_version(&tx, LINEAGE_SCHEMA_VERSION)?;
            write_store_meta(&tx)?;
        }
        // COMPAT(lineage-schema-v7): v7 stores reclaimed by tracing the whole graph.
        7 => {
            migrate_reclamation_queue(&tx)?;
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
    tx.commit()?;
    Ok(())
}

/// Installs the reclamation queue and its triggers, then queues every canonical
/// row once so whatever v7 left unreclaimed is found without tracing the graph.
fn migrate_reclamation_queue(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TRIGGER lineage_history_index_delete;
         DROP TRIGGER lineage_history_index_node_delete;
         DELETE FROM store_meta WHERE key = 'reclaimed_through';",
    )?;
    let shape = lineage_schema_shape()?;
    let tables = shape
        .tables
        .iter()
        .map(|table| ("table", &table.name, &table.sql));
    let objects = shape
        .objects
        .iter()
        .map(|object| (object.kind.as_str(), &object.name, &object.sql));
    for (kind, name, sql) in tables.chain(objects) {
        if schema_object_sql(conn, kind, name)?.is_none() {
            conn.execute_batch(sql)?;
        }
    }
    conn.execute_batch(
        "INSERT INTO lineage_reclamation_queue (lineage_id, kind, id)
         SELECT lineage_id, 'branch', session_id FROM lineage_branches
         WHERE deleted_at IS NOT NULL
         UNION ALL SELECT lineage_id, 'revision', revision_id FROM lineage_revisions
         UNION ALL SELECT lineage_id, 'root', root_id FROM lineage_sequence_roots
         UNION ALL SELECT lineage_id, 'history_index', node_id FROM lineage_history_index_nodes
         UNION ALL SELECT lineage_id, 'node', node_id FROM lineage_sequence_nodes
         UNION ALL SELECT lineage_id, 'payload', payload_id FROM lineage_payload_object_refs
         UNION ALL SELECT identity.lineage_id, 'object', object.hash
         FROM objects object, lineage_identity identity;",
    )?;
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
    validate_schema_shape(conn, lineage_schema_shape()?)
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

fn lineage_schema_shape() -> Result<&'static SchemaShape> {
    static SHAPE: OnceLock<std::result::Result<SchemaShape, String>> = OnceLock::new();
    match SHAPE.get_or_init(load_lineage_schema_shape) {
        Ok(shape) => Ok(shape),
        Err(message) => Err(StoreError::Integrity(message.clone())),
    }
}

fn load_lineage_schema_shape() -> std::result::Result<SchemaShape, String> {
    let conn = Connection::open_in_memory().map_err(|error| error.to_string())?;
    conn.pragma_update(None, "foreign_keys", false)
        .map_err(|error| error.to_string())?;
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

    fn creation_boundary(context: rusqlite::hooks::AuthContext<'_>) -> Option<String> {
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
                table_name: "store_meta",
            } => Some("store_meta".into()),
            AuthAction::Pragma {
                pragma_name: "user_version",
                pragma_value: Some(value),
            } if value == LINEAGE_SCHEMA_VERSION.to_string() => Some("published".into()),
            _ => None,
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
    fn v7_stores_migrate_and_reclaim_what_they_left_behind() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, BranchId, LineageId};
        let mut conn = current_connection();
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
        let snapshot = lineage::lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
        let definitions = schema_definitions(&conn);
        // Rebuild the v7 surface: no queue and immutable history indexes.
        let added = conn
            .prepare(
                "SELECT type, name FROM sqlite_schema
                 WHERE name IN ('lineage_reclamation_queue', 'lineage_session_receipts_turn_idx',
                                'lineage_revision_release')
                    OR (type = 'trigger' AND (name LIKE '%reclaim' OR name LIKE '%created'))
                 ORDER BY type = 'table'",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for (kind, name) in added {
            conn.execute_batch(&format!("DROP {kind} {name}")).unwrap();
        }
        conn.execute_batch(
            "DROP TRIGGER lineage_history_index_delete;
             CREATE TRIGGER lineage_history_index_delete
             BEFORE DELETE ON lineage_history_semantic_roots
             BEGIN
                 SELECT RAISE(ABORT, 'history semantic indexes are immutable');
             END;
             CREATE TRIGGER lineage_history_index_node_delete
             BEFORE DELETE ON lineage_history_index_nodes
             BEGIN
                 SELECT RAISE(ABORT, 'history semantic index nodes are immutable');
             END;
             PRAGMA user_version = 7;",
        )
        .unwrap();
        let leaked =
            crate::object::put_object(&conn, b"left behind by v7", ObjectCompression::none())
                .unwrap();

        initialize_lineage_schema(&mut conn).unwrap();

        assert_eq!(user_version(&conn).unwrap(), LINEAGE_SCHEMA_VERSION);
        assert_eq!(schema_definitions(&conn), definitions);
        while !lineage::reclaim_step(&mut conn, &lineage, 64)
            .unwrap()
            .complete
        {}
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM objects WHERE hash = ?1",
                [leaked.hash()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            lineage::lineage_session_snapshot(&conn, &lineage, &branch).unwrap(),
            snapshot
        );
    }

    #[test]
    fn reverse_reference_lookups_use_indexed_searches() {
        let conn = current_connection();
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
    fn data_sequences_share_unchanged_binary_payloads_and_validate_completion() {
        use crate::compression::ObjectCompression;
        use crate::lineage::{self, LineageId, SequenceKind};

        let mut conn = current_connection();
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
        let current = lineage_schema_shape().unwrap();
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
            .chain(["published".into(), "store_meta".into(), "commit".into()])
            .collect::<Vec<_>>();
        assert_eq!(stages.len(), 148);
        for foreign_keys in [false, true] {
            for stage in &stages {
                let mut conn = Connection::open_in_memory().unwrap();
                conn.pragma_update(None, "foreign_keys", foreign_keys)
                    .unwrap();
                let boundary = stage.clone();
                conn.authorizer(Some(move |context: AuthContext<'_>| {
                    if creation_boundary(context).as_deref() == Some(boundary.as_str()) {
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
            3,
            4,
            5,
            6,
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
