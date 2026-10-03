use super::*;
use crate::object::{object_meta, ObjectLayout, MAX_OBJECT_RAW_SIZE};

pub(crate) const SHARED_OBJECT_MIN_BYTES: u64 = 128 * 1024;
const CHUNK_MIN_BYTES: usize = 8 * 1024;
const CHUNK_MAX_BYTES: usize = 64 * 1024;
const CHUNK_BOUNDARY_MASK: u64 = (1 << 14) - 1;
const MAX_CHUNKS: u64 = MAX_OBJECT_RAW_SIZE / CHUNK_MIN_BYTES as u64;
const MAX_CHUNK_TREE_DEPTH: u32 = 3;

const fn gear_table() -> [u64; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        let mut value = (i as u64).wrapping_add(0x9e3779b97f4a7c15);
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        table[i] = value ^ (value >> 31);
        i += 1;
    }
    table
}

const GEAR: [u64; 256] = gear_table();

/// Content-defined boundaries allow unchanged bytes to share chunks after edits
/// shift their position. Chunks borrow the input and are never recursively shared.
fn object_chunks(bytes: &[u8]) -> Vec<&[u8]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut hash = 0_u64;
    for (index, byte) in bytes.iter().enumerate() {
        hash = hash.wrapping_shl(1).wrapping_add(GEAR[*byte as usize]);
        let len = index + 1 - start;
        if len >= CHUNK_MIN_BYTES && (hash & CHUNK_BOUNDARY_MASK == 0 || len == CHUNK_MAX_BYTES) {
            chunks.push(&bytes[start..index + 1]);
            start = index + 1;
            hash = 0;
        }
    }
    if start < bytes.len() {
        chunks.push(&bytes[start..]);
    }
    chunks
}

pub(crate) fn hydrate_object_data(
    conn: &Connection,
    lineage_id: &str,
    root_id: &str,
    raw_size: u64,
) -> Result<Vec<u8>> {
    if !(SHARED_OBJECT_MIN_BYTES..=MAX_OBJECT_RAW_SIZE).contains(&raw_size) {
        return Err(StoreError::Integrity(
            "shared object size is out of bounds".into(),
        ));
    }
    let lineage = LineageId::from_hex(lineage_id)?;
    let root_id = RootId::from_db(root_id.to_owned())?;
    let root = load_root(conn, &lineage, &root_id)?;
    if root.kind != SequenceKind::Data
        || root.byte_count != raw_size
        || !(2..=MAX_CHUNKS).contains(&root.item_count)
        || !(1..=MAX_CHUNK_TREE_DEPTH).contains(&root.depth)
    {
        return Err(StoreError::Integrity(
            "shared object root has invalid extents or kind".into(),
        ));
    }
    let (refs, _) = sequence_payload_refs_from_root(conn, &lineage, &root, 0, root.item_count)?;
    let mut bytes = Vec::with_capacity(raw_size as usize);
    for (index, payload) in refs.iter().enumerate() {
        let minimum = if index + 1 == refs.len() {
            1
        } else {
            CHUNK_MIN_BYTES as u64
        };
        if !(minimum..=CHUNK_MAX_BYTES as u64).contains(&payload.byte_count) {
            return Err(StoreError::Integrity(
                "shared object chunk size is out of bounds".into(),
            ));
        }
        let meta =
            object_meta(conn, &payload.object_hash)?.ok_or_else(|| StoreError::MissingObject {
                reference: format!("object {}", payload.object_hash),
            })?;
        if meta.layout != ObjectLayout::Blob || meta.raw_size != payload.byte_count {
            return Err(StoreError::Integrity(
                "shared object leaf is not a matching inline chunk".into(),
            ));
        }
        if (bytes.len() as u64).saturating_add(meta.raw_size) > raw_size {
            return Err(StoreError::Integrity(
                "shared object exceeds its declared size".into(),
            ));
        }
        bytes.extend_from_slice(&crate::object::object_bytes(conn, &meta)?);
    }
    if bytes.len() as u64 != raw_size {
        return Err(StoreError::Integrity(
            "shared object reconstructed the wrong size".into(),
        ));
    }
    Ok(bytes)
}

/// A bounded sharing pass over one lineage. Restarting a pass is safe: committed
/// layouts are skipped and rejected cohorts leave no physical allocations.
#[derive(Clone, Debug, Default)]
pub struct ObjectSharingCursor {
    lineage: Option<LineageId>,
    raw_size: i64,
    hash: String,
    complete: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ObjectSharingStep {
    pub objects_scanned: usize,
    pub objects_shared: usize,
    pub raw_bytes_processed: u64,
    pub pages_saved: u64,
    pub complete: bool,
}

const SHARING_SCAN_ROWS: usize = 256;
// Cross-object reuse must be possible even at the logical object size limit.
const SHARING_COHORT_BYTES: u64 = 2 * MAX_OBJECT_RAW_SIZE;

fn occupied_pages(conn: &Connection) -> Result<u64> {
    let pages: i64 = conn.query_row(
        "SELECT (SELECT page_count FROM pragma_page_count) -
                (SELECT freelist_count FROM pragma_freelist_count)",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(pages).map_err(|_| StoreError::Integrity("negative occupied page count".into()))
}

/// Compare the aggregate compressed payload, tree, table and index cost inside
/// one transaction. The first member may grow; only the entire cohort must save
/// occupied pages. Rollback also discards chunks and roots from rejected cohorts.
pub(crate) fn share_objects(
    conn: &mut Connection,
    lineage: &LineageId,
    cursor: &mut ObjectSharingCursor,
) -> Result<ObjectSharingStep> {
    if cursor
        .lineage
        .as_ref()
        .is_some_and(|owner| owner != lineage)
    {
        return Err(StoreError::Integrity(
            "object sharing cursor belongs to another lineage".into(),
        ));
    }
    if cursor.complete {
        return Ok(ObjectSharingStep {
            complete: true,
            ..ObjectSharingStep::default()
        });
    }
    let tx = crate::write_transaction::begin_write(conn, "share object cohort")?;
    // Include already shared rows in the bounded indexed scan. Filtering them
    // before LIMIT would allow an unbounded scan after earlier maintenance.
    let candidates = tx
        .prepare(
            "SELECT raw_size, hash, EXISTS (
             SELECT 1 FROM object_data_roots WHERE object_hash = objects.hash
         ) FROM objects INDEXED BY objects_sharing_idx
         WHERE (raw_size, hash) > (?1, ?2) AND raw_size >= 131072
         ORDER BY raw_size, hash LIMIT ?3",
        )?
        .query_map(
            (cursor.raw_size, &cursor.hash, SHARING_SCAN_ROWS as i64),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let before = occupied_pages(&tx)?;
    let mut step = ObjectSharingStep::default();
    let mut position = (cursor.raw_size, cursor.hash.clone());
    for (raw_size, hash, shared) in candidates.iter().take(SHARING_SCAN_ROWS) {
        let raw_bytes = u64::try_from(*raw_size)
            .map_err(|_| StoreError::Integrity("negative sharing object size".into()))?;
        if !shared {
            if step.raw_bytes_processed > 0
                && step.raw_bytes_processed.saturating_add(raw_bytes) > SHARING_COHORT_BYTES
            {
                break;
            }
            step.objects_shared += usize::from(share_object_in_transaction(
                &tx,
                lineage,
                hash,
                ObjectCompression::default(),
            )?);
            step.raw_bytes_processed += raw_bytes;
        }
        step.objects_scanned += 1;
        position = (*raw_size, hash.clone());
    }
    step.complete =
        step.objects_scanned == candidates.len() && candidates.len() < SHARING_SCAN_ROWS;
    let after = occupied_pages(&tx)?;
    if after < before {
        step.pages_saved = before - after;
        tx.commit()?;
    } else {
        tx.rollback()?;
        step.objects_shared = 0;
    }
    // Publish progress only after either commit or successful rollback. A
    // returned error leaves the same cohort available for an explicit retry.
    cursor.lineage = Some(lineage.clone());
    cursor.raw_size = position.0;
    cursor.hash = position.1;
    cursor.complete = step.complete;
    Ok(step)
}

#[cfg(test)]
fn share_object(
    conn: &mut Connection,
    lineage: &LineageId,
    hash: &str,
    compression: ObjectCompression,
) -> Result<bool> {
    let tx = crate::write_transaction::begin_write(conn, "share object")?;
    let shared = share_object_in_transaction(&tx, lineage, hash, compression)?;
    tx.commit()?;
    Ok(shared)
}

/// Replace only the physical representation, preserving every logical reference.
fn share_object_in_transaction(
    conn: &Connection,
    lineage: &LineageId,
    hash: &str,
    compression: ObjectCompression,
) -> Result<bool> {
    let meta = object_meta(conn, hash)?.ok_or_else(|| StoreError::MissingObject {
        reference: format!("object {hash}"),
    })?;
    if meta.layout != ObjectLayout::Blob || meta.raw_size < SHARED_OBJECT_MIN_BYTES {
        return Ok(false);
    }
    let bytes = crate::object::object_bytes(conn, &meta)?;
    let chunks = object_chunks(&bytes);
    let mut stats = OperationStats::default();
    let root = build_sequence_from_empty(
        conn,
        lineage,
        SequenceKind::Data,
        &chunks,
        compression,
        &mut stats,
    )?;
    insert_root(conn, lineage, &root, &mut stats)?;
    let reconstructed =
        hydrate_object_data(conn, lineage.as_str(), root.id.as_str(), meta.raw_size)?;
    if reconstructed != bytes || sha256_hex(&reconstructed) != meta.hash {
        return Err(StoreError::Integrity(
            "physical object sharing changed logical bytes".into(),
        ));
    }
    conn.execute(
        "UPDATE objects SET codec = 'none', stored_size = 0, bytes = x'' WHERE hash = ?1",
        [hash],
    )?;
    conn.execute(
        "INSERT INTO object_data_roots (object_hash, lineage_id, root_id) VALUES (?1, ?2, ?3)",
        (hash, lineage.as_str(), root.id.as_str()),
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Connection, LineageId) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        create_lineage(&conn, &lineage, 1).unwrap();
        (conn, lineage)
    }

    fn binary_fixture(len: usize) -> Vec<u8> {
        let mut state = 0x123456789abcdef0_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn physical_bytes(conn: &Connection) -> u64 {
        let size = conn
            .query_row("SELECT SUM(stored_size) FROM objects", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        u64::try_from(size).unwrap()
    }

    fn sharing_table_counts(conn: &Connection) -> Vec<i64> {
        [
            "objects",
            "object_data_roots",
            "lineage_payload_object_refs",
            "lineage_sequence_roots",
            "lineage_sequence_nodes",
            "lineage_sequence_entries",
            "lineage_completed_sequence_nodes",
        ]
        .iter()
        .map(|table| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        })
        .collect()
    }

    #[test]
    fn sharing_cohort_accepts_aggregate_savings_despite_first_member_overhead() {
        for foreign_keys in [false, true] {
            let (mut conn, lineage) = fixture();
            conn.pragma_update(None, "foreign_keys", foreign_keys)
                .unwrap();
            let archive = binary_fixture(512 * 1024);
            let first = put_object(&conn, &archive, ObjectCompression::default()).unwrap();
            let counts = sharing_table_counts(&conn);
            let pages = occupied_pages(&conn).unwrap();
            let rejected =
                share_objects(&mut conn, &lineage, &mut ObjectSharingCursor::default()).unwrap();
            assert!(rejected.complete);
            assert_eq!(rejected.objects_shared, 0);
            assert_eq!(sharing_table_counts(&conn), counts);
            assert_eq!(occupied_pages(&conn).unwrap(), pages);
            assert_eq!(
                object(&conn, first.hash()).unwrap().unwrap().meta.layout,
                ObjectLayout::Blob
            );
            let mut objects = vec![first];
            for index in 0..3 {
                let mut bytes = format!("title-{index}").into_bytes();
                bytes.extend_from_slice(&archive);
                objects.push(put_object(&conn, &bytes, ObjectCompression::default()).unwrap());
            }
            let before = occupied_pages(&conn).unwrap();
            let step =
                share_objects(&mut conn, &lineage, &mut ObjectSharingCursor::default()).unwrap();
            assert!(step.complete);
            assert_eq!(step.objects_shared, objects.len());
            assert_eq!(step.pages_saved, before - occupied_pages(&conn).unwrap());
            assert!(step.pages_saved > before / 2);
            for stored in objects {
                assert_eq!(
                    object(&conn, stored.hash()).unwrap().unwrap().bytes,
                    stored.bytes
                );
            }
            crate::schema::validate_lineage_schema(&conn).unwrap();
        }
    }

    #[test]
    fn sharing_cohort_rejects_unique_and_highly_compressed_data_without_remnants() {
        let (mut conn, lineage) = fixture();
        let mut objects = Vec::new();
        let mut binary = binary_fixture(2 * 1024 * 1024);
        for index in 0..4 {
            let bytes = binary.split_off(binary.len() - 512 * 1024);
            objects.push(put_object(&conn, &bytes, ObjectCompression::default()).unwrap());
            let compressed = vec![index; 512 * 1024];
            objects.push(put_object(&conn, &compressed, ObjectCompression::default()).unwrap());
        }
        let before = sharing_table_counts(&conn);
        let pages = occupied_pages(&conn).unwrap();
        let mut cursor = ObjectSharingCursor::default();
        let step = share_objects(&mut conn, &lineage, &mut cursor).unwrap();
        assert!(step.complete);
        assert_eq!(step.objects_scanned, objects.len());
        assert_eq!(step.objects_shared, 0);
        assert_eq!(step.pages_saved, 0);
        assert_eq!(sharing_table_counts(&conn), before);
        assert_eq!(occupied_pages(&conn).unwrap(), pages);
        assert_eq!(
            share_objects(&mut conn, &lineage, &mut cursor)
                .unwrap()
                .objects_scanned,
            0
        );
        for stored in objects {
            assert_eq!(object(&conn, stored.hash()).unwrap(), Some(stored));
        }
    }

    #[test]
    fn sharing_cohort_error_rolls_back_prior_members_and_preserves_retry_cursor() {
        let (mut conn, lineage) = fixture();
        let archive = binary_fixture(512 * 1024);
        for index in 0..4 {
            let mut bytes = vec![index];
            bytes.extend_from_slice(&archive);
            put_object(&conn, &bytes, ObjectCompression::default()).unwrap();
        }
        let before = sharing_table_counts(&conn);
        conn.execute_batch(
            "CREATE TEMP TRIGGER reject_second_layout BEFORE INSERT ON object_data_roots
             WHEN (SELECT COUNT(*) FROM object_data_roots) = 1
             BEGIN SELECT RAISE(ABORT, 'injected second member failure'); END;",
        )
        .unwrap();
        let mut cursor = ObjectSharingCursor::default();
        assert!(share_objects(&mut conn, &lineage, &mut cursor).is_err());
        assert_eq!(sharing_table_counts(&conn), before);
        assert!(cursor.lineage.is_none());
        assert_eq!(cursor.raw_size, 0);
        assert!(cursor.hash.is_empty());
        assert!(!cursor.complete);
        assert!(conn.is_autocommit());
        conn.execute_batch("DROP TRIGGER reject_second_layout")
            .unwrap();
        let step = share_objects(&mut conn, &lineage, &mut cursor).unwrap();
        assert_eq!(step.objects_shared, 4);
        assert!(step.complete);
        assert!(step.pages_saved > 0);
    }

    #[test]
    fn sharing_cursor_bounds_already_shared_scans_and_rejects_foreign_lineages() {
        let (mut conn, lineage) = fixture();
        for index in 0..300 {
            let mut bytes = vec![0; SHARED_OBJECT_MIN_BYTES as usize];
            bytes.extend_from_slice(format!("title-{index:04}").as_bytes());
            let stored = put_object(&conn, &bytes, ObjectCompression::default()).unwrap();
            share_object(
                &mut conn,
                &lineage,
                stored.hash(),
                ObjectCompression::default(),
            )
            .unwrap();
        }
        let mut cursor = ObjectSharingCursor::default();
        let first = share_objects(&mut conn, &lineage, &mut cursor).unwrap();
        assert_eq!(first.objects_scanned, SHARING_SCAN_ROWS);
        assert_eq!(first.raw_bytes_processed, 0);
        assert_eq!(first.objects_shared, 0);
        assert!(!first.complete);
        let foreign = LineageId::from_hex("2".repeat(32)).unwrap();
        assert!(share_objects(&mut conn, &foreign, &mut cursor).is_err());
        let second = share_objects(&mut conn, &lineage, &mut cursor).unwrap();
        assert_eq!(second.objects_scanned, 300 - SHARING_SCAN_ROWS);
        assert!(second.complete);

        // Exercise the actual indexed query at the exhausted position instead
        // of the completed-cursor short circuit. SQL work must not rescan the
        // prefix that was already inspected.
        cursor.complete = false;
        let vm_steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = vm_steps.clone();
        conn.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let exhausted = share_objects(&mut conn, &lineage, &mut cursor).unwrap();
        conn.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert!(exhausted.complete);
        assert_eq!(exhausted.objects_scanned, 0);
        let steps = vm_steps.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            steps < 1000,
            "sharing scan revisited its prefix: {steps} VM steps"
        );
        println!("sharing indexed exhausted seek: {steps} VM steps after 300 shared objects");
    }

    #[test]
    fn sharing_cursor_bounds_raw_work_and_advances_rejected_cohorts() {
        let (mut conn, lineage) = fixture();
        for index in 0..40 {
            let mut bytes = vec![index; 8 * 1024 * 1024];
            bytes.extend_from_slice(b"unique compressed tail");
            put_object(&conn, &bytes, ObjectCompression::default()).unwrap();
        }
        let before = sharing_table_counts(&conn);
        let mut cursor = ObjectSharingCursor::default();
        let mut scanned = 0;
        for _ in 0..3 {
            let step = share_objects(&mut conn, &lineage, &mut cursor).unwrap();
            assert!(step.raw_bytes_processed <= SHARING_COHORT_BYTES);
            assert!(step.objects_scanned > 0);
            assert_eq!(step.objects_shared, 0);
            scanned += step.objects_scanned;
            if step.complete {
                break;
            }
        }
        assert_eq!(scanned, 40);
        assert!(cursor.complete);
        assert_eq!(sharing_table_counts(&conn), before);
    }

    #[test]
    fn sharing_cohort_reclaims_large_archive_objects_together() {
        let (mut conn, lineage) = fixture();
        let archive = binary_fixture(17 * 1024 * 1024);
        let mut originals = Vec::new();
        for index in 0..3 {
            let mut bytes = format!("large-title-{index}").into_bytes();
            bytes.extend_from_slice(&archive);
            originals.push(put_object(&conn, &bytes, ObjectCompression::default()).unwrap());
        }
        let before = occupied_pages(&conn).unwrap();
        let step = share_objects(&mut conn, &lineage, &mut ObjectSharingCursor::default()).unwrap();
        assert!(step.complete);
        assert_eq!(step.objects_shared, originals.len());
        assert!(step.raw_bytes_processed <= SHARING_COHORT_BYTES);
        assert!(step.pages_saved > before / 2);
        for original in originals {
            let stored = object(&conn, original.hash()).unwrap().unwrap();
            assert_eq!(stored.bytes, original.bytes);
            assert_eq!(stored.meta.hash, original.meta.hash);
            assert_eq!(stored.meta.raw_size, original.meta.raw_size);
        }
    }

    #[test]
    fn content_defined_chunks_are_bounded_and_reconstruct_exact_binary_bytes() {
        for len in [
            0,
            1,
            CHUNK_MIN_BYTES - 1,
            CHUNK_MIN_BYTES,
            CHUNK_MAX_BYTES,
            2 * 1024 * 1024,
        ] {
            let bytes = binary_fixture(len);
            let chunks = object_chunks(&bytes);
            assert_eq!(chunks.concat(), bytes);
            assert!(chunks.len() as u64 <= MAX_CHUNKS);
            for (index, chunk) in chunks.iter().enumerate() {
                let minimum = if index + 1 == chunks.len() {
                    1
                } else {
                    CHUNK_MIN_BYTES
                };
                assert!((minimum..=CHUNK_MAX_BYTES).contains(&chunk.len()));
            }
        }
    }

    #[test]
    fn sharing_shifted_binary_objects_preserves_hashes_and_reuses_physical_bytes() {
        let (mut conn, lineage) = fixture();
        let archive = binary_fixture(2 * 1024 * 1024);
        let mut objects = Vec::new();
        for index in 0..4 {
            let mut bytes = format!("title-{index}{}", "x".repeat(index * 7)).into_bytes();
            bytes.extend_from_slice(&archive);
            bytes.extend_from_slice(format!("tail-{index}").as_bytes());
            let stored = put_object(&conn, &bytes, ObjectCompression::none()).unwrap();
            objects.push((stored.meta.hash, bytes));
        }
        let before = physical_bytes(&conn);
        for (hash, bytes) in &objects {
            assert!(share_object(&mut conn, &lineage, hash, ObjectCompression::none()).unwrap());
            assert!(!share_object(&mut conn, &lineage, hash, ObjectCompression::none()).unwrap());
            let stored = object(&conn, hash).unwrap().unwrap();
            assert_eq!(&stored.bytes, bytes);
            assert_eq!(&stored.meta.hash, hash);
            assert_eq!(stored.meta.stored_size, 0);
            let ObjectLayout::DataSequence { root_id, .. } = stored.meta.layout else {
                panic!("expected shared physical layout");
            };
            let root = load_root(&conn, &lineage, &RootId::from_db(root_id).unwrap()).unwrap();
            assert!(root.depth > 1);
            let (_, stats) =
                sequence_payload_refs_from_root(&conn, &lineage, &root, 0, root.item_count)
                    .unwrap();
            assert_eq!(stats.payloads_read, 0);
            validate_sequence(&conn, &lineage, &root).unwrap();
        }
        assert!(
            physical_bytes(&conn) < before / 2,
            "unchanged binary bytes must share across shifted objects"
        );
        assert!(conn
            .execute(
                "DELETE FROM object_data_roots WHERE object_hash = ?1",
                [&objects[0].0]
            )
            .is_err());
        assert!(conn
            .execute(
                "UPDATE objects SET bytes = x'' WHERE hash = ?1",
                [&objects[0].0]
            )
            .is_err());
        crate::schema::validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn failed_sharing_rolls_back_blobs_chunks_roots_and_guards() {
        let (mut conn, lineage) = fixture();
        let bytes = binary_fixture(256 * 1024);
        let stored = put_object(&conn, &bytes, ObjectCompression::none()).unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER reject_object_sharing BEFORE INSERT ON object_data_roots
             BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;",
        )
        .unwrap();
        assert!(share_object(
            &mut conn,
            &lineage,
            stored.hash(),
            ObjectCompression::default()
        )
        .is_err());
        assert_eq!(object(&conn, stored.hash()).unwrap(), Some(stored));
        for table in [
            "object_data_roots",
            "lineage_payload_object_refs",
            "lineage_sequence_roots",
            "lineage_sequence_nodes",
            "lineage_completed_sequence_nodes",
        ] {
            assert_eq!(
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0,
                "partial sharing survived in {table}"
            );
        }
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM objects", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(conn.is_autocommit());
        crate::schema::validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn sharing_repairs_a_data_tree_reused_between_partial_reclamation_steps() {
        let (mut conn, lineage) = fixture();
        let bytes = binary_fixture(256 * 1024);
        let hash = put_object(&conn, &bytes, ObjectCompression::none())
            .unwrap()
            .meta
            .hash;
        share_object(&mut conn, &lineage, &hash, ObjectCompression::none()).unwrap();
        let root_id = conn
            .query_row(
                "SELECT root_id FROM object_data_roots WHERE object_hash = ?1",
                [&hash],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let before = conn
            .query_row("SELECT COUNT(*) FROM lineage_sequence_entries", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        let mut partial = false;
        for _ in 0..super::super::tests::reclamation_step_limit(&conn, &lineage) {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            assert!(step.work_rows() <= 1);
            let remaining = conn
                .query_row("SELECT COUNT(*) FROM lineage_sequence_entries", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap();
            if remaining > 0 && remaining < before {
                partial = true;
                break;
            }
        }
        assert!(partial, "reclamation did not reach a partial data leaf");
        assert!(object(&conn, &hash).unwrap().is_none());
        put_object(&conn, &bytes, ObjectCompression::none()).unwrap();
        conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO request_object_refs VALUES (1, ?1, 'response')",
            [&hash],
        )
        .unwrap();
        assert!(share_object(&mut conn, &lineage, &hash, ObjectCompression::none()).unwrap());
        let restored = conn
            .query_row(
                "SELECT root_id FROM object_data_roots WHERE object_hash = ?1",
                [&hash],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(restored, root_id);
        let root = load_root(&conn, &lineage, &RootId::from_db(restored).unwrap()).unwrap();
        validate_sequence(&conn, &lineage, &root).unwrap();
        assert_eq!(object(&conn, &hash).unwrap().unwrap().bytes, bytes);
        assert_eq!(super::super::tests::reclaim_fixture(&mut conn, &lineage), 0);
        assert!(conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none());
    }

    #[test]
    fn reclamation_protects_shared_request_objects_then_releases_every_chunk() {
        let (mut conn, lineage) = fixture();
        let bytes = binary_fixture(256 * 1024);
        let stored = put_object(&conn, &bytes, ObjectCompression::none()).unwrap();
        conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO request_object_refs VALUES (1, ?1, 'response')",
            [stored.hash()],
        )
        .unwrap();
        share_object(
            &mut conn,
            &lineage,
            stored.hash(),
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(super::super::tests::reclaim_fixture(&mut conn, &lineage), 0);
        assert_eq!(object(&conn, stored.hash()).unwrap().unwrap().bytes, bytes);
        conn.execute("DELETE FROM request_attempts WHERE id = 1", [])
            .unwrap();
        let mut complete = false;
        for _ in 0..1000 {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            assert!(step.work_rows() <= 1);
            if step.complete {
                complete = true;
                break;
            }
        }
        assert!(
            complete,
            "shared-object reclamation did not make bounded progress"
        );
        for table in [
            "objects",
            "object_data_roots",
            "lineage_payload_object_refs",
            "lineage_sequence_roots",
            "lineage_sequence_nodes",
            "lineage_completed_sequence_nodes",
        ] {
            assert_eq!(
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0,
                "unreachable sharing survived in {table}"
            );
        }
        assert!(conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none());
        crate::schema::validate_lineage_schema(&conn).unwrap();
    }

    #[test]
    fn physical_object_sharing_is_crash_atomic_at_publication_boundaries() {
        const ROLE: &str = "SMELT_OBJECT_SHARING_CRASH_ROLE";
        const DB: &str = "SMELT_OBJECT_SHARING_CRASH_DB";
        let bytes = binary_fixture(1024 * 1024);
        let hash = sha256_hex(&bytes);
        let mut cohort = vec![bytes.clone()];
        for index in 0..3 {
            let mut member = format!("cohort-title-{index}").into_bytes();
            member.extend_from_slice(&bytes);
            cohort.push(member);
        }
        if let (Ok(role), Ok(path)) = (std::env::var(ROLE), std::env::var(DB)) {
            let mut conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
            )
            .unwrap();
            conn.create_scalar_function(
                "smelt_test_object_crash",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |_| -> rusqlite::Result<i64> { std::process::abort() },
            )
            .unwrap();
            let is_cohort = role.starts_with("cohort-");
            let boundary = role.strip_prefix("cohort-").unwrap_or(&role);
            if boundary == "commit" {
                conn.commit_hook(Some(|| -> bool { std::process::abort() }))
                    .unwrap();
            } else if boundary != "after-commit" {
                let event = match boundary {
                    "chunk" => "AFTER INSERT ON lineage_payload_object_refs",
                    "node" => "AFTER INSERT ON lineage_sequence_nodes",
                    "root" => "AFTER INSERT ON lineage_sequence_roots",
                    "blob" => "AFTER UPDATE ON objects",
                    "layout" => "AFTER INSERT ON object_data_roots",
                    _ => panic!("unknown sharing crash boundary"),
                };
                // Cohort crashes occur during the second member, after the first
                // layout has been published inside the same transaction.
                let condition = if is_cohort && boundary == "layout" {
                    "WHEN (SELECT COUNT(*) FROM object_data_roots) = 2"
                } else if is_cohort {
                    "WHEN (SELECT COUNT(*) FROM object_data_roots) = 1"
                } else {
                    ""
                };
                conn.execute_batch(&format!("CREATE TEMP TRIGGER crash_object_sharing {event} {condition} BEGIN SELECT smelt_test_object_crash(); END;")).unwrap();
            }
            let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
            if is_cohort {
                let step = share_objects(&mut conn, &lineage, &mut ObjectSharingCursor::default())
                    .unwrap();
                assert_eq!(step.objects_shared, cohort.len());
                if boundary == "after-commit" {
                    std::process::abort();
                }
                panic!("sharing crash boundary was not reached: {step:?}");
            }
            let result = share_object(&mut conn, &lineage, &hash, ObjectCompression::none());
            panic!("sharing crash boundary was not reached: {result:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        for role in [
            "chunk",
            "node",
            "root",
            "blob",
            "layout",
            "cohort-chunk",
            "cohort-node",
            "cohort-root",
            "cohort-blob",
            "cohort-layout",
            "cohort-commit",
            "cohort-after-commit",
        ] {
            let is_cohort = role.starts_with("cohort-");
            let path = dir.path().join(format!("sharing-{role}.db"));
            {
                let mut conn = Connection::open(&path).unwrap();
                conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;").unwrap();
                crate::schema::initialize_lineage_schema(&mut conn).unwrap();
                let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
                create_lineage(&conn, &lineage, 1).unwrap();
                for member in if is_cohort { &cohort[..] } else { &cohort[..1] } {
                    put_object(&conn, member, ObjectCompression::none()).unwrap();
                }
            }
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("lineage::object_storage::tests::physical_object_sharing_is_crash_atomic_at_publication_boundaries")
                .arg("--nocapture")
                .env(ROLE, role).env(DB, &path)
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                .status().unwrap();
            assert!(!status.success(), "child did not crash at {role}");
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(
                    status.signal(),
                    Some(libc::SIGABRT),
                    "wrong failure at {role}"
                );
            }
            let mut conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
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
            crate::schema::validate_lineage_schema(&conn).unwrap();
            for member in if is_cohort { &cohort[..] } else { &cohort[..1] } {
                let stored = object(&conn, &sha256_hex(member)).unwrap().unwrap();
                assert_eq!(&stored.bytes, member);
                if role == "cohort-after-commit" {
                    assert!(matches!(
                        stored.meta.layout,
                        ObjectLayout::DataSequence { .. }
                    ));
                } else {
                    assert_eq!(stored.meta.layout, ObjectLayout::Blob);
                }
            }
            if role == "cohort-after-commit" {
                let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
                let step = share_objects(&mut conn, &lineage, &mut ObjectSharingCursor::default())
                    .unwrap();
                assert!(step.complete);
                assert_eq!(step.objects_shared, 0);
                continue;
            }
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM objects", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                if is_cohort { cohort.len() as i64 } else { 1 }
            );
            for table in [
                "object_data_roots",
                "lineage_payload_object_refs",
                "lineage_sequence_roots",
                "lineage_sequence_nodes",
                "lineage_completed_sequence_nodes",
            ] {
                assert_eq!(
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                        .unwrap(),
                    0,
                    "partial publication survived {role} in {table}"
                );
            }
            let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
            if is_cohort {
                let step = share_objects(&mut conn, &lineage, &mut ObjectSharingCursor::default())
                    .unwrap();
                assert!(step.complete);
                assert_eq!(step.objects_shared, cohort.len());
                for member in &cohort {
                    assert_eq!(
                        object(&conn, &sha256_hex(member)).unwrap().unwrap().bytes,
                        *member
                    );
                }
            } else {
                assert!(
                    share_object(&mut conn, &lineage, &hash, ObjectCompression::none()).unwrap()
                );
                assert_eq!(object(&conn, &hash).unwrap().unwrap().bytes, bytes);
            }
        }
    }

    #[test]
    fn hydration_rejects_corrupt_chunks_and_recursive_physical_layouts() {
        for recursive in [false, true] {
            let (mut conn, lineage) = fixture();
            let bytes = binary_fixture(256 * 1024);
            let stored = put_object(&conn, &bytes, ObjectCompression::none()).unwrap();
            share_object(
                &mut conn,
                &lineage,
                stored.hash(),
                ObjectCompression::none(),
            )
            .unwrap();
            let root_id = conn
                .query_row(
                    "SELECT root_id FROM object_data_roots WHERE object_hash = ?1",
                    [stored.hash()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap();
            let root =
                load_root(&conn, &lineage, &RootId::from_db(root_id.clone()).unwrap()).unwrap();
            let (refs, _) = sequence_payload_refs_from_root(&conn, &lineage, &root, 0, 1).unwrap();
            let hash = &refs[0].object_hash;
            if recursive {
                let guard = conn
                    .query_row(
                        "SELECT sql FROM sqlite_schema WHERE name = 'object_data_root_insert'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap();
                conn.execute_batch("DROP TRIGGER object_data_root_insert")
                    .unwrap();
                conn.execute("UPDATE objects SET bytes = x'', codec = 'none', stored_size = 0 WHERE hash = ?1", [hash]).unwrap();
                conn.execute(
                    "INSERT INTO object_data_roots VALUES (?1, ?2, ?3)",
                    (hash, lineage.as_str(), root_id),
                )
                .unwrap();
                conn.execute_batch(&guard).unwrap();
            } else {
                conn.execute(
                    "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
                    [hash],
                )
                .unwrap();
            }
            assert!(matches!(
                object(&conn, stored.hash()),
                Err(StoreError::Integrity(_))
            ));
            crate::schema::validate_lineage_schema(&conn).unwrap();
        }
    }

    #[test]
    fn snapshot_reads_remain_available_while_physical_publication_is_paused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sharing.db");
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
            .unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        create_lineage(&conn, &lineage, 1).unwrap();
        let bytes = binary_fixture(256 * 1024);
        let hash = put_object(&conn, &bytes, ObjectCompression::none())
            .unwrap()
            .meta
            .hash;
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        conn.create_scalar_function(
            "pause_object_publication",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |_| -> rusqlite::Result<i64> {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(0)
            },
        )
        .unwrap();
        conn.execute_batch("CREATE TEMP TRIGGER pause_sharing AFTER UPDATE OF bytes ON objects BEGIN SELECT pause_object_publication(); END;").unwrap();
        let mut reader = Connection::open(path).unwrap();
        reader.busy_timeout(std::time::Duration::ZERO).unwrap();
        reader.set_transaction_behavior(TransactionBehavior::Immediate);
        let target = hash.clone();
        let worker = std::thread::spawn(move || {
            share_object(&mut conn, &lineage, &target, ObjectCompression::none())
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let during = object(&reader, &hash);
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap().unwrap());
        let during = during.unwrap().unwrap();
        assert_eq!(during.meta.layout, ObjectLayout::Blob);
        assert_eq!(during.bytes, bytes);
        let after = object(&reader, &hash).unwrap().unwrap();
        assert!(matches!(
            after.meta.layout,
            ObjectLayout::DataSequence { .. }
        ));
        assert_eq!(after.bytes, bytes);
    }

    #[test]
    fn hydration_resolves_a_layout_replaced_after_metadata_was_read() {
        let (mut conn, lineage) = fixture();
        let bytes = binary_fixture(256 * 1024);
        let stored = put_object(&conn, &bytes, ObjectCompression::none()).unwrap();
        let old_meta = stored.meta;
        share_object(
            &mut conn,
            &lineage,
            &old_meta.hash,
            ObjectCompression::default(),
        )
        .unwrap();
        assert_eq!(
            crate::object::object_bytes(&conn, &old_meta).unwrap(),
            bytes
        );
        assert_eq!(object(&conn, &old_meta.hash).unwrap().unwrap().bytes, bytes);
    }
}
