use std::sync::OnceLock;

use rusqlite::{Connection, OptionalExtension};

use crate::error::{Result, StoreError};

pub const LINEAGE_SCHEMA_VERSION: i32 = 3;

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
    fn rejects_unknown_schema_versions_without_mutation() {
        for version in [1, 2, LINEAGE_SCHEMA_VERSION + 1] {
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
