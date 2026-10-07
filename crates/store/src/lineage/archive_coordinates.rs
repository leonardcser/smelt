use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ArchiveHeaderKind {
    Metadata,
    Checkpoint,
}

impl ArchiveHeaderKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Checkpoint => "checkpoint",
        }
    }

    fn from_db(value: &str) -> Result<Self> {
        match value {
            "metadata" => Ok(Self::Metadata),
            "checkpoint" => Ok(Self::Checkpoint),
            _ => Err(StoreError::Integrity("invalid archive header kind".into())),
        }
    }
}

// Invalid values remain in the authoritative header, not in a bounded numeric
// projection. Missing and null are distinct; every valid unsigned value is exact.
#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) enum ArchiveCoordinate {
    Missing,
    Null,
    Invalid,
    Unsigned(u64),
}

impl ArchiveCoordinate {
    pub(super) fn field(fields: &serde_json::Value, key: &str) -> Self {
        match fields.get(key) {
            None => Self::Missing,
            Some(serde_json::Value::Null) => Self::Null,
            Some(value) => value.as_u64().map_or(Self::Invalid, Self::Unsigned),
        }
    }
}

#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ArchiveCoordinates {
    Metadata {
        index: HistoryIndex,
    },
    Checkpoint {
        first_live_index: ArchiveCoordinate,
        completed_at_history_len: ArchiveCoordinate,
        created_at_ms: ArchiveCoordinate,
    },
}

impl ArchiveCoordinates {
    fn kind(&self) -> ArchiveHeaderKind {
        match self {
            Self::Metadata { .. } => ArchiveHeaderKind::Metadata,
            Self::Checkpoint { .. } => ArchiveHeaderKind::Checkpoint,
        }
    }
}

fn coordinate_id(lineage: &LineageId, payload: &PayloadRef, json: &str) -> String {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-archive-coordinates-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(payload.id.as_str());
    encoder.str(payload.object_hash.as_str());
    encoder.u64(payload.byte_count);
    encoder.str(json);
    encoder.hash()
}

fn load_archive_coordinates(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadRef,
    kind: ArchiveHeaderKind,
) -> Result<Option<ArchiveCoordinates>> {
    if payload.kind != PayloadKind::Data
        || load_payload_ref(conn, lineage, &payload.id)? != *payload
    {
        return Err(StoreError::Integrity(
            "invalid archive header payload".into(),
        ));
    }
    let row = conn
        .query_row(
            "SELECT header_kind, coordinates_json, coordinate_id FROM lineage_archive_coordinates
         WHERE lineage_id = ?1 AND header_payload_id = ?2",
            (lineage.as_str(), payload.id.as_str()),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_kind, json, proof)) = row else {
        return Ok(None);
    };
    if json.len() > 512
        || ArchiveHeaderKind::from_db(&stored_kind)? != kind
        || proof != coordinate_id(lineage, payload, &json)
    {
        return Err(StoreError::Integrity(
            "invalid archive coordinate proof".into(),
        ));
    }
    let coordinates: ArchiveCoordinates = serde_json::from_str(&json)?;
    if coordinates.kind() != kind || serde_json::to_string(&coordinates)? != json {
        return Err(StoreError::Integrity(
            "invalid archive coordinate encoding".into(),
        ));
    }
    Ok(Some(coordinates))
}

pub(super) fn publish_archive_coordinates(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadRef,
    kind: ArchiveHeaderKind,
    bytes: &[u8],
) -> Result<ArchiveCoordinates> {
    if conn.is_autocommit() {
        return Err(StoreError::Integrity(
            "archive coordinate publication requires a write transaction".into(),
        ));
    }
    if bytes.len() as u64 != payload.byte_count || sha256_hex(bytes) != payload.object_hash {
        return Err(StoreError::Integrity(
            "archive coordinates differ from their source bytes".into(),
        ));
    }
    let derived = derive_archive_coordinates(kind, bytes)?;
    if let Some(stored) = load_archive_coordinates(conn, lineage, payload, kind)? {
        if stored != derived {
            return Err(StoreError::Integrity(
                "archive coordinates differ from their header".into(),
            ));
        }
        return Ok(stored);
    }
    let json = serde_json::to_string(&derived)?;
    conn.execute(
        "INSERT INTO lineage_archive_coordinates
         (lineage_id, header_payload_id, header_kind, coordinates_json, coordinate_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            lineage.as_str(),
            payload.id.as_str(),
            kind.as_str(),
            &json,
            coordinate_id(lineage, payload, &json),
        ),
    )?;
    Ok(derived)
}

pub(super) fn archive_header_coordinates(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadRef,
    kind: ArchiveHeaderKind,
    stats: &mut OperationStats,
) -> Result<ArchiveCoordinates> {
    if let Some(coordinates) = load_archive_coordinates(conn, lineage, payload, kind)? {
        return Ok(coordinates);
    }
    // A missing legacy projection is a verified cold transition. It is never
    // guessed or backfilled by a schema migration.
    let bytes = hydrate_payload_ref(conn, payload, PayloadKind::Data, stats)?;
    publish_archive_coordinates(conn, lineage, payload, kind, &bytes)
}

fn summary_presence_id(lineage: &LineageId, payload: &PayloadRef, present: bool) -> String {
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-checkpoint-summary-presence-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(payload.id.as_str());
    encoder.str(payload.object_hash.as_str());
    encoder.u64(payload.byte_count);
    encoder.u64(u64::from(present));
    encoder.hash()
}

fn load_checkpoint_summary_presence(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadRef,
) -> Result<Option<bool>> {
    if payload.kind != PayloadKind::Data
        || load_payload_ref(conn, lineage, &payload.id)? != *payload
    {
        return Err(StoreError::Integrity(
            "invalid checkpoint header payload".into(),
        ));
    }
    let row = conn
        .query_row(
            "SELECT has_summary, presence_id FROM lineage_checkpoint_summary_presence
         WHERE lineage_id = ?1 AND header_payload_id = ?2",
            (lineage.as_str(), payload.id.as_str()),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((present, proof)) = row else {
        return Ok(None);
    };
    if !matches!(present, 0 | 1) || proof != summary_presence_id(lineage, payload, present == 1) {
        return Err(StoreError::Integrity(
            "invalid checkpoint summary presence proof".into(),
        ));
    }
    Ok(Some(present == 1))
}

pub(super) fn publish_checkpoint_summary_presence(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadRef,
    bytes: &[u8],
) -> Result<bool> {
    if conn.is_autocommit() {
        return Err(StoreError::Integrity(
            "checkpoint summary presence publication requires a write transaction".into(),
        ));
    }
    if bytes.len() as u64 != payload.byte_count || sha256_hex(bytes) != payload.object_hash {
        return Err(StoreError::Integrity(
            "checkpoint summary presence differs from its source bytes".into(),
        ));
    }
    let present = derive_checkpoint_summary_presence(bytes)?;
    if let Some(stored) = load_checkpoint_summary_presence(conn, lineage, payload)? {
        if stored != present {
            return Err(StoreError::Integrity(
                "checkpoint summary presence differs from its header".into(),
            ));
        }
        return Ok(stored);
    }
    conn.execute(
        "INSERT INTO lineage_checkpoint_summary_presence
         (lineage_id, header_payload_id, has_summary, presence_id) VALUES (?1, ?2, ?3, ?4)",
        (
            lineage.as_str(),
            payload.id.as_str(),
            present,
            summary_presence_id(lineage, payload, present),
        ),
    )?;
    Ok(present)
}

pub(super) fn checkpoint_summary_presence(
    conn: &Connection,
    lineage: &LineageId,
    payload: &PayloadRef,
    stats: &mut OperationStats,
) -> Result<bool> {
    if let Some(present) = load_checkpoint_summary_presence(conn, lineage, payload)? {
        return Ok(present);
    }
    let bytes = hydrate_payload_ref(conn, payload, PayloadKind::Data, stats)?;
    publish_checkpoint_summary_presence(conn, lineage, payload, &bytes)
}

pub(crate) fn verify_archive_coordinates(conn: &Connection, lineage: &LineageId) -> Result<()> {
    let _read = conn
        .is_autocommit()
        .then(|| Transaction::new_unchecked(conn, TransactionBehavior::Deferred))
        .transpose()?;
    if crate::schema::user_version(conn)? == 3 {
        return Ok(());
    }
    let headers = conn.prepare(
        "SELECT header_payload_id, header_kind FROM lineage_archive_coordinates WHERE lineage_id = ?1",
    )?.query_map([lineage.as_str()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, kind) in headers {
        let kind = ArchiveHeaderKind::from_db(&kind)?;
        let payload = load_payload_ref(conn, lineage, &PayloadId::from_db(id)?)?;
        let stored = load_archive_coordinates(conn, lineage, &payload, kind)?.ok_or_else(|| {
            StoreError::Integrity("archive coordinates disappeared from read snapshot".into())
        })?;
        let bytes = hydrate_payload_ref(
            conn,
            &payload,
            PayloadKind::Data,
            &mut OperationStats::default(),
        )?;
        if stored != derive_archive_coordinates(kind, &bytes)? {
            return Err(StoreError::Integrity(
                "archive coordinates differ from their authoritative header".into(),
            ));
        }
    }
    if crate::schema::has_shared_storage(conn)? {
        let headers = conn.prepare(
            "SELECT header_payload_id FROM lineage_checkpoint_summary_presence WHERE lineage_id = ?1",
        )?.query_map([lineage.as_str()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in headers {
            let payload = load_payload_ref(conn, lineage, &PayloadId::from_db(id)?)?;
            let stored =
                load_checkpoint_summary_presence(conn, lineage, &payload)?.ok_or_else(|| {
                    StoreError::Integrity(
                        "checkpoint summary presence disappeared from read snapshot".into(),
                    )
                })?;
            let bytes = hydrate_payload_ref(
                conn,
                &payload,
                PayloadKind::Data,
                &mut OperationStats::default(),
            )?;
            if stored != derive_checkpoint_summary_presence(&bytes)? {
                return Err(StoreError::Integrity(
                    "checkpoint summary presence differs from its authoritative header".into(),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn fixture() -> (Connection, LineageId) {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        create_lineage(&conn, &lineage, 1).unwrap();
        (conn, lineage)
    }

    fn header(kind: ArchiveHeaderKind, index: u64, fields: Value) -> Vec<u8> {
        serde_json::to_vec(&match kind {
            ArchiveHeaderKind::Metadata => json!({
                "index": index, "fields": fields, "has_first_user_message": false,
            }),
            ArchiveHeaderKind::Checkpoint => json!({"fields": fields, "has_summary": false}),
        })
        .unwrap()
    }

    fn install(
        conn: &mut Connection,
        lineage: &LineageId,
        kind: ArchiveHeaderKind,
        bytes: &[u8],
        compression: ObjectCompression,
    ) -> PayloadRef {
        let tx = conn.transaction().unwrap();
        let payload = put_payload(
            &tx,
            lineage,
            PayloadKind::Data,
            bytes,
            compression,
            &mut OperationStats::default(),
        )
        .unwrap();
        publish_archive_coordinates(&tx, lineage, &payload, kind, bytes).unwrap();
        if kind == ArchiveHeaderKind::Checkpoint {
            publish_checkpoint_summary_presence(&tx, lineage, &payload, bytes).unwrap();
        }
        tx.commit().unwrap();
        payload
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM lineage_archive_coordinates",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn checkpoint_summary_presence_rejects_wrong_kinds_and_invalid_headers() {
        let (mut conn, lineage) = fixture();
        for value in [
            json!(null),
            json!({"fields":{}}),
            json!({"fields":{},"has_summary":"true"}),
            json!({"fields":null,"has_summary":true}),
            json!({"fields":{"summary":null},"has_summary":true}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            let tx = conn.transaction().unwrap();
            let payload = put_payload(
                &tx,
                &lineage,
                PayloadKind::Data,
                &bytes,
                ObjectCompression::none(),
                &mut OperationStats::default(),
            )
            .unwrap();
            assert!(publish_checkpoint_summary_presence(&tx, &lineage, &payload, &bytes).is_err());
        }
        let tx = conn.transaction().unwrap();
        let bytes = serde_json::to_vec(&json!({"fields":{},"has_summary":true})).unwrap();
        let payload = put_payload(
            &tx,
            &lineage,
            PayloadKind::History,
            &bytes,
            ObjectCompression::none(),
            &mut OperationStats::default(),
        )
        .unwrap();
        assert!(publish_checkpoint_summary_presence(&tx, &lineage, &payload, &bytes).is_err());
        assert!(tx
            .execute(
                "INSERT INTO lineage_checkpoint_summary_presence VALUES (?1, ?2, 1, ?3)",
                (lineage.as_str(), payload.id.as_str(), "0".repeat(64))
            )
            .is_err());
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM lineage_checkpoint_summary_presence",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn checkpoint_summary_presence_lookup_is_bounded_and_authoritative_audit_detects_forgery() {
        for compression in [ObjectCompression::none(), ObjectCompression::zstd(3, 1, 0)] {
            let mut counts = Vec::new();
            for size in [0, 262144, 1048576] {
                let (mut conn, lineage) = fixture();
                let bytes = serde_json::to_vec(
                    &json!({"fields":{"unknown":"x".repeat(size)},"has_summary":true}),
                )
                .unwrap();
                let payload = install(
                    &mut conn,
                    &lineage,
                    ArchiveHeaderKind::Checkpoint,
                    &bytes,
                    compression,
                );
                let steps = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let counter = steps.clone();
                conn.progress_handler(
                    1,
                    Some(move || {
                        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        false
                    }),
                )
                .unwrap();
                let mut stats = OperationStats::default();
                assert!(
                    checkpoint_summary_presence(&conn, &lineage, &payload, &mut stats).unwrap()
                );
                let steps = steps.load(std::sync::atomic::Ordering::Relaxed);
                conn.progress_handler(0, None::<fn() -> bool>).unwrap();
                assert_eq!(stats.payloads_read, 0);
                counts.push(steps);
                eprintln!(
                    "CHECKPOINT_SUMMARY_PRESENCE header_bytes={} vm_steps={steps} payloads_read=0",
                    bytes.len()
                );
                for sql in [
                    "UPDATE lineage_checkpoint_summary_presence SET has_summary = 0",
                    "DELETE FROM lineage_checkpoint_summary_presence",
                ] {
                    assert!(conn.execute_batch(sql).is_err());
                }
                let mut forged = payload.clone();
                forged.byte_count += 1;
                assert!(checkpoint_summary_presence(&conn, &lineage, &forged, &mut stats).is_err());
                let foreign = LineageId::from_hex("2".repeat(32)).unwrap();
                assert!(
                    checkpoint_summary_presence(&conn, &foreign, &payload, &mut stats).is_err()
                );
                conn.execute_batch("DROP TRIGGER lineage_checkpoint_summary_presence_update; UPDATE lineage_checkpoint_summary_presence SET has_summary = 0").unwrap();
                assert!(
                    checkpoint_summary_presence(&conn, &lineage, &payload, &mut stats).is_err()
                );
                conn.execute(
                    "UPDATE lineage_checkpoint_summary_presence SET presence_id = ?1",
                    [summary_presence_id(&lineage, &payload, false)],
                )
                .unwrap();
                assert!(
                    !checkpoint_summary_presence(&conn, &lineage, &payload, &mut stats).unwrap()
                );
                assert!(verify_archive_coordinates(&conn, &lineage).is_err());
                let backup = tempfile::tempdir().unwrap();
                let path = backup.path().join("summary-presence.db");
                crate::diagnostics::backup_connection_to(&conn, &path).unwrap();
                assert!(crate::verify_lineage_backup(&path, lineage.as_str())
                    .unwrap()
                    .issues
                    .iter()
                    .any(|issue| issue.contains("summary presence")));
                let tx = conn.transaction().unwrap();
                assert!(
                    publish_checkpoint_summary_presence(&tx, &lineage, &payload, &bytes).is_err()
                );
            }
            assert!(counts.iter().all(|steps| *steps == counts[0]));
            assert!(counts[0] < 100);
        }
    }

    #[test]
    fn checkpoint_summary_presence_missing_proof_is_lazy_verified_and_parent_owned() {
        let (mut conn, lineage) = fixture();
        let bytes = serde_json::to_vec(
            &json!({"fields":{"unknown":"α".repeat(262144)},"has_summary":true}),
        )
        .unwrap();
        let payload = put_payload(
            &conn,
            &lineage,
            PayloadKind::Data,
            &bytes,
            ObjectCompression::zstd(3, 1, 0),
            &mut OperationStats::default(),
        )
        .unwrap();
        conn.pragma_update(None, "query_only", true).unwrap();
        crate::schema::validate_lineage_schema(&conn).unwrap();
        let mut stats = OperationStats::default();
        assert!(matches!(
            checkpoint_summary_presence(&conn, &lineage, &payload, &mut stats),
            Err(StoreError::Integrity(message)) if message.contains("requires a write transaction")
        ));
        assert_eq!(stats.payloads_read, 1);
        conn.pragma_update(None, "query_only", false).unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let count = |conn: &Connection| {
            conn.query_row(
                "SELECT count(*) FROM lineage_checkpoint_summary_presence",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(count(&conn), 0);
        assert!(publish_checkpoint_summary_presence(&conn, &lineage, &payload, &bytes).is_err());
        {
            let tx = conn.transaction().unwrap();
            assert!(checkpoint_summary_presence(
                &tx,
                &lineage,
                &payload,
                &mut OperationStats::default()
            )
            .unwrap());
            assert_eq!(count(&tx), 1);
        }
        assert_eq!(count(&conn), 0);
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT bytes FROM objects WHERE hash = ?1",
                [&payload.object_hash],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
            [&payload.object_hash],
        )
        .unwrap();
        {
            let tx = conn.transaction().unwrap();
            assert!(checkpoint_summary_presence(
                &tx,
                &lineage,
                &payload,
                &mut OperationStats::default()
            )
            .is_err());
        }
        assert_eq!(count(&conn), 0);
        conn.execute(
            "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
            (&stored, &payload.object_hash),
        )
        .unwrap();
        {
            let tx = conn.transaction().unwrap();
            assert!(checkpoint_summary_presence(
                &tx,
                &lineage,
                &payload,
                &mut OperationStats::default()
            )
            .unwrap());
            tx.commit().unwrap();
        }
        let mut stats = OperationStats::default();
        assert!(checkpoint_summary_presence(&conn, &lineage, &payload, &mut stats).unwrap());
        assert_eq!(stats.payloads_read, 0);
        verify_archive_coordinates(&conn, &lineage).unwrap();
        conn.execute(
            "DELETE FROM lineage_payload_object_refs WHERE payload_id = ?1",
            [payload.id.as_str()],
        )
        .unwrap();
        assert_eq!(count(&conn), 0);
        let replacement = install(
            &mut conn,
            &lineage,
            ArchiveHeaderKind::Checkpoint,
            &bytes,
            ObjectCompression::none(),
        );
        assert_eq!(replacement, payload);
        assert_eq!(count(&conn), 1);
    }

    #[test]
    fn archive_coordinates_preserve_unsigned_values_and_legacy_shapes_without_changing_bytes() {
        let (mut conn, lineage) = fixture();
        for index in [0, i64::MAX as u64, u64::MAX] {
            for fields in [
                Value::Null,
                json!([]),
                json!({}),
                json!({"first_user_message": null}),
                json!({"unknown": "α\n\0".repeat(4096)}),
            ] {
                let bytes = header(ArchiveHeaderKind::Metadata, index, fields);
                let payload = install(
                    &mut conn,
                    &lineage,
                    ArchiveHeaderKind::Metadata,
                    &bytes,
                    ObjectCompression::none(),
                );
                assert_eq!(
                    archive_header_coordinates(
                        &conn,
                        &lineage,
                        &payload,
                        ArchiveHeaderKind::Metadata,
                        &mut OperationStats::default()
                    )
                    .unwrap(),
                    ArchiveCoordinates::Metadata {
                        index: HistoryIndex::new(index)
                    }
                );
                assert_eq!(
                    object(&conn, &payload.object_hash).unwrap().unwrap().bytes,
                    bytes
                );
            }
        }
        for (value, expected) in [
            (json!(null), ArchiveCoordinate::Null),
            (json!(false), ArchiveCoordinate::Invalid),
            (json!("1"), ArchiveCoordinate::Invalid),
            (json!(-1), ArchiveCoordinate::Invalid),
            (json!(1.25), ArchiveCoordinate::Invalid),
            (json!([]), ArchiveCoordinate::Invalid),
            (json!({}), ArchiveCoordinate::Invalid),
            (json!(0), ArchiveCoordinate::Unsigned(0)),
            (
                json!(i64::MAX),
                ArchiveCoordinate::Unsigned(i64::MAX as u64),
            ),
            (json!(u64::MAX), ArchiveCoordinate::Unsigned(u64::MAX)),
        ] {
            let bytes = header(
                ArchiveHeaderKind::Checkpoint,
                0,
                json!({
                    "first_live_index": value, "created_at_ms": null, "unknown": [null, true, "α"],
                }),
            );
            let payload = install(
                &mut conn,
                &lineage,
                ArchiveHeaderKind::Checkpoint,
                &bytes,
                ObjectCompression::none(),
            );
            assert_eq!(
                archive_header_coordinates(
                    &conn,
                    &lineage,
                    &payload,
                    ArchiveHeaderKind::Checkpoint,
                    &mut OperationStats::default()
                )
                .unwrap(),
                ArchiveCoordinates::Checkpoint {
                    first_live_index: expected,
                    completed_at_history_len: ArchiveCoordinate::Missing,
                    created_at_ms: ArchiveCoordinate::Null,
                }
            );
            assert_eq!(
                object(&conn, &payload.object_hash).unwrap().unwrap().bytes,
                bytes
            );
        }
        for fields in [json!(null), json!([]), json!("legacy")] {
            let bytes = header(ArchiveHeaderKind::Checkpoint, 0, fields);
            let payload = install(
                &mut conn,
                &lineage,
                ArchiveHeaderKind::Checkpoint,
                &bytes,
                ObjectCompression::none(),
            );
            assert_eq!(
                archive_header_coordinates(
                    &conn,
                    &lineage,
                    &payload,
                    ArchiveHeaderKind::Checkpoint,
                    &mut OperationStats::default()
                )
                .unwrap(),
                ArchiveCoordinates::Checkpoint {
                    first_live_index: ArchiveCoordinate::Missing,
                    completed_at_history_len: ArchiveCoordinate::Missing,
                    created_at_ms: ArchiveCoordinate::Missing,
                }
            );
        }
        verify_archive_coordinates(&conn, &lineage).unwrap();
    }

    #[test]
    fn archive_coordinate_hot_lookup_is_body_independent_and_cold_audit_checks_source() {
        for compression in [ObjectCompression::none(), ObjectCompression::zstd(3, 0, 0)] {
            for kind in [ArchiveHeaderKind::Metadata, ArchiveHeaderKind::Checkpoint] {
                for size in [128, 262144, 1048576] {
                    let (mut conn, lineage) = fixture();
                    let bytes = header(
                        kind,
                        42,
                        json!({"first_live_index": 7,
                            "completed_at_history_len": 42, "created_at_ms": u64::MAX,
                            "unknown": "x".repeat(size),
                        }),
                    );
                    let payload = install(&mut conn, &lineage, kind, &bytes, compression);
                    let stored: Vec<u8> = conn
                        .query_row(
                            "SELECT bytes FROM objects WHERE hash = ?1",
                            [&payload.object_hash],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let expected = derive_archive_coordinates(kind, &bytes).unwrap();
                    let steps = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let counter = steps.clone();
                    conn.progress_handler(
                        1,
                        Some(move || {
                            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            false
                        }),
                    )
                    .unwrap();
                    let mut stats = OperationStats::default();
                    assert_eq!(
                        archive_header_coordinates(&conn, &lineage, &payload, kind, &mut stats)
                            .unwrap(),
                        expected
                    );
                    conn.progress_handler(0, None::<fn() -> bool>).unwrap();
                    let steps = steps.load(std::sync::atomic::Ordering::Relaxed);
                    eprintln!(
                        "kind={kind:?} header_bytes={} vm_steps={steps} payloads_read={}",
                        bytes.len(),
                        stats.payloads_read
                    );
                    assert!(steps < 256, "lookup must stay indexed and bounded");
                    assert_eq!(stats.payloads_read, 0);
                    conn.execute(
                        "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
                        [&payload.object_hash],
                    )
                    .unwrap();
                    assert_eq!(
                        archive_header_coordinates(&conn, &lineage, &payload, kind, &mut stats)
                            .unwrap(),
                        expected
                    );
                    assert!(verify_archive_coordinates(&conn, &lineage).is_err());
                    conn.execute(
                        "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
                        (&stored, &payload.object_hash),
                    )
                    .unwrap();
                    verify_archive_coordinates(&conn, &lineage).unwrap();
                    assert_eq!(
                        object(&conn, &payload.object_hash).unwrap().unwrap().bytes,
                        bytes
                    );
                }
            }
        }
    }

    #[test]
    fn archive_coordinates_reject_forgery_wrong_kind_and_lineage_and_audit_false_derivations() {
        let (mut conn, lineage) = fixture();
        let kind = ArchiveHeaderKind::Metadata;
        let bytes = header(kind, 42, json!({"unknown": "preserved"}));
        let payload = install(&mut conn, &lineage, kind, &bytes, ObjectCompression::none());
        assert!(archive_header_coordinates(
            &conn,
            &lineage,
            &payload,
            ArchiveHeaderKind::Checkpoint,
            &mut OperationStats::default()
        )
        .is_err());
        let other = LineageId::from_hex("2".repeat(32)).unwrap();
        let mut foreign_conn = Connection::open_in_memory().unwrap();
        foreign_conn
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        crate::schema::initialize_lineage_schema(&mut foreign_conn).unwrap();
        create_lineage(&foreign_conn, &other, 1).unwrap();
        let foreign_payload = install(
            &mut foreign_conn,
            &other,
            kind,
            &bytes,
            ObjectCompression::none(),
        );
        assert_ne!(foreign_payload.id, payload.id);
        assert!(archive_header_coordinates(
            &conn,
            &lineage,
            &foreign_payload,
            kind,
            &mut OperationStats::default()
        )
        .is_err());
        assert!(archive_header_coordinates(
            &conn,
            &other,
            &payload,
            kind,
            &mut OperationStats::default()
        )
        .is_err());
        let mut forged = payload.clone();
        forged.byte_count += 1;
        assert!(archive_header_coordinates(
            &conn,
            &lineage,
            &forged,
            kind,
            &mut OperationStats::default()
        )
        .is_err());
        for sql in [
            "UPDATE lineage_archive_coordinates SET coordinates_json = '{}'",
            "DELETE FROM lineage_archive_coordinates",
        ] {
            assert!(conn.execute(sql, []).is_err());
        }
        conn.execute_batch("DROP TRIGGER lineage_archive_coordinate_update")
            .unwrap();
        let json = serde_json::to_string(&ArchiveCoordinates::Metadata {
            index: HistoryIndex::new(41),
        })
        .unwrap();
        conn.execute(
            "UPDATE lineage_archive_coordinates SET coordinates_json = ?1",
            [&json],
        )
        .unwrap();
        assert!(archive_header_coordinates(
            &conn,
            &lineage,
            &payload,
            kind,
            &mut OperationStats::default()
        )
        .is_err());
        conn.execute(
            "UPDATE lineage_archive_coordinates SET coordinate_id = ?1",
            [coordinate_id(&lineage, &payload, &json)],
        )
        .unwrap();
        assert_eq!(
            archive_header_coordinates(
                &conn,
                &lineage,
                &payload,
                kind,
                &mut OperationStats::default()
            )
            .unwrap(),
            ArchiveCoordinates::Metadata {
                index: HistoryIndex::new(41)
            }
        );
        assert!(matches!(verify_archive_coordinates(&conn, &lineage),
            Err(StoreError::Integrity(message)) if message.contains("authoritative header")));
        let backup = tempfile::tempdir().unwrap();
        let path = backup.path().join("false-coordinate-derivation.db");
        crate::diagnostics::backup_connection_to(&conn, &path).unwrap();
        let report = crate::verify_lineage_backup(&path, lineage.as_str()).unwrap();
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.starts_with("archive coordinates:")
                && issue.contains("authoritative header")));
        let tx = conn.transaction().unwrap();
        assert!(publish_archive_coordinates(&tx, &lineage, &payload, kind, &bytes).is_err());
        let history = put_payload(
            &tx,
            &lineage,
            PayloadKind::History,
            b"opaque",
            ObjectCompression::none(),
            &mut OperationStats::default(),
        )
        .unwrap();
        assert!(tx
            .execute(
                "INSERT INTO lineage_archive_coordinates VALUES (?1, ?2, 'metadata', '{}', ?3)",
                (lineage.as_str(), history.id.as_str(), "0".repeat(64))
            )
            .is_err());
    }

    #[test]
    fn archive_coordinate_missing_proof_is_lazy_verified_transactional_and_parent_owned() {
        let (mut conn, lineage) = fixture();
        let kind = ArchiveHeaderKind::Metadata;
        let bytes = header(kind, 42, json!({"unknown": "x".repeat(524288)}));
        let payload = put_payload(
            &conn,
            &lineage,
            PayloadKind::Data,
            &bytes,
            ObjectCompression::none(),
            &mut OperationStats::default(),
        )
        .unwrap();
        conn.pragma_update(None, "query_only", true).unwrap();
        crate::schema::validate_lineage_schema(&conn).unwrap();
        let mut stats = OperationStats::default();
        assert!(matches!(
            archive_header_coordinates(&conn, &lineage, &payload, kind, &mut stats),
            Err(StoreError::Integrity(message)) if message.contains("requires a write transaction")
        ));
        assert_eq!(stats.payloads_read, 1);
        conn.pragma_update(None, "query_only", false).unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        assert_eq!(
            count(&conn),
            0,
            "reopening must not guess or hydrate coordinates"
        );
        assert!(publish_archive_coordinates(&conn, &lineage, &payload, kind, &bytes).is_err());
        {
            let tx = conn.transaction().unwrap();
            let mut stats = OperationStats::default();
            archive_header_coordinates(&tx, &lineage, &payload, kind, &mut stats).unwrap();
            assert_eq!(stats.payloads_read, 1);
            assert_eq!(count(&tx), 1);
        }
        assert_eq!(
            count(&conn),
            0,
            "rollback must release unpublished coordinates"
        );
        let stored = object(&conn, &payload.object_hash).unwrap().unwrap().bytes;
        conn.execute(
            "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
            [&payload.object_hash],
        )
        .unwrap();
        {
            let tx = conn.transaction().unwrap();
            assert!(archive_header_coordinates(
                &tx,
                &lineage,
                &payload,
                kind,
                &mut OperationStats::default()
            )
            .is_err());
        }
        assert_eq!(
            count(&conn),
            0,
            "corruption must never publish guessed coordinates"
        );
        conn.execute(
            "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
            (&stored, &payload.object_hash),
        )
        .unwrap();
        {
            let tx = conn.transaction().unwrap();
            archive_header_coordinates(
                &tx,
                &lineage,
                &payload,
                kind,
                &mut OperationStats::default(),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(count(&conn), 1);
        conn.execute(
            "DELETE FROM lineage_payload_object_refs WHERE payload_id = ?1",
            [payload.id.as_str()],
        )
        .unwrap();
        assert_eq!(
            count(&conn),
            0,
            "only the source payload owns its coordinate projection"
        );
        let replacement = install(&mut conn, &lineage, kind, &bytes, ObjectCompression::none());
        assert_eq!(replacement, payload);
        verify_archive_coordinates(&conn, &lineage).unwrap();
    }
}
