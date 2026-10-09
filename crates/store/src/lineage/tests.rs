use super::*;

mod compact;

const LINEAGE_CRASH_ROLE: &str = "SMELT_LINEAGE_CRASH_ROLE";
const LINEAGE_CRASH_DB: &str = "SMELT_LINEAGE_CRASH_DB";
const RECLAMATION_CRASH_DB: &str = "SMELT_RECLAMATION_CRASH_DB";

fn setup() -> (Connection, LineageId) {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::schema::initialize_lineage_schema(&mut conn).unwrap();
    let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
    create_lineage(&conn, &lineage, 1).unwrap();
    (conn, lineage)
}

fn legacy_setup() -> (Connection, LineageId) {
    let conn = crate::schema::tests::v3_connection();
    let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
    create_lineage(&conn, &lineage, 1).unwrap();
    (conn, lineage)
}

fn history_bytes(text: impl Into<String>) -> Vec<u8> {
    serde_json::to_vec(&protocol::HistoryItem::user(protocol::Content::text(text))).unwrap()
}

fn bytes(index: usize) -> Vec<u8> {
    history_bytes(format!("item-{index}-{}", "x".repeat(index % 17)))
}

fn branch_id(digit: char) -> BranchId {
    BranchId::new(digit.to_string().repeat(64)).unwrap()
}

fn branch_metadata() -> BranchMetadata {
    BranchMetadata {
        parent_session_id: None,
        cwd: Some("/workspace".into()),
        mode: Some("agent".into()),
        reasoning_effort: Some("medium".into()),
        model: Some("test-model".into()),
        fast_mode: Some(true),
        session_cost_usd: 1.25,
        input_tokens: 100,
        cached_input_tokens: 40,
        output_tokens: 30,
        reasoning_tokens: 20,
        accounting_json: "{}".into(),
    }
}

fn assert_integrity<T>(result: Result<T>) {
    assert!(matches!(result, Err(StoreError::Integrity(_))));
}

fn reachable_leaves(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
) -> Vec<SequenceNode> {
    let Some(root_node) = root.node_id.clone() else {
        return Vec::new();
    };
    let mut pending = vec![root_node];
    let mut seen = BTreeSet::new();
    let mut leaves = Vec::new();
    while let Some(node_id) = pending.pop() {
        if !seen.insert(node_id.as_str().to_owned()) {
            continue;
        }
        let node = load_node_shallow(conn, lineage, &node_id, None).unwrap();
        if node.level == 0 {
            assert!(node.entries.len() == 1 || node.byte_count <= LEAF_TARGET_BYTES);
            leaves.push(node);
            continue;
        }
        for entry in node.entries {
            let EntryTarget::Child(child_id) = entry.target else {
                panic!("validated internal node contains an item");
            };
            pending.push(child_id);
        }
    }
    leaves
}

#[test]
fn mixed_payload_append_preserves_bulk_and_frontier_content_addresses() {
    for kind in [
        SequenceKind::History,
        SequenceKind::Transcript,
        SequenceKind::Data,
    ] {
        let item_bytes = |index: usize, text: String| {
            if kind == SequenceKind::Transcript {
                serde_json::to_vec(&StoredTranscriptBlock {
                    block_idx: index as u64,
                    history_idx: Some(index as u64),
                    kind: "assistant".into(),
                    tool_call_id: None,
                    tool_name: None,
                    content_hash: format!("{index:064x}"),
                    estimated_text_bytes: text.len() as u64,
                    preview_text: text.clone(),
                    block_json: serde_json::json!({"Text": {"content": text}}).to_string(),
                    indexed_text: text,
                    origin_json: None,
                    tool_state_json: None,
                    tool_render_revision: 0,
                })
                .unwrap()
            } else {
                history_bytes(text)
            }
        };
        for count in [0, 1, 31, 32, 33, 1025] {
            let (mut conn, lineage) = setup();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            let items = (0..count)
                .map(|index| item_bytes(index, format!("item-{index}")))
                .collect::<Vec<_>>();
            let empty = empty_sequence(&conn, &lineage, kind).unwrap();
            let (original, _) = append_sequence(
                &mut conn,
                &lineage,
                &empty,
                &items,
                ObjectCompression::none(),
            )
            .unwrap();
            let (references, _) =
                sequence_payload_refs_from_root(&conn, &lineage, &original, 0, original.item_count)
                    .unwrap();
            let rows: i64 = conn
                .query_row("SELECT count(*) FROM objects", [], |row| row.get(0))
                .unwrap();
            let (bulk, stats) = append_sequence_payloads_in(
                &conn,
                &lineage,
                &empty,
                references.iter().cloned().map(Ok),
            )
            .unwrap();
            assert_eq!(bulk, original);
            assert_eq!(stats.payloads_read, 0);
            assert_eq!(stats.payloads_written, 0);
            assert_eq!(
                conn.query_row("SELECT count(*) FROM objects", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                rows
            );
            let mut incremental = empty.clone();
            let mut expected = empty;
            for (chunk, originals) in references.chunks(19).zip(items.chunks(19)) {
                let mut expected_items = Vec::new();
                let mut payload_stats = OperationStats::default();
                let payloads =
                    chunk
                        .iter()
                        .zip(originals)
                        .enumerate()
                        .map(|(index, (payload, bytes))| {
                            if index.is_multiple_of(7) {
                                let changed = item_bytes(index, format!("new header {index}"));
                                let stored = put_payload(
                                    &conn,
                                    &lineage,
                                    kind.into(),
                                    &changed,
                                    ObjectCompression::none(),
                                    &mut payload_stats,
                                );
                                expected_items.push(changed);
                                stored
                            } else {
                                expected_items.push(bytes.clone());
                                Ok(payload.clone())
                            }
                        });
                let (next, stats) =
                    append_sequence_payloads_in(&conn, &lineage, &incremental, payloads).unwrap();
                assert_eq!(stats.payloads_read, 0);
                assert_eq!(stats.payloads_written, 0);
                incremental = next;
                expected = append_sequence_in(
                    &conn,
                    &lineage,
                    &expected,
                    &expected_items,
                    ObjectCompression::none(),
                )
                .unwrap()
                .0;
                assert_eq!(incremental, expected);
            }
            validate_sequence(&conn, &lineage, &incremental).unwrap();
        }
    }
}

#[test]
fn mixed_payload_append_is_body_independent_and_keeps_cold_verification() {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };
    let mut work = Vec::new();
    for (body_bytes, compression) in [128, 256 * 1024, 1024 * 1024, LEAF_TARGET_BYTES as usize + 1]
        .into_iter()
        .flat_map(|body_bytes| {
            [ObjectCompression::none(), ObjectCompression::default()]
                .map(|compression| (body_bytes, compression))
        })
    {
        let (mut conn, lineage) = setup();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::Data).unwrap();
        let body = vec![0xff; body_bytes];
        let (original, _) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            std::slice::from_ref(&body),
            compression,
        )
        .unwrap();
        let (references, _) =
            sequence_payload_refs_from_root(&conn, &lineage, &original, 0, 1).unwrap();
        let retained = &references[0];
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT bytes FROM objects WHERE hash = ?1",
                [&retained.object_hash],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
            [&retained.object_hash],
        )
        .unwrap();
        let steps = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&steps);
        conn.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let header = b"synthetic changed header";
        let mut payload_stats = OperationStats::default();
        let changed = put_payload(
            &tx,
            &lineage,
            PayloadKind::Data,
            header,
            ObjectCompression::none(),
            &mut payload_stats,
        )
        .unwrap();
        let (shared, stats) = append_sequence_payloads_in(
            &tx,
            &lineage,
            &original,
            [Ok(changed), Ok(retained.clone())],
        )
        .unwrap();
        tx.commit().unwrap();
        let vm_steps = steps.load(Ordering::Relaxed);
        conn.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert_eq!(payload_stats.payloads_written, 1);
        assert_eq!(stats.payloads_written, 0);
        assert_eq!(stats.payloads_read, 0);
        assert!(stats.nodes_read < 20);
        assert!(stats.nodes_written < 20);
        assert_eq!(shared.item_count, 3);
        assert_eq!(shared.byte_count(), (body_bytes * 2 + header.len()) as u64);
        assert!(validate_sequence(&conn, &lineage, &shared).is_err());
        assert!(
            sequence_range(&conn, &lineage, &shared, 0, 3).is_err(),
            "cold reads must reject the corrupt retained body"
        );
        conn.execute(
            "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
            (&stored, &retained.object_hash),
        )
        .unwrap();
        assert_eq!(
            sequence_range(&conn, &lineage, &shared, 0, 3).unwrap().0,
            vec![body.clone(), header.to_vec(), body]
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM objects", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        eprintln!("MIXED_PAYLOAD_APPEND body_bytes={body_bytes} compression={compression:?} vm_steps={vm_steps} nodes_read={} nodes_written={} payloads_read={} payloads_written={}", stats.nodes_read, stats.nodes_written, stats.payloads_read, payload_stats.payloads_written);
        work.push(vm_steps);
    }
    for steps in work.iter().skip(1) {
        assert!(
            *steps <= work[0] * 4 + 5000,
            "retained body bytes increased metadata publication work: {work:?}"
        );
    }
}

#[test]
fn mixed_payload_append_rejects_forged_or_wrong_lineage_references_atomically() {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::Data).unwrap();
    let (prefix, _) = append_sequence(
        &mut conn,
        &lineage,
        &empty,
        &[b"retained body".to_vec()],
        ObjectCompression::none(),
    )
    .unwrap();
    let (references, _) = sequence_payload_refs_from_root(&conn, &lineage, &prefix, 0, 1).unwrap();
    let original = references[0].clone();
    let other_lineage = LineageId::from_hex("2".repeat(32)).unwrap();
    let mut other_conn = Connection::open_in_memory().unwrap();
    crate::schema::initialize_lineage_schema(&mut other_conn).unwrap();
    create_lineage(&other_conn, &other_lineage, 1).unwrap();
    let foreign = put_payload(
        &other_conn,
        &other_lineage,
        PayloadKind::Data,
        b"foreign retained body",
        ObjectCompression::none(),
        &mut OperationStats::default(),
    )
    .unwrap();
    let mut wrong_size = original.clone();
    wrong_size.byte_count += 1;
    let mut wrong_hash = original.clone();
    wrong_hash.object_hash = "0".repeat(64);
    let mut wrong_kind = original.clone();
    wrong_kind.kind = PayloadKind::History;
    let mut wrong_id = original.clone();
    wrong_id.id = PayloadId::from_db("0".repeat(64)).unwrap();
    for root in [&empty, &prefix] {
        for invalid in [&wrong_size, &wrong_hash, &wrong_kind, &wrong_id, &foreign] {
            let before = archive_publication_counts(&conn);
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let fresh = put_payload(
                &tx,
                &lineage,
                PayloadKind::Data,
                b"new header before invalid reference",
                ObjectCompression::none(),
                &mut OperationStats::default(),
            )
            .unwrap();
            assert!(append_sequence_payloads_in(
                &tx,
                &lineage,
                root,
                [Ok(fresh), Ok(invalid.clone())]
            )
            .is_err());
            tx.rollback().unwrap();
            assert_eq!(archive_publication_counts(&conn), before);
            assert_eq!(load_root(&conn, &lineage, &root.id).unwrap(), *root);
        }
    }
    let before = archive_publication_counts(&conn);
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let fresh = put_payload(
        &tx,
        &lineage,
        PayloadKind::Data,
        b"new header before producer failure",
        ObjectCompression::none(),
        &mut OperationStats::default(),
    )
    .unwrap();
    assert!(append_sequence_payloads_in(
        &tx,
        &lineage,
        &prefix,
        [
            Ok(fresh),
            Err(StoreError::Integrity("injected producer failure".into()))
        ]
    )
    .is_err());
    tx.rollback().unwrap();
    assert_eq!(archive_publication_counts(&conn), before);
}

#[test]
fn mixed_payload_append_publication_failures_roll_back_nodes_proofs_and_roots() {
    for table in [
        "lineage_sequence_nodes",
        "lineage_completed_sequence_nodes",
        "lineage_sequence_roots",
    ] {
        let (mut conn, lineage) = setup();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let empty = empty_sequence(&conn, &lineage, SequenceKind::Data).unwrap();
        let (prefix, _) = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            &[b"retained body".to_vec()],
            ObjectCompression::none(),
        )
        .unwrap();
        let (references, _) =
            sequence_payload_refs_from_root(&conn, &lineage, &prefix, 0, 1).unwrap();
        for root in [&empty, &prefix] {
            conn.execute_batch(&format!("CREATE TEMP TRIGGER fail_reference_publication AFTER INSERT ON {table} BEGIN SELECT RAISE(ABORT, 'injected reference publication failure'); END;")).unwrap();
            let before = archive_publication_counts(&conn);
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(append_sequence_payloads_in(
                &tx,
                &lineage,
                root,
                [Ok(references[0].clone()), Ok(references[0].clone())]
            )
            .is_err());
            tx.rollback().unwrap();
            assert_eq!(archive_publication_counts(&conn), before);
            assert_eq!(load_root(&conn, &lineage, &root.id).unwrap(), *root);
            conn.execute_batch("DROP TRIGGER fail_reference_publication;")
                .unwrap();
        }
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let (shared, stats) = append_sequence_payloads_in(
            &tx,
            &lineage,
            &prefix,
            [Ok(references[0].clone()), Ok(references[0].clone())],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(stats.payloads_read, 0);
        assert_eq!(stats.payloads_written, 0);
        assert_eq!(shared.item_count, 3);
        validate_sequence(&conn, &lineage, &shared).unwrap();
    }
}

fn session_metadata(updated_at: i64, title: &str) -> SessionMetadata {
    SessionMetadata {
        title: Some(title.into()),
        slug: None,
        first_user_message: None,
        cwd: Some("/workspace".into()),
        mode: Some("agent".into()),
        reasoning_effort: Some("medium".into()),
        model: Some("test-model".into()),
        fast_mode: Some(true),
        accounting_json: Some(serde_json::json!({
            "session_usage": {
                "input_tokens": 10,
                "cached_input_tokens": 3,
                "output_tokens": 4,
                "reasoning_tokens": 2
            }
        })),
        checkpoint_json: None,
        checkpoint_events_json: None,
        context_tokens: None,
        context_tokens_history_len: None,
        display_context_tokens: None,
        session_cost_usd: SessionCostUsd::new(1.5).unwrap(),
        updated_at,
    }
}

fn initial_session_commit(branch: &BranchId) -> SessionCommit {
    SessionCommit {
        session_id: branch.as_str().into(),
        expected: StoreHead::default(),
        identity: SessionIdentity {
            id: branch.as_str().into(),
            created_at: 1,
            parent_id: None,
        },
        metadata: session_metadata(1, "first"),
        history: crate::session_commit::HistorySuffix {
            start: HistoryIndex::ZERO,
            final_len: crate::session_commit::HistoryLen::new(1),
            items: vec![protocol::HistoryItem::system("one")],
        },
        side_tables: SideTableSuffixes::default(),
        transcript_records: None,
    }
}

fn archived_session_commit(branch: &BranchId, expected: StoreHead) -> SessionCommit {
    let mut command = initial_session_commit(branch);
    command.expected = expected;
    command.history.start = HistoryIndex::new(1);
    command.history.items.clear();
    command.metadata.updated_at = 2;
    command.metadata.first_user_message = Some("synthetic shared first message α\n".repeat(4096));
    let event = serde_json::json!({
        "kind": "auto", "summary": "archived α 日本語\n".repeat(4096),
        "first_live_index": 0, "completed_at_history_len": 1, "created_at_ms": 2,
        "unknown": {"preserved": true}
    });
    command.metadata.checkpoint_json = Some(event.clone());
    command.metadata.checkpoint_events_json = Some(serde_json::json!([event]));
    command.side_tables.metadata_snapshots.push((
        HistoryIndex::ZERO,
        serde_json::json!({"mode": "plan", "first_user_message": command.metadata.first_user_message, "unknown": [1,2,3]}),
    ));
    command
}

fn archive_publication_event(role: &str) -> &'static str {
    match role {
        "value" => "AFTER INSERT ON lineage_payload_object_refs WHEN NEW.payload_kind = 'data'",
        "coordinates" => "AFTER INSERT ON lineage_archive_coordinates",
        "root" => "AFTER INSERT ON lineage_sequence_roots WHEN NEW.root_kind = 'data'",
        "owner" => "AFTER INSERT ON lineage_revision_state_roots WHEN NEW.role = 'checkpoint'",
        "revision" => "AFTER INSERT ON lineage_revisions",
        "receipt" => "AFTER INSERT ON lineage_session_receipts",
        _ => panic!("unknown archive publication boundary"),
    }
}

fn commit_session_result(
    conn: &mut Connection,
    lineage: &LineageId,
    branch: &BranchId,
    command: &SessionCommit,
) -> std::result::Result<SessionCommitResult, SessionCommitFailure> {
    let mut tx = crate::write_transaction::begin_write(conn, "test result-owning save")
        .map_err(store_failure)?;
    let (fingerprint, receipt) = apply_lineage_session_commit_with_fingerprint(
        &mut tx,
        lineage,
        branch,
        command,
        ObjectCompression::none(),
    )?;
    let result = retain_session_receipt_result(
        &tx,
        lineage,
        branch,
        &fingerprint,
        receipt,
        ObjectCompression::none(),
    )
    .map_err(store_failure)?;
    tx.commit()
        .map_err(StoreError::from)
        .map_err(store_failure)?;
    Ok(result)
}

pub(super) fn reclaim_fixture(conn: &mut Connection, lineage: &LineageId) -> usize {
    let mut mutated = 0;
    for _ in 0..reclamation_step_limit(conn, lineage) {
        let step = reclaim_step(conn, lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        mutated += reclamation_mutations(step);
        if step.complete {
            return mutated;
        }
    }
    panic!("budget-one fixture reclamation did not complete");
}

fn reclamation_mutations(step: ReclamationStep) -> usize {
    step.branch_heads_cleared + step.canonical_rows_deleted + step.objects_deleted
}

pub(crate) fn reclamation_step_limit(conn: &Connection, lineage: &LineageId) -> usize {
    // Fixture settlement includes seed/frontier visits, stale-mark cleanup and
    // owner-release passes as well as canonical mutations. Each step stays budget one.
    reclamation_work_units(conn, lineage)
        .saturating_mul(8)
        .saturating_add(256)
}

#[test]
fn receipt_result_guards_bind_exact_revision_and_detect_tampered_proof() {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let branch = branch_id('a');
    let first = commit_session_result(
        &mut conn,
        &lineage,
        &branch,
        &initial_session_commit(&branch),
    )
    .unwrap();
    let command = archived_session_commit(&branch, first.receipt.current);
    let result = commit_session_result(&mut conn, &lineage, &branch, &command).unwrap();
    let fingerprint = crate::session_command::session_commit_fingerprint(&command).unwrap();
    let revision = RevisionId::from_db(result.revision_id.clone()).unwrap();
    assert_integrity(retain_session_receipt_result(
        &conn,
        &lineage,
        &branch,
        &fingerprint,
        result.receipt.clone(),
        ObjectCompression::none(),
    ));
    let mut changed = command.clone();
    changed.expected = result.receipt.current;
    changed.metadata.title = Some("same coordinates, different immutable revision".into());
    changed.metadata.updated_at = 3;
    let other = commit_session_result(&mut conn, &lineage, &branch, &changed).unwrap();
    assert_ne!(other.revision_id, result.revision_id);
    for sql in [
        "UPDATE lineage_session_receipt_results SET result_id = printf('%064d', 0)",
        "DELETE FROM lineage_session_receipt_results",
    ] {
        assert!(conn.execute_batch(sql).is_err());
    }
    for (owner_lineage, owner_branch, destination) in [
        (
            lineage.as_str(),
            branch.as_str(),
            other.revision_id.as_str(),
        ),
        (lineage.as_str(), branch_id('b').as_str(), revision.as_str()),
        (
            "22222222222222222222222222222222",
            branch.as_str(),
            revision.as_str(),
        ),
    ] {
        assert!(conn
            .execute(
                "INSERT INTO lineage_session_receipt_results
             (lineage_id, session_id, fingerprint, result_revision_id, result_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    owner_lineage,
                    owner_branch,
                    &fingerprint,
                    destination,
                    "0".repeat(64)
                ),
            )
            .is_err());
    }
    verify_session_receipt_results(&conn, &lineage).unwrap();
    let proof: String = conn
        .query_row(
            "SELECT result_id FROM lineage_session_receipt_results WHERE fingerprint = ?1",
            [&fingerprint],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute_batch("DROP TRIGGER lineage_session_receipt_result_update")
        .unwrap();
    conn.execute(
        "UPDATE lineage_session_receipt_results SET result_id = ?1 WHERE fingerprint = ?2",
        ("0".repeat(64), &fingerprint),
    )
    .unwrap();
    assert_integrity(load_session_receipt_result(
        &conn,
        &lineage,
        &branch,
        &fingerprint,
    ));
    assert_integrity(verify_session_receipt_results(&conn, &lineage));
    conn.execute(
        "UPDATE lineage_session_receipt_results SET result_id = ?1 WHERE fingerprint = ?2",
        (&proof, &fingerprint),
    )
    .unwrap();
    assert_eq!(
        load_session_receipt_result(&conn, &lineage, &branch, &fingerprint).unwrap(),
        Some(result)
    );
}

#[test]
fn receipt_result_multiple_owners_release_after_deleted_session_gc() {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let branch = branch_id('a');
    let first = commit_session_result(
        &mut conn,
        &lineage,
        &branch,
        &initial_session_commit(&branch),
    )
    .unwrap();
    let command = archived_session_commit(&branch, first.receipt.current);
    let result = commit_session_result(&mut conn, &lineage, &branch, &command).unwrap();
    let mut noop = command;
    noop.expected = result.receipt.current;
    let second = commit_session_result(&mut conn, &lineage, &branch, &noop).unwrap();
    assert_eq!(second.revision_id, result.revision_id);
    let id = RevisionId::from_db(result.revision_id.clone()).unwrap();
    rewind_branch(
        &mut conn,
        &lineage,
        &branch,
        &id,
        &RevisionId::from_db(first.revision_id).unwrap(),
        3,
    )
    .unwrap();
    let report = inspect_reachability(&conn, &lineage).unwrap();
    assert!(report.reachable_revisions.contains(id.as_str()));
    let original = load_revision(&conn, &lineage, &id).unwrap();
    let state = load_revision_state(&conn, &lineage, &original).unwrap();
    reclaim_fixture(&mut conn, &lineage);
    assert_eq!(
        load_revision_state(&conn, &lineage, &original).unwrap(),
        state
    );
    assert_eq!(
        commit_session_result(&mut conn, &lineage, &branch, &noop).unwrap(),
        second
    );
    delete_branch(&conn, &lineage, &branch, 4).unwrap();
    reclaim_fixture(&mut conn, &lineage);
    assert!(matches!(
        load_revision(&conn, &lineage, &id),
        Err(StoreError::MissingObject { .. })
    ));
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM lineage_session_receipt_results",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM lineage_session_receipts", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
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
fn receipt_result_deleted_turn_transition_releases_ownership_cycle() {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let branch = branch_id('a');
    let first = commit_session_result(
        &mut conn,
        &lineage,
        &branch,
        &initial_session_commit(&branch),
    )
    .unwrap();
    let mut session = initial_session_commit(&branch);
    session.expected = first.receipt.current;
    session.history.start = HistoryIndex::new(1);
    session.history.final_len = crate::HistoryLen::new(2);
    session.history.items = vec![protocol::HistoryItem::user(protocol::Content::text(
        "synthetic next turn",
    ))];
    session.side_tables.start = HistoryIndex::new(1);
    session.metadata.updated_at = 2;
    let submit = SubmitTurn {
        session,
        turn: crate::NewTurn {
            kind: TurnKind::User,
            submitted_history_idx: HistoryIndex::new(1),
            continuation_of: None,
            created_at_ms: 2,
        },
    };
    let submitted = apply_lineage_submit_turn(
        &mut conn,
        &lineage,
        &branch,
        &submit,
        ObjectCompression::none(),
    )
    .unwrap();
    let mut session = submit.session.clone();
    session.expected = submitted.session.current;
    session.history.start = HistoryIndex::new(2);
    session.history.items.clear();
    session.side_tables.start = HistoryIndex::new(2);
    session.metadata.updated_at = 3;
    let transition = TurnTransition {
        session,
        turn_id: submitted.turn_id,
        state: TurnState::Running,
        at_ms: 3,
        terminal_reason: None,
    };
    let transitioned = apply_lineage_turn_transition(
        &mut conn,
        &lineage,
        &branch,
        &transition,
        ObjectCompression::none(),
    )
    .unwrap();
    let fingerprint = crate::session_command::turn_transition_fingerprint(&transition).unwrap();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let owned = retain_session_receipt_result(
        &tx,
        &lineage,
        &branch,
        &fingerprint,
        transitioned.session,
        ObjectCompression::none(),
    )
    .unwrap();
    tx.commit().unwrap();
    let id = RevisionId::from_db(owned.revision_id).unwrap();
    let submitted_id = branch_revision_at_sequence(
        &conn,
        &lineage,
        &branch,
        submitted.session.current.revision.get(),
    )
    .unwrap();
    rewind_branch(
        &mut conn,
        &lineage,
        &branch,
        &id,
        &RevisionId::from_db(first.revision_id).unwrap(),
        4,
    )
    .unwrap();
    assert!(inspect_reachability(&conn, &lineage)
        .unwrap()
        .reachable_revisions
        .contains(submitted_id.as_str()));
    reclaim_fixture(&mut conn, &lineage);
    assert!(
        load_session_receipt_result(&conn, &lineage, &branch, &fingerprint)
            .unwrap()
            .is_some()
    );
    delete_branch(&conn, &lineage, &branch, 5).unwrap();
    reclaim_fixture(&mut conn, &lineage);
    for table in [
        "lineage_turn_transitions",
        "lineage_turns",
        "lineage_session_receipt_results",
    ] {
        assert_eq!(
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert!(matches!(
        load_revision(&conn, &lineage, &id),
        Err(StoreError::MissingObject { .. })
    ));
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
fn receipt_result_hot_replay_avoids_archives_but_cold_audit_verifies_them() {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let branch = branch_id('a');
    let first = commit_session_result(
        &mut conn,
        &lineage,
        &branch,
        &initial_session_commit(&branch),
    )
    .unwrap();
    let command = archived_session_commit(&branch, first.receipt.current);
    let result = commit_session_result(&mut conn, &lineage, &branch, &command).unwrap();
    let revision = load_revision(
        &conn,
        &lineage,
        &RevisionId::from_db(result.revision_id.clone()).unwrap(),
    )
    .unwrap();
    rewind_branch(
        &mut conn,
        &lineage,
        &branch,
        &revision.id,
        &RevisionId::from_db(first.revision_id).unwrap(),
        3,
    )
    .unwrap();
    let (hash, bytes): (String, Vec<u8>) = conn.query_row(
        "SELECT object.hash, object.bytes FROM lineage_revision_state_roots owner
         JOIN lineage_sequence_roots root ON root.lineage_id = owner.lineage_id AND root.root_id = owner.root_id
         JOIN lineage_sequence_entries entry ON entry.lineage_id = root.lineage_id AND entry.node_id = root.root_node_id
         JOIN lineage_payload_object_refs payload ON payload.lineage_id = entry.lineage_id AND payload.payload_id = entry.payload_id
         JOIN objects object ON object.hash = payload.object_hash
         WHERE owner.lineage_id = ?1 AND owner.state_payload_id = ?2 AND owner.role = 'first_user_message'",
        (lineage.as_str(), revision.state_payload_id.as_str()), |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    conn.execute(
        "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
        [&hash],
    )
    .unwrap();
    assert_eq!(
        commit_session_result(&mut conn, &lineage, &branch, &command).unwrap(),
        result
    );
    assert!(load_revision_state(&conn, &lineage, &revision).is_err());
    assert!(verify_session_receipt_results(&conn, &lineage).is_err());
    assert!(lineage_session_snapshot(&conn, &lineage, &branch).is_ok());
    let backup = tempfile::tempdir().unwrap();
    let path = backup.path().join("corrupt-receipt-result.db");
    crate::diagnostics::backup_connection_to(&conn, &path).unwrap();
    let report = crate::verify_lineage_backup(&path, lineage.as_str()).unwrap();
    assert!(!report.healthy);
    assert!(report
        .issues
        .iter()
        .any(|issue| issue.starts_with("session receipt result:")));
    conn.execute(
        "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
        (&bytes, &hash),
    )
    .unwrap();
    verify_session_receipt_results(&conn, &lineage).unwrap();
}

#[test]
fn receipt_result_legacy_noop_owns_original_projected_revision() {
    let (mut conn, lineage, branch, command, original) = legacy_projection_fixture(1);
    let before = load_revision_state(&conn, &lineage, &original).unwrap();
    let result = commit_session_result(&mut conn, &lineage, &branch, &command).unwrap();
    assert_eq!(result.revision_id, original.id.as_str());
    assert_eq!(result.receipt.current, command.expected);
    assert_eq!(
        load_revision_state(&conn, &lineage, &original).unwrap(),
        before
    );
    verify_revision_projections(&conn, &lineage).unwrap();
    verify_session_receipt_results(&conn, &lineage).unwrap();
    let rows = archive_publication_counts(&conn);
    assert_eq!(
        commit_session_result(&mut conn, &lineage, &branch, &command).unwrap(),
        result
    );
    assert_eq!(archive_publication_counts(&conn), rows);
}

#[test]
fn receipt_result_publication_failures_roll_back_canonical_and_projection_rows() {
    for role in ["value", "root", "owner", "projection", "receipt", "result"] {
        let (mut conn, lineage, branch, command, original) = legacy_projection_fixture(1);
        let before = archive_publication_counts(&conn);
        let event = if role == "result" {
            "AFTER INSERT ON lineage_session_receipt_results"
        } else {
            legacy_projection_publication_event(role)
        };
        conn.execute_batch(&format!("CREATE TEMP TRIGGER reject_result {event} BEGIN SELECT RAISE(ABORT, 'injected receipt result failure'); END;")).unwrap();
        assert!(commit_session_result(&mut conn, &lineage, &branch, &command).is_err());
        assert!(conn.is_autocommit());
        assert_eq!(
            archive_publication_counts(&conn),
            before,
            "partial rows at {role}"
        );
        assert_eq!(
            load_branch_record(&conn, &lineage, &branch, false)
                .unwrap()
                .revision,
            original
        );
        conn.execute_batch("DROP TRIGGER reject_result").unwrap();
        assert_eq!(
            commit_session_result(&mut conn, &lineage, &branch, &command)
                .unwrap()
                .revision_id,
            original.id.as_str()
        );
    }
}

#[test]
fn receipt_result_refuses_missing_legacy_outcomes_without_head_fallback() {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let branch = branch_id('a');
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial_session_commit(&branch),
        ObjectCompression::none(),
    )
    .unwrap();
    let mut command = archived_session_commit(&branch, first.current);
    let second = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    command.expected = second.current;
    let noop = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    let id = branch_revision_at_sequence(&conn, &lineage, &branch, second.current.revision.get())
        .unwrap();
    let target = branch_revision_at_sequence(&conn, &lineage, &branch, 1).unwrap();
    rewind_branch(&mut conn, &lineage, &branch, &id, &target, 3).unwrap();
    reclaim_fixture(&mut conn, &lineage);
    assert!(matches!(
        load_revision(&conn, &lineage, &id),
        Err(StoreError::MissingObject { .. })
    ));
    let rows = archive_publication_counts(&conn);
    let head = branch_head(&conn, &lineage, &branch).unwrap();
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none()
        )
        .unwrap(),
        noop
    );
    assert!(commit_session_result(&mut conn, &lineage, &branch, &command).is_err());
    assert_eq!(archive_publication_counts(&conn), rows);
    assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), head);
}

#[test]
fn receipt_result_legacy_bootstrap_equivalence_publishes_verified_projection() {
    let (mut conn, lineage) = legacy_setup();
    let branch = branch_id('a');
    let mut command = initial_session_commit(&branch);
    let receipt = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    let original = load_branch_record(&conn, &lineage, &branch, false)
        .unwrap()
        .revision;
    let before = load_revision_state(&conn, &lineage, &original).unwrap();
    crate::schema::initialize_lineage_schema(&mut conn).unwrap();
    command.transcript_records = Some(crate::TranscriptRecordSuffix {
        start: crate::TranscriptRecordIndex::ZERO,
        records: vec![],
    });
    let result = commit_session_result(&mut conn, &lineage, &branch, &command).unwrap();
    assert_eq!(result.receipt.current, receipt.current);
    assert_eq!(result.revision_id, original.id.as_str());
    assert_eq!(
        load_revision_state(&conn, &lineage, &original).unwrap(),
        before
    );
    verify_revision_projections(&conn, &lineage).unwrap();
    verify_session_receipt_results(&conn, &lineage).unwrap();
}

#[test]
fn receipt_result_publication_is_process_crash_atomic() {
    const ROLE: &str = "SMELT_RECEIPT_RESULT_CRASH_ROLE";
    const DB: &str = "SMELT_RECEIPT_RESULT_CRASH_DB";
    const MODE: &str = "SMELT_RECEIPT_RESULT_CRASH_MODE";
    fn event(role: &str) -> &'static str {
        if role == "result" {
            "AFTER INSERT ON lineage_session_receipt_results"
        } else {
            legacy_projection_publication_event(role)
        }
    }
    let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
    let branch = branch_id('a');
    if let (Ok(role), Ok(path), Ok(mode)) =
        (std::env::var(ROLE), std::env::var(DB), std::env::var(MODE))
    {
        let mut conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        if role == "commit" {
            conn.commit_hook(Some(|| -> bool { std::process::abort() }))
                .unwrap();
        } else {
            conn.create_scalar_function(
                "smelt_test_result_crash",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |_| -> rusqlite::Result<i64> { std::process::abort() },
            )
            .unwrap();
            conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER crash_result {} BEGIN SELECT smelt_test_result_crash(); END;",
                event(&role)
            ))
            .unwrap();
        }
        let snapshot = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
        let command = if mode == "modern" {
            archived_session_commit(&branch, snapshot.head)
        } else {
            SessionCommit {
                session_id: branch.as_str().into(),
                expected: snapshot.head,
                identity: snapshot.identity,
                metadata: snapshot.metadata,
                history: crate::HistorySuffix {
                    start: HistoryIndex::new(1),
                    final_len: crate::HistoryLen::new(1),
                    items: vec![],
                },
                side_tables: SideTableSuffixes {
                    start: HistoryIndex::new(1),
                    ..Default::default()
                },
                transcript_records: None,
            }
        };
        let result = commit_session_result(&mut conn, &lineage, &branch, &command);
        panic!("receipt result crash boundary not reached: {result:?}");
    }
    let dir = tempfile::tempdir().unwrap();
    for mode in ["legacy", "modern"] {
        for role in [
            "value",
            "root",
            "owner",
            if mode == "legacy" {
                "projection"
            } else {
                "revision"
            },
            "receipt",
            "result",
            "commit",
        ] {
            let (mut source, lineage, branch, command, original) = if mode == "legacy" {
                legacy_projection_fixture(1)
            } else {
                let (mut conn, lineage) = setup();
                conn.pragma_update(None, "foreign_keys", true).unwrap();
                let branch = branch_id('a');
                let first = apply_lineage_session_commit(
                    &mut conn,
                    &lineage,
                    &branch,
                    &initial_session_commit(&branch),
                    ObjectCompression::none(),
                )
                .unwrap();
                let original = load_branch_record(&conn, &lineage, &branch, false)
                    .unwrap()
                    .revision;
                let command = archived_session_commit(&branch, first.current);
                (conn, lineage, branch, command, original)
            };
            let before = archive_publication_counts(&source);
            let snapshot = load_revision_state(&source, &lineage, &original).unwrap();
            let path = dir.path().join(format!("result-{mode}-{role}.db"));
            crate::diagnostics::backup_connection_to(&source, &path).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("lineage::tests::receipt_result_publication_is_process_crash_atomic")
                .arg("--nocapture")
                .env(ROLE, role)
                .env(DB, &path)
                .env(MODE, mode)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(!status.success());
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(
                    status.signal(),
                    Some(libc::SIGABRT),
                    "wrong failure at {mode}/{role}"
                );
            }
            let mut restarted = Connection::open(&path).unwrap();
            restarted.pragma_update(None, "foreign_keys", true).unwrap();
            assert_eq!(archive_publication_counts(&restarted), before);
            assert_eq!(
                load_branch_record(&restarted, &lineage, &branch, false)
                    .unwrap()
                    .revision,
                original
            );
            assert_eq!(
                load_revision_state(&restarted, &lineage, &original).unwrap(),
                snapshot
            );
            crate::schema::validate_lineage_schema(&restarted).unwrap();
            let expected = commit_session_result(&mut source, &lineage, &branch, &command).unwrap();
            assert_eq!(
                commit_session_result(&mut restarted, &lineage, &branch, &command).unwrap(),
                expected
            );
            verify_session_receipt_results(&restarted, &lineage).unwrap();
            assert_eq!(
                commit_session_result(&mut restarted, &lineage, &branch, &command).unwrap(),
                expected
            );
        }
    }
}

fn archive_publication_counts(conn: &Connection) -> Vec<i64> {
    [
        "objects",
        "lineage_payload_object_refs",
        "lineage_archive_coordinates",
        "lineage_checkpoint_summary_presence",
        "lineage_revision_state_roots",
        "lineage_revision_state_projections",
        "lineage_sequence_nodes",
        "lineage_sequence_entries",
        "lineage_sequence_roots",
        "lineage_completed_sequence_nodes",
        "lineage_revisions",
        "lineage_session_receipts",
        "lineage_session_receipt_results",
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
fn archive_coordinates_follow_shared_header_owners_through_budget_one_reclamation() {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let a = branch_id('a');
    let b = branch_id('b');
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &a,
        &initial_session_commit(&a),
        ObjectCompression::none(),
    )
    .unwrap();
    apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &a,
        &archived_session_commit(&a, first.current),
        ObjectCompression::none(),
    )
    .unwrap();
    let source = lineage_session_snapshot(&conn, &lineage, &a).unwrap();
    let coordinates = conn
        .query_row(
            "SELECT count(*) FROM lineage_archive_coordinates",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert!(coordinates > 0);
    let first_b = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &b,
        &initial_session_commit(&b),
        ObjectCompression::none(),
    )
    .unwrap();
    apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &b,
        &archived_session_commit(&b, first_b.current),
        ObjectCompression::none(),
    )
    .unwrap();
    let second = lineage_session_snapshot(&conn, &lineage, &b).unwrap();
    assert_eq!(second.side_tables, source.side_tables);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM lineage_archive_coordinates",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        coordinates,
        "identical headers across branches share their coordinate owner"
    );
    let retained_payloads = inspect_reachability(&conn, &lineage)
        .unwrap()
        .reachable_payloads;
    delete_branch(&conn, &lineage, &a, 4).unwrap();
    let mut complete = false;
    for _ in 0..1000 {
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        assert_eq!(
            inspect_reachability(&conn, &lineage)
                .unwrap()
                .reachable_payloads,
            retained_payloads
        );
        assert_eq!(
            lineage_session_snapshot(&conn, &lineage, &b).unwrap(),
            second
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM lineage_archive_coordinates",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            coordinates
        );
        verify_archive_coordinates(&conn, &lineage).unwrap();
        if step.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    delete_branch(&conn, &lineage, &b, 5).unwrap();
    complete = false;
    for _ in 0..1000 {
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        inspect_reachability(&conn, &lineage).unwrap();
        verify_archive_coordinates(&conn, &lineage).unwrap();
        if step.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM lineage_archive_coordinates",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "coordinates do not retain an otherwise unreachable archive header"
    );
}

#[test]
fn session_save_receipt_failure_rolls_back_initial_noop_and_changed_publication() {
    for (existing, changed) in [(false, false), (true, false), (true, true)] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let mut command = initial_session_commit(&branch);
        if existing {
            let first = apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none(),
            )
            .unwrap();
            command.expected = first.current;
            command.history.start = HistoryIndex::new(1);
            command.history.items.clear();
            command.side_tables.start = HistoryIndex::new(1);
        }
        if changed {
            command.metadata.title = Some("changed".into());
            command.metadata.updated_at = 2;
        }
        let before = archive_publication_counts(&conn);
        let snapshot = load_branch_snapshot(&conn, &lineage, &branch, false)
            .optional_store()
            .unwrap();
        conn.execute_batch(&format!(
            "CREATE TEMP TRIGGER reject_save_receipt {} BEGIN SELECT RAISE(ABORT, 'injected save receipt failure'); END;",
            archive_publication_event("receipt"),
        ))
        .unwrap();
        assert!(apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .is_err());
        assert_eq!(archive_publication_counts(&conn), before);
        assert_eq!(
            load_branch_snapshot(&conn, &lineage, &branch, false)
                .optional_store()
                .unwrap(),
            snapshot,
        );
        conn.execute_batch("DROP TRIGGER reject_save_receipt")
            .unwrap();
        let receipt = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(receipt.previous, command.expected);
        let expected_revision = if existing && !changed {
            command.expected.revision
        } else {
            command.expected.revision.checked_add(1).unwrap()
        };
        assert_eq!(receipt.current.revision, expected_revision);
        assert_eq!(receipt.current.history_len, command.history.final_len);
        assert_eq!(receipt.current.transcript_record_count.get(), 0);
        let result = load_branch_snapshot(&conn, &lineage, &branch, false).unwrap();
        assert_eq!(result.metadata, command.metadata);
        if existing && !changed {
            assert_eq!(Some(&result), snapshot.as_ref());
        }
        let after = archive_publication_counts(&conn);
        assert_eq!(
            apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none(),
            )
            .unwrap(),
            receipt,
        );
        assert_eq!(archive_publication_counts(&conn), after);
    }
}

#[test]
fn compact_revision_archive_publication_failures_roll_back_every_owner_and_value() {
    for role in [
        "value",
        "coordinates",
        "root",
        "owner",
        "revision",
        "receipt",
    ] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let initial = initial_session_commit(&branch);
        let first = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &initial,
            ObjectCompression::none(),
        )
        .unwrap();
        let before = archive_publication_counts(&conn);
        let snapshot = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
        conn.execute_batch(&format!(
            "CREATE TEMP TRIGGER reject_archive_publication {} BEGIN SELECT RAISE(ABORT, 'injected archive publication failure'); END;",
            archive_publication_event(role))).unwrap();
        let command = archived_session_commit(&branch, first.current);
        assert!(apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none()
        )
        .is_err());
        assert_eq!(
            archive_publication_counts(&conn),
            before,
            "partial rows survived {role}"
        );
        assert_eq!(
            lineage_session_snapshot(&conn, &lineage, &branch).unwrap(),
            snapshot
        );
        conn.execute_batch("DROP TRIGGER reject_archive_publication")
            .unwrap();
        let receipt = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(receipt.current.revision.get(), 2);
        assert_eq!(
            lineage_session_snapshot(&conn, &lineage, &branch)
                .unwrap()
                .metadata,
            command.metadata
        );
    }
}

#[test]
fn compact_revision_archive_publication_is_process_crash_atomic() {
    const ROLE: &str = "SMELT_ARCHIVE_CRASH_ROLE";
    const DB: &str = "SMELT_ARCHIVE_CRASH_DB";
    let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
    let branch = branch_id('a');
    if let (Ok(role), Ok(path)) = (std::env::var(ROLE), std::env::var(DB)) {
        let mut conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        conn.create_scalar_function(
            "smelt_test_archive_crash",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_| -> rusqlite::Result<i64> { std::process::abort() },
        )
        .unwrap();
        conn.execute_batch(&format!("CREATE TEMP TRIGGER crash_archive_publication {} BEGIN SELECT smelt_test_archive_crash(); END;",
            archive_publication_event(&role))).unwrap();
        let head = lineage_session_snapshot(&conn, &lineage, &branch)
            .unwrap()
            .head;
        let command = archived_session_commit(&branch, head);
        let result = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        );
        panic!("archive crash boundary was not reached: {result:?}");
    }
    let dir = tempfile::tempdir().unwrap();
    for role in [
        "value",
        "coordinates",
        "root",
        "owner",
        "revision",
        "receipt",
    ] {
        let path = dir.path().join(format!("archive-{role}.db"));
        let initial = initial_session_commit(&branch);
        let (first, before, snapshot) = {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
            )
            .unwrap();
            crate::schema::initialize_lineage_schema(&mut conn).unwrap();
            create_lineage(&conn, &lineage, 1).unwrap();
            let first = apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &initial,
                ObjectCompression::none(),
            )
            .unwrap();
            (
                first,
                archive_publication_counts(&conn),
                lineage_session_snapshot(&conn, &lineage, &branch).unwrap(),
            )
        };
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("lineage::tests::compact_revision_archive_publication_is_process_crash_atomic")
            .arg("--nocapture")
            .env(ROLE, role)
            .env(DB, &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
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
        assert_eq!(
            archive_publication_counts(&conn),
            before,
            "partial publication survived {role}"
        );
        assert_eq!(
            lineage_session_snapshot(&conn, &lineage, &branch).unwrap(),
            snapshot
        );
        assert_eq!(
            apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &initial,
                ObjectCompression::none()
            )
            .unwrap(),
            first
        );
        let command = archived_session_commit(&branch, first.current);
        let receipt = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(receipt.current.revision.get(), 2);
        assert_eq!(
            lineage_session_snapshot(&conn, &lineage, &branch)
                .unwrap()
                .metadata,
            command.metadata
        );
    }
}

#[test]
fn compact_revision_archives_survive_forks_historical_reads_and_reclamation() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let initial = initial_session_commit(&branch);
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial,
        ObjectCompression::none(),
    )
    .unwrap();
    let command = archived_session_commit(&branch, first.current);
    let second = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    let archived = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
    let fork = branch_id('b');
    fork_branch(
        &mut conn,
        &lineage,
        &branch,
        &fork,
        Some(&archived.revision_id),
        3,
    )
    .unwrap();
    let mut cleared = command.clone();
    cleared.expected = second.current;
    cleared.metadata.updated_at = 4;
    cleared.metadata.checkpoint_json = None;
    cleared.metadata.checkpoint_events_json = Some(serde_json::json!([]));
    cleared.side_tables = SideTableSuffixes::default();
    apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &cleared,
        ObjectCompression::none(),
    )
    .unwrap();
    let before = inspect_reachability(&conn, &lineage).unwrap();
    assert!(
        before.reachable_roots.len() >= 5,
        "oracle must include revision archive roots"
    );
    let mut completed = false;
    for _ in 0..1000 {
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        if step.complete {
            completed = true;
            break;
        }
    }
    assert!(completed);
    let revision = load_revision(&conn, &lineage, &archived.revision_id).unwrap();
    assert_eq!(
        load_revision_state(&conn, &lineage, &revision)
            .unwrap()
            .metadata
            .checkpoint_events_json,
        command.metadata.checkpoint_events_json
    );
    let forked = lineage_session_snapshot(&conn, &lineage, &fork).unwrap();
    assert_eq!(
        forked.metadata.checkpoint_json,
        archived.metadata.checkpoint_json
    );
    assert_eq!(forked.side_tables, archived.side_tables);
    assert_eq!(
        inspect_reachability(&conn, &lineage)
            .unwrap()
            .reachable_objects,
        before.reachable_objects
    );
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none()
        )
        .unwrap(),
        second
    );
}

#[test]
fn compact_envelope_reads_hydrate_one_payload_independently_of_archive_size() {
    for count in [0, 32, 128] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let mut command = initial_session_commit(&branch);
        command.metadata.checkpoint_events_json = Some(serde_json::Value::Array((0..count).map(|index| {
            serde_json::json!({"kind": "auto", "summary": format!("{index}:{}", "s".repeat(32 * 1024)),
                "first_live_index": 0, "completed_at_history_len": 0, "created_at_ms": index})
        }).collect()));
        command.side_tables.metadata_snapshots.push((
            HistoryIndex::ZERO,
            serde_json::json!({"retained": "metadata α".repeat(4096)}),
        ));
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        let record = load_branch_record(&conn, &lineage, &branch, false).unwrap();
        let mut stats = OperationStats::default();
        assert!(matches!(
            load_revision_envelope(&conn, &lineage, &record.revision, &mut stats).unwrap(),
            StoredRevisionState::Shared(_)
        ));
        assert_eq!(stats.payloads_read, 1);
        assert_eq!(stats.nodes_read, 0);
        assert_eq!(stats.payloads_written, 0);
        let payload = load_payload_ref(&conn, &lineage, &record.revision.state_payload_id).unwrap();
        assert!(payload.byte_count < 2048);
    }
}

#[test]
fn canonical_title_submit_and_recovery_do_not_hydrate_retained_side_values() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let mut initial = initial_session_commit(&branch);
    initial.side_tables.metadata_snapshots.push((
        HistoryIndex::ZERO,
        serde_json::json!({"first_user_message": "retained metadata α".repeat(4096), "unknown": [null, true]}),
    ));
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial,
        ObjectCompression::none(),
    )
    .unwrap();
    let record = load_branch_record(&conn, &lineage, &branch, false).unwrap();
    let id = conn.query_row(
        "SELECT root_id FROM lineage_revision_state_roots WHERE lineage_id = ?1 AND state_payload_id = ?2 AND role = 'metadata_snapshots'",
        (lineage.as_str(), record.revision.state_payload_id.as_str()), |row| row.get::<_, String>(0)).unwrap();
    let root = load_root(&conn, &lineage, &RootId::from_db(id).unwrap()).unwrap();
    let (values, _) = sequence_payload_refs_from_root(&conn, &lineage, &root, 1, 2).unwrap();
    conn.execute(
        "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
        [&values[0].object_hash],
    )
    .unwrap();
    assert!(
        lineage_session_snapshot(&conn, &lineage, &branch).is_err(),
        "cold snapshots must still validate retained archive bytes"
    );
    let mut title = initial.clone();
    title.expected = first.current;
    title.metadata.title = Some("new title".into());
    title.metadata.updated_at = 2;
    title.history.start = HistoryIndex::new(1);
    title.history.items.clear();
    title.side_tables = SideTableSuffixes {
        start: HistoryIndex::new(1),
        ..SideTableSuffixes::default()
    };
    let second = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &title,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(second.current.revision.get(), 2);
    let mut session = title.clone();
    session.expected = second.current;
    session.metadata.updated_at = 3;
    session.history.items = vec![protocol::HistoryItem::user(protocol::Content::text(
        "next turn",
    ))];
    session.history.final_len = crate::session_commit::HistoryLen::new(2);
    let submit = SubmitTurn {
        session,
        turn: crate::session_commit::NewTurn {
            kind: TurnKind::User,
            submitted_history_idx: HistoryIndex::new(1),
            continuation_of: None,
            created_at_ms: 3,
        },
    };
    let receipt = apply_lineage_submit_turn(
        &mut conn,
        &lineage,
        &branch,
        &submit,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(receipt.session.current.history_len.get(), 2);
    let recovery = recover_lineage_nonterminal_turns(&mut conn, &lineage, &branch, 4)
        .unwrap()
        .unwrap();
    assert_eq!(recovery.session.receipt.previous, receipt.session.current);
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &title,
            ObjectCompression::none()
        )
        .unwrap(),
        second
    );
    assert!(lineage_session_snapshot(&conn, &lineage, &branch).is_err());
}

fn startup_pending_fixture(archive_count: usize) -> (Connection, LineageId, BranchId) {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let branch = branch_id('a');
    let mut session = initial_session_commit(&branch);
    session.metadata.checkpoint_events_json = Some(serde_json::Value::Array(
        (0..archive_count)
            .map(|index| {
                serde_json::json!({
                    "kind": "auto", "summary": format!("{index}:{}", "s".repeat(32 * 1024)),
                    "first_live_index": 0, "completed_at_history_len": 0, "created_at_ms": index,
                })
            })
            .collect(),
    ));
    for at_ms in [1, 2] {
        session.metadata.updated_at = at_ms;
        let receipt = apply_lineage_submit_turn(
            &mut conn,
            &lineage,
            &branch,
            &SubmitTurn {
                session: session.clone(),
                turn: crate::NewTurn {
                    kind: TurnKind::Command,
                    submitted_history_idx: HistoryIndex::ZERO,
                    continuation_of: None,
                    created_at_ms: at_ms as u64,
                },
            },
            ObjectCompression::none(),
        )
        .unwrap();
        session.expected = receipt.session.current;
        session.history.start = HistoryIndex::new(1);
        session.history.items.clear();
        session.side_tables = SideTableSuffixes {
            start: HistoryIndex::new(1),
            ..SideTableSuffixes::default()
        };
    }
    (conn, lineage, branch)
}

#[test]
fn startup_exact_result_publication_is_atomic_for_every_interrupted_turn() {
    let (mut conn, lineage, branch) = startup_pending_fixture(32);
    let before = load_branch_record(&conn, &lineage, &branch, false).unwrap();
    let rows = archive_publication_counts(&conn);
    for table in [
        "lineage_session_receipt_results",
        "lineage_turn_transitions",
    ] {
        conn.execute_batch(&format!(
            "CREATE TEMP TRIGGER fail_startup_publication AFTER INSERT ON {table}
             WHEN NEW.fingerprint = (SELECT fingerprint FROM lineage_session_receipts
                 WHERE command_kind = 'startup_recovery' AND turn_id = 2)
             BEGIN SELECT RAISE(ABORT, 'injected startup result failure'); END;"
        ))
        .unwrap();
        assert!(recover_lineage_nonterminal_turns(&mut conn, &lineage, &branch, 4).is_err());
        assert_eq!(archive_publication_counts(&conn), rows);
        let after = load_branch_record(&conn, &lineage, &branch, false).unwrap();
        assert_eq!(after.head, before.head);
        assert_eq!(after.revision.id, before.revision.id);
        for (table, expected) in [
            ("lineage_turns", 2),
            ("lineage_branch_revisions", 2),
            ("lineage_turn_transitions", 0),
        ] {
            assert_eq!(
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                expected
            );
        }
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM lineage_turns WHERE turn_state = 'ready' AND finished_at_ms IS NULL AND terminal_reason IS NULL", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
        conn.execute_batch("DROP TRIGGER fail_startup_publication")
            .unwrap();
    }
    let result = recover_lineage_nonterminal_turns(&mut conn, &lineage, &branch, 4)
        .unwrap()
        .unwrap();
    assert_eq!(
        result.interrupted_turns,
        vec![TurnId::new(1), TurnId::new(2)]
    );
    assert_eq!(result.session.revision_id, before.revision.id.as_str());
    assert_eq!(result.session.receipt.previous, before.head);
    assert_eq!(
        result.head().revision,
        before.head.revision.checked_add(1).unwrap()
    );
    let fingerprints = conn.prepare("SELECT fingerprint FROM lineage_session_receipts WHERE command_kind = 'startup_recovery' ORDER BY turn_id").unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<std::result::Result<Vec<_>, _>>().unwrap();
    assert_eq!(
        fingerprints,
        [
            "d05028dd43c1c2c7b2f668219dd477f26971f745fba9117898bc982353df5db2",
            "6aec5e237339a0d243d5200af2ab6186e9dbc8ef464b364f73cb7c3dbbf32d41",
        ]
    );
    for fingerprint in &fingerprints {
        assert_eq!(
            load_session_receipt_result(&conn, &lineage, &branch, fingerprint).unwrap(),
            Some(result.session.clone())
        );
    }
    let ordinary = crate::StartupRecoveryReceipt {
        session: result.session.receipt.clone(),
        interrupted_turns: result.interrupted_turns.clone(),
    };
    assert_eq!(
        serde_json::to_value(&ordinary).unwrap(),
        serde_json::json!({
            "session": result.session.receipt, "interrupted_turns": [1, 2]
        })
    );
    assert!(
        recover_lineage_nonterminal_turns(&mut conn, &lineage, &branch, 5)
            .unwrap()
            .is_none()
    );

    let initial = RevisionId::from_db(
        conn.query_row(
            "SELECT revision_id FROM lineage_branch_revisions WHERE branch_sequence = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap(),
    )
    .unwrap();
    rewind_branch(
        &mut conn,
        &lineage,
        &branch,
        &before.revision.id,
        &initial,
        6,
    )
    .unwrap();
    reclaim_fixture(&mut conn, &lineage);
    for fingerprint in &fingerprints {
        assert_eq!(
            load_session_receipt_result(&conn, &lineage, &branch, fingerprint).unwrap(),
            Some(result.session.clone())
        );
    }
    verify_session_receipt_results(&conn, &lineage).unwrap();
    delete_branch(&conn, &lineage, &branch, 7).unwrap();
    reclaim_fixture(&mut conn, &lineage);
    for fingerprint in &fingerprints {
        assert!(
            load_session_receipt_result(&conn, &lineage, &branch, fingerprint)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn startup_exact_result_work_is_independent_of_retained_archive_size() {
    let mut counts = Vec::new();
    for count in [0, 32, 128] {
        let (mut conn, lineage, branch) = startup_pending_fixture(count);
        let steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = steps.clone();
        conn.progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let result = recover_lineage_nonterminal_turns(&mut conn, &lineage, &branch, 4)
            .unwrap()
            .unwrap();
        conn.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert_eq!(result.interrupted_turns.len(), 2);
        counts.push(steps.load(std::sync::atomic::Ordering::Relaxed));
    }
    eprintln!(
        "STARTUP_EXACT_RESULT archive_events=0/32/128 vm_steps={}/{}/{}",
        counts[0], counts[1], counts[2]
    );
    for count in &counts[1..] {
        assert!(
            *count <= counts[0] + 512,
            "retained archives inflate startup work: {counts:?}"
        );
    }
}

#[test]
fn legacy_noop_commits_preserve_revision_identity_before_compact_publication() {
    let (mut conn, lineage) = legacy_setup();
    crate::schema::validate_lineage_schema(&conn).unwrap();
    let branch = branch_id('a');
    let mut initial = initial_session_commit(&branch);
    initial.metadata.checkpoint_events_json = Some(serde_json::json!([{
        "kind": "auto", "summary": "legacy summary".repeat(8192),
        "first_live_index": 0, "completed_at_history_len": 0, "created_at_ms": 1
    }]));
    initial
        .side_tables
        .metadata_snapshots
        .push((HistoryIndex::ZERO, serde_json::json!({"retained": true})));
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial,
        ObjectCompression::none(),
    )
    .unwrap();
    let record = load_branch_record(&conn, &lineage, &branch, false).unwrap();
    let original = hydrate_payload(
        &conn,
        &lineage,
        &record.revision.state_payload_id,
        PayloadKind::RevisionState,
        &mut OperationStats::default(),
    )
    .unwrap();
    crate::schema::initialize_lineage_schema(&mut conn).unwrap();
    let mut noop = initial.clone();
    noop.expected = first.current;
    noop.history.start = HistoryIndex::new(1);
    noop.history.items.clear();
    noop.side_tables = SideTableSuffixes {
        start: HistoryIndex::new(1),
        ..SideTableSuffixes::default()
    };
    let receipt = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &noop,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(receipt.current, first.current);
    assert_eq!(
        load_branch_record(&conn, &lineage, &branch, false)
            .unwrap()
            .revision
            .id,
        record.revision.id
    );
    assert_eq!(
        hydrate_payload(
            &conn,
            &lineage,
            &record.revision.state_payload_id,
            PayloadKind::RevisionState,
            &mut OperationStats::default()
        )
        .unwrap(),
        original
    );
    let mut changed = noop.clone();
    changed.metadata.updated_at = 2;
    changed.metadata.title = Some("compact title".into());
    let second = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &changed,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(second.current.revision.get(), 2);
    let current = load_branch_record(&conn, &lineage, &branch, false).unwrap();
    assert!(matches!(
        load_revision_envelope(
            &conn,
            &lineage,
            &current.revision,
            &mut OperationStats::default()
        )
        .unwrap(),
        StoredRevisionState::Shared(_)
    ));
    assert_eq!(
        lineage_session_snapshot(&conn, &lineage, &branch)
            .unwrap()
            .metadata,
        changed.metadata
    );
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &noop,
            ObjectCompression::none()
        )
        .unwrap(),
        receipt
    );
}

#[test]
fn message_only_changes_are_not_noops_in_inline_or_shared_formats() {
    for version in [3, crate::schema::LINEAGE_SCHEMA_VERSION] {
        let (mut conn, lineage) = if version == 3 {
            legacy_setup()
        } else {
            setup()
        };
        let branch = branch_id('a');
        let mut command = initial_session_commit(&branch);
        let first = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        command.expected = first.current;
        command.history.start = HistoryIndex::new(1);
        command.history.items.clear();
        command.side_tables.start = HistoryIndex::new(1);
        for message in [Some("synthetic first α".into()), None, Some(String::new())] {
            command.metadata.first_user_message = message;
            let changed = apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none(),
            )
            .unwrap();
            assert!(
                changed.current.revision > command.expected.revision,
                "first-message-only changes must persist in schema {version}"
            );
            assert_eq!(
                lineage_session_snapshot(&conn, &lineage, &branch)
                    .unwrap()
                    .metadata,
                command.metadata
            );
            command.expected = changed.current;
            let noop = apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none(),
            )
            .unwrap();
            assert_eq!(noop.current, command.expected);
        }
    }
}

fn legacy_projection_archive_commit(branch: &BranchId, expected: StoreHead) -> SessionCommit {
    let mut command = archived_session_commit(branch, expected);
    command.side_tables.turn_metas.push((
        HistoryIndex::ZERO,
        serde_json::json!({"turn": 1, "kind": "synthetic"}),
    ));
    command.side_tables.context_snapshots.push((
        HistoryIndex::ZERO,
        serde_json::json!({"tokens": 32, "unknown": ["preserved"]}),
    ));
    command
}

fn legacy_projection_source(
    checkpoints: usize,
) -> (
    Connection,
    LineageId,
    BranchId,
    SessionCommit,
    RevisionRecord,
) {
    let (mut conn, lineage) = legacy_setup();
    crate::schema::validate_lineage_schema(&conn).unwrap();
    let branch = branch_id('a');
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial_session_commit(&branch),
        ObjectCompression::none(),
    )
    .unwrap();
    let mut command = legacy_projection_archive_commit(&branch, first.current);
    let checkpoint = command.metadata.checkpoint_json.clone().unwrap();
    command.metadata.checkpoint_events_json = Some(serde_json::Value::Array(
        (0..checkpoints)
            .map(|index| {
                let mut checkpoint = checkpoint.clone();
                checkpoint["created_at_ms"] = serde_json::json!(index + 2);
                checkpoint
            })
            .collect(),
    ));
    let receipt = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    let original = load_branch_record(&conn, &lineage, &branch, false)
        .unwrap()
        .revision;
    command.expected = receipt.current;
    command.side_tables = SideTableSuffixes {
        start: HistoryIndex::new(1),
        ..SideTableSuffixes::default()
    };
    (conn, lineage, branch, command, original)
}

fn legacy_projection_fixture(
    checkpoints: usize,
) -> (
    Connection,
    LineageId,
    BranchId,
    SessionCommit,
    RevisionRecord,
) {
    let (mut conn, lineage, branch, command, original) = legacy_projection_source(checkpoints);
    crate::schema::initialize_lineage_schema(&mut conn).unwrap();
    (conn, lineage, branch, command, original)
}

#[test]
fn legacy_projection_reads_are_bounded_and_original_snapshots_remain_authoritative() {
    for checkpoints in [1, 32, 128] {
        let (mut conn, lineage, branch, command, original) = legacy_projection_fixture(checkpoints);
        let before = load_revision_state(&conn, &lineage, &original).unwrap();
        let source = load_payload_ref(&conn, &lineage, &original.state_payload_id).unwrap();
        let bytes = hydrate_payload_ref(
            &conn,
            &source,
            PayloadKind::RevisionState,
            &mut OperationStats::default(),
        )
        .unwrap();
        let noop = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(noop.current, command.expected);
        assert_eq!(
            load_branch_record(&conn, &lineage, &branch, false)
                .unwrap()
                .revision,
            original
        );
        assert_eq!(
            load_revision_state(&conn, &lineage, &original).unwrap(),
            before
        );
        assert_eq!(
            hydrate_payload_ref(
                &conn,
                &source,
                PayloadKind::RevisionState,
                &mut OperationStats::default()
            )
            .unwrap(),
            bytes
        );
        let projected = conn.query_row(
                "SELECT projected_payload_id FROM lineage_revision_state_projections WHERE original_payload_id = ?1",
                [original.state_payload_id.as_str()], |row| row.get::<_, String>(0),
            ).unwrap();
        let projected = PayloadId::from_db(projected).unwrap();
        assert!(
            load_payload_ref(&conn, &lineage, &projected)
                .unwrap()
                .byte_count
                < 2048
        );
        let rows = archive_publication_counts(&conn);
        assert_eq!(
            apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none()
            )
            .unwrap(),
            noop
        );
        assert_eq!(
            archive_publication_counts(&conn),
            rows,
            "receipt replay cannot republish a projection"
        );
        let stored_bytes = conn
            .query_row(
                "SELECT bytes FROM objects WHERE hash = ?1",
                [&source.object_hash],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
            [&source.object_hash],
        )
        .unwrap();
        let mut stats = OperationStats::default();
        assert!(matches!(
            load_revision_envelope(&conn, &lineage, &original, &mut stats).unwrap(),
            StoredRevisionState::Shared(_)
        ));
        assert_eq!(stats.payloads_read, 1);
        assert_eq!(stats.nodes_read, 0);
        assert_eq!(stats.payloads_written, 0);
        assert!(
            load_revision_state(&conn, &lineage, &original).is_err(),
            "full reads must verify original bytes"
        );
        assert!(
            verify_revision_projections(&conn, &lineage).is_err(),
            "cold audit must verify the derivation"
        );
        conn.execute(
            "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
            (&stored_bytes, &source.object_hash),
        )
        .unwrap();
        verify_revision_projections(&conn, &lineage).unwrap();
        assert_eq!(
            load_revision_state(&conn, &lineage, &original).unwrap(),
            before
        );
    }
}

#[test]
fn legacy_projection_guards_content_addresses_and_publication_without_a_transaction() {
    let (mut conn, lineage, branch, command, original) = legacy_projection_fixture(1);
    assert!(load_revision_for_save(&conn, &lineage, &original, ObjectCompression::none()).is_err());
    let before = archive_publication_counts(&conn);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM lineage_revision_state_projections",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let noop = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(noop.current, command.expected);
    assert_ne!(archive_publication_counts(&conn), before);
    for sql in [
        "UPDATE lineage_revision_state_projections SET projected_payload_id = original_payload_id",
        "DELETE FROM lineage_revision_state_projections",
        "DELETE FROM lineage_payload_object_refs WHERE payload_id IN (SELECT projected_payload_id FROM lineage_revision_state_projections)",
    ] {
        assert!(conn.execute(sql, []).is_err(), "ownership guard did not reject {sql}");
    }
    let row = conn.query_row(
        "SELECT projected_payload_id, projection_id FROM lineage_revision_state_projections WHERE original_payload_id = ?1",
        [original.state_payload_id.as_str()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).unwrap();
    let data = conn.query_row("SELECT payload_id FROM lineage_payload_object_refs WHERE payload_kind = 'data' LIMIT 1", [], |row| row.get::<_, String>(0)).unwrap();
    for (source, target) in [
        (row.0.as_str(), original.state_payload_id.as_str()),
        (
            original.state_payload_id.as_str(),
            original.state_payload_id.as_str(),
        ),
        (data.as_str(), row.0.as_str()),
        (original.state_payload_id.as_str(), data.as_str()),
    ] {
        assert!(conn.execute(
            "INSERT INTO lineage_revision_state_projections (lineage_id, original_payload_id, projected_payload_id, original_format_version, projection_id) VALUES (?1, ?2, ?3, 1, ?4)",
            (lineage.as_str(), source, target, &row.1),
        ).is_err());
    }
    conn.execute_batch("DROP TRIGGER lineage_revision_state_projection_update;")
        .unwrap();
    conn.execute(
        "UPDATE lineage_revision_state_projections SET projection_id = ?1",
        ["0".repeat(64)],
    )
    .unwrap();
    assert_integrity(load_revision_envelope(
        &conn,
        &lineage,
        &original,
        &mut OperationStats::default(),
    ));
    assert!(verify_revision_projections(&conn, &lineage).is_err());
    assert!(load_revision_state(&conn, &lineage, &original).is_ok());
    let backup = tempfile::tempdir().unwrap();
    let path = backup.path().join("corrupt-projection.db");
    crate::diagnostics::backup_connection_to(&conn, &path).unwrap();
    let report = crate::verify_lineage_backup(&path, lineage.as_str()).unwrap();
    assert!(!report.healthy);
    assert!(report
        .issues
        .iter()
        .any(|issue| issue.starts_with("revision projection:")));
    conn.execute(
        "UPDATE lineage_revision_state_projections SET projection_id = ?1",
        [&row.1],
    )
    .unwrap();
    let projected = PayloadId::from_db(row.0).unwrap();
    let projected = load_payload_ref(&conn, &lineage, &projected).unwrap();
    let stored_bytes = conn
        .query_row(
            "SELECT bytes FROM objects WHERE hash = ?1",
            [&projected.object_hash],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
        [&projected.object_hash],
    )
    .unwrap();
    assert!(
        load_revision_envelope(&conn, &lineage, &original, &mut OperationStats::default()).is_err()
    );
    assert!(load_revision_state(&conn, &lineage, &original).is_ok());
    conn.execute(
        "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
        (&stored_bytes, &projected.object_hash),
    )
    .unwrap();
    verify_revision_projections(&conn, &lineage).unwrap();
}

#[test]
fn legacy_projection_cold_audit_verifies_source_format_and_all_derived_bodies() {
    let (mut conn, lineage, branch, command, original) = legacy_projection_fixture(1);
    apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    let (projected, proof) = conn
        .query_row(
            "SELECT projected_payload_id, projection_id
             FROM lineage_revision_state_projections WHERE original_payload_id = ?1",
            [original.state_payload_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    conn.execute_batch("DROP TRIGGER lineage_revision_state_projection_update;")
        .unwrap();
    let mut encoder = CanonicalEncoder::new(b"smelt-lineage-revision-projection-v1\0");
    encoder.str(lineage.as_str());
    encoder.str(original.state_payload_id.as_str());
    encoder.str(&projected);
    encoder.u64(2);
    encoder.u64(u64::from(SHARED_REVISION_STATE_VERSION));
    conn.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    conn.execute(
        "UPDATE lineage_revision_state_projections SET original_format_version = 2, projection_id = ?1",
        [encoder.hash()],
    )
    .unwrap();
    assert_integrity(load_revision_envelope(
        &conn,
        &lineage,
        &original,
        &mut OperationStats::default(),
    ));
    assert_integrity(verify_revision_projections(&conn, &lineage));
    conn.execute(
        "UPDATE lineage_revision_state_projections SET original_format_version = 1, projection_id = ?1",
        [&proof],
    )
    .unwrap();
    conn.pragma_update(None, "ignore_check_constraints", false)
        .unwrap();
    let bodies = conn
        .prepare(
            "SELECT archive.role, object.hash, object.bytes
         FROM lineage_revision_state_roots archive
         JOIN lineage_sequence_roots root ON root.lineage_id = archive.lineage_id
           AND root.root_id = archive.root_id AND root.depth = 1
         JOIN lineage_sequence_entries entry ON entry.lineage_id = root.lineage_id
           AND entry.node_id = root.root_node_id
           AND entry.entry_index = CASE archive.role WHEN 'first_user_message' THEN 0 ELSE 1 END
         JOIN lineage_payload_object_refs payload ON payload.lineage_id = entry.lineage_id
           AND payload.payload_id = entry.payload_id
         JOIN objects object ON object.hash = payload.object_hash
         WHERE archive.lineage_id = ?1 AND archive.state_payload_id = ?2
         ORDER BY archive.role",
        )
        .unwrap()
        .query_map((lineage.as_str(), &projected), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(bodies.len(), 6);
    for (role, hash, stored_bytes) in bodies {
        conn.execute(
            "UPDATE objects SET bytes = zeroblob(stored_size) WHERE hash = ?1",
            [&hash],
        )
        .unwrap();
        let mut stats = OperationStats::default();
        load_revision_envelope(&conn, &lineage, &original, &mut stats).unwrap();
        assert_eq!(stats.payloads_read, 1);
        assert_eq!(stats.nodes_read, 0);
        assert!(
            verify_revision_projections(&conn, &lineage).is_err(),
            "unchecked derived body: {role}"
        );
        conn.execute(
            "UPDATE objects SET bytes = ?1 WHERE hash = ?2",
            (&stored_bytes, &hash),
        )
        .unwrap();
        verify_revision_projections(&conn, &lineage).unwrap();
    }
}

#[test]
fn legacy_projection_sharing_and_budget_one_gc_follow_all_original_owners() {
    let (mut conn, lineage, a, command_a, original_a) = legacy_projection_source(1);
    crate::schema::initialize_lineage_schema(&mut conn).unwrap();
    let b = branch_id('b');
    let first_b = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &b,
        &initial_session_commit(&b),
        ObjectCompression::none(),
    )
    .unwrap();
    let mut command_b = legacy_projection_archive_commit(&b, first_b.current);
    let previous_b = load_branch_record(&conn, &lineage, &b, false)
        .unwrap()
        .revision;
    let state = load_revision_state(&conn, &lineage, &original_a).unwrap();
    let alternate_bytes = serde_json::to_vec_pretty(&state).unwrap();
    let (original_b, _) = commit_revision(
        &mut conn,
        &lineage,
        &b,
        &previous_b.id,
        &previous_b.history_root,
        &previous_b.transcript_root,
        &alternate_bytes,
        LineageOperation::Append,
        2,
    )
    .unwrap();
    assert_ne!(original_a.state_payload_id, original_b.state_payload_id);
    command_b.expected = load_branch_record(&conn, &lineage, &b, false).unwrap().head;
    command_b.side_tables = SideTableSuffixes {
        start: HistoryIndex::new(1),
        ..SideTableSuffixes::default()
    };
    for (branch, command) in [(&a, &command_a), (&b, &command_b)] {
        assert_eq!(
            apply_lineage_session_commit(
                &mut conn,
                &lineage,
                branch,
                command,
                ObjectCompression::none()
            )
            .unwrap()
            .current,
            command.expected
        );
    }
    let projections = conn.prepare("SELECT projected_payload_id FROM lineage_revision_state_projections ORDER BY original_payload_id").unwrap()
        .query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(projections.len(), 2);
    assert_eq!(
        projections[0], projections[1],
        "equivalent legacy encodings must share their projection"
    );
    let projected = PayloadId::from_db(projections[0].clone()).unwrap();
    assert!(inspect_reachability(&conn, &lineage)
        .unwrap()
        .reachable_payloads
        .contains(projected.as_str()));
    delete_branch(&conn, &lineage, &a, 3).unwrap();
    let mut complete = false;
    for _ in 0..1000 {
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        assert!(load_payload_ref(&conn, &lineage, &projected).is_ok());
        if step.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert!(load_payload_ref(&conn, &lineage, &original_a.state_payload_id).is_err());
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM lineage_revision_state_projections",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    verify_revision_projections(&conn, &lineage).unwrap();
    let remaining = load_branch_record(&conn, &lineage, &b, false).unwrap();
    let mut stats = OperationStats::default();
    load_revision_envelope(&conn, &lineage, &remaining.revision, &mut stats).unwrap();
    assert_eq!(stats.payloads_read, 1);
    delete_branch(&conn, &lineage, &b, 4).unwrap();
    let mut complete = false;
    for _ in 0..1000 {
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        if step.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert!(load_payload_ref(&conn, &lineage, &projected).is_err());
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM lineage_revision_state_projections",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

fn legacy_projection_publication_event(role: &str) -> &'static str {
    match role {
        "projection" => "AFTER INSERT ON lineage_revision_state_projections",
        _ => archive_publication_event(role),
    }
}

#[test]
fn legacy_projection_publication_failures_roll_back_owners_proofs_and_receipts() {
    for role in ["value", "root", "owner", "projection", "receipt"] {
        let (mut conn, lineage, branch, command, original) = legacy_projection_fixture(1);
        let before = archive_publication_counts(&conn);
        let snapshot = load_revision_state(&conn, &lineage, &original).unwrap();
        conn.execute_batch(&format!("CREATE TEMP TRIGGER reject_projection {} BEGIN SELECT RAISE(ABORT, 'injected projection failure'); END;", legacy_projection_publication_event(role))).unwrap();
        assert!(apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none()
        )
        .is_err());
        assert_eq!(
            archive_publication_counts(&conn),
            before,
            "partial rows at {role}"
        );
        assert_eq!(
            load_revision_state(&conn, &lineage, &original).unwrap(),
            snapshot
        );
        assert_eq!(
            load_branch_record(&conn, &lineage, &branch, false)
                .unwrap()
                .revision,
            original
        );
        conn.execute_batch("DROP TRIGGER reject_projection;")
            .unwrap();
        assert_eq!(
            apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none()
            )
            .unwrap()
            .current,
            command.expected
        );
        verify_revision_projections(&conn, &lineage).unwrap();
    }
}

#[test]
fn legacy_projection_publication_is_process_crash_atomic() {
    const ROLE: &str = "SMELT_PROJECTION_CRASH_ROLE";
    const DB: &str = "SMELT_PROJECTION_CRASH_DB";
    let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
    let branch = branch_id('a');
    if let (Ok(role), Ok(path)) = (std::env::var(ROLE), std::env::var(DB)) {
        let mut conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        if role == "commit" {
            conn.commit_hook(Some(|| -> bool { std::process::abort() }))
                .unwrap();
        } else {
            conn.create_scalar_function(
                "smelt_test_projection_crash",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |_| -> rusqlite::Result<i64> { std::process::abort() },
            )
            .unwrap();
            conn.execute_batch(&format!("CREATE TEMP TRIGGER crash_projection {} BEGIN SELECT smelt_test_projection_crash(); END;", legacy_projection_publication_event(&role))).unwrap();
        }
        let snapshot = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
        let command = SessionCommit {
            session_id: branch.as_str().into(),
            expected: snapshot.head,
            identity: snapshot.identity,
            metadata: snapshot.metadata,
            history: crate::session_commit::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: crate::session_commit::HistoryLen::new(1),
                items: vec![],
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                ..SideTableSuffixes::default()
            },
            transcript_records: None,
        };
        let result = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        );
        panic!("projection crash boundary not reached: {result:?}");
    }
    let dir = tempfile::tempdir().unwrap();
    for role in ["value", "root", "owner", "projection", "receipt", "commit"] {
        let (conn, lineage, branch, command, original) = legacy_projection_fixture(1);
        let before = archive_publication_counts(&conn);
        let snapshot = load_revision_state(&conn, &lineage, &original).unwrap();
        let path = dir.path().join(format!("projection-{role}.db"));
        crate::diagnostics::backup_connection_to(&conn, &path).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("lineage::tests::legacy_projection_publication_is_process_crash_atomic")
            .arg("--nocapture")
            .env(ROLE, role)
            .env(DB, &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                status.signal(),
                Some(libc::SIGABRT),
                "wrong failure at {role}"
            );
        }
        let mut restarted = Connection::open(&path).unwrap();
        restarted.pragma_update(None, "foreign_keys", true).unwrap();
        assert_eq!(archive_publication_counts(&restarted), before);
        assert_eq!(
            load_revision_state(&restarted, &lineage, &original).unwrap(),
            snapshot
        );
        assert_eq!(
            load_branch_record(&restarted, &lineage, &branch, false)
                .unwrap()
                .revision,
            original
        );
        let receipt = apply_lineage_session_commit(
            &mut restarted,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(receipt.current, command.expected);
        assert_eq!(
            apply_lineage_session_commit(
                &mut restarted,
                &lineage,
                &branch,
                &command,
                ObjectCompression::none()
            )
            .unwrap(),
            receipt
        );
        verify_revision_projections(&restarted, &lineage).unwrap();
        drop(restarted);
        let reopened = Connection::open(&path).unwrap();
        reopened.pragma_update(None, "query_only", true).unwrap();
        let mut stats = OperationStats::default();
        assert!(matches!(
            load_revision_envelope(&reopened, &lineage, &original, &mut stats).unwrap(),
            StoredRevisionState::Shared(_)
        ));
        assert_eq!(stats.payloads_read, 1);
        assert_eq!(stats.nodes_read, 0);
        assert_eq!(
            load_revision_state(&reopened, &lineage, &original).unwrap(),
            snapshot
        );
    }
}

#[test]
fn legacy_shared_message_transition_preserves_noops_history_forks_and_receipts() {
    let (mut conn, lineage) = legacy_setup();
    let branch = branch_id('a');
    let initial = initial_session_commit(&branch);
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial,
        ObjectCompression::none(),
    )
    .unwrap();
    let archived = archived_session_commit(&branch, first.current);
    let second = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &archived,
        ObjectCompression::none(),
    )
    .unwrap();
    let before = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
    let revision = load_branch_record(&conn, &lineage, &branch, false)
        .unwrap()
        .revision;
    let original = hydrate_payload(
        &conn,
        &lineage,
        &revision.state_payload_id,
        PayloadKind::RevisionState,
        &mut OperationStats::default(),
    )
    .unwrap();
    assert!(matches!(
        load_revision_envelope(&conn, &lineage, &revision, &mut OperationStats::default()).unwrap(),
        StoredRevisionState::Legacy(_)
    ));
    crate::schema::initialize_lineage_schema(&mut conn).unwrap();
    let mut command = archived.clone();
    command.expected = second.current;
    command.side_tables = SideTableSuffixes {
        start: HistoryIndex::new(1),
        ..SideTableSuffixes::default()
    };
    let noop = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(noop.current, second.current);
    assert_eq!(
        load_branch_record(&conn, &lineage, &branch, false)
            .unwrap()
            .revision
            .id,
        revision.id
    );
    assert_eq!(
        hydrate_payload(
            &conn,
            &lineage,
            &revision.state_payload_id,
            PayloadKind::RevisionState,
            &mut OperationStats::default()
        )
        .unwrap(),
        original
    );
    command.metadata.title = Some("shared title".into());
    command.metadata.updated_at = 3;
    let shared = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::none(),
    )
    .unwrap();
    let record = load_branch_record(&conn, &lineage, &branch, false).unwrap();
    let mut stats = OperationStats::default();
    assert!(matches!(
        load_revision_envelope(&conn, &lineage, &record.revision, &mut stats).unwrap(),
        StoredRevisionState::Shared(_)
    ));
    assert_eq!(stats.payloads_read, 1);
    assert_eq!(
        stats.nodes_read, 0,
        "normal envelopes must not hydrate message bodies"
    );
    let snapshot = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
    assert_eq!(snapshot.metadata, command.metadata);
    assert_eq!(snapshot.side_tables, before.side_tables);
    assert_eq!(
        load_revision_state(&conn, &lineage, &revision)
            .unwrap()
            .metadata
            .first_user_message,
        archived.metadata.first_user_message
    );
    command.expected = shared.current;
    for (updated_at, message) in [(4, None), (5, Some(String::new()))] {
        command.metadata.updated_at = updated_at;
        command.metadata.first_user_message = message;
        let changed = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        assert!(changed.current.revision > command.expected.revision);
        assert_eq!(
            lineage_session_snapshot(&conn, &lineage, &branch)
                .unwrap()
                .metadata,
            command.metadata
        );
        command.expected = changed.current;
        let unchanged = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(unchanged.current, command.expected);
    }
    let mut original_noop = archived.clone();
    original_noop.expected = second.current;
    original_noop.side_tables = SideTableSuffixes {
        start: HistoryIndex::new(1),
        ..SideTableSuffixes::default()
    };
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &original_noop,
            ObjectCompression::none()
        )
        .unwrap(),
        noop
    );
    let fork = branch_id('b');
    fork_branch(&mut conn, &lineage, &branch, &fork, Some(&revision.id), 6).unwrap();
    let snapshot = lineage_session_snapshot(&conn, &lineage, &fork).unwrap();
    assert_eq!(
        snapshot.metadata.first_user_message,
        archived.metadata.first_user_message
    );
    assert_eq!(snapshot.side_tables, before.side_tables);
    let fork_command = SessionCommit {
        session_id: fork.as_str().into(),
        expected: snapshot.head,
        identity: snapshot.identity,
        metadata: snapshot.metadata,
        history: crate::session_commit::HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: crate::session_commit::HistoryLen::new(1),
            items: vec![],
        },
        side_tables: SideTableSuffixes {
            start: HistoryIndex::new(1),
            ..SideTableSuffixes::default()
        },
        transcript_records: None,
    };
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &fork,
            &fork_command,
            ObjectCompression::none()
        )
        .unwrap()
        .current,
        fork_command.expected
    );
    assert_eq!(
        load_branch_record(&conn, &lineage, &fork, false)
            .unwrap()
            .revision
            .id,
        revision.id
    );
    assert_eq!(
        hydrate_payload(
            &conn,
            &lineage,
            &revision.state_payload_id,
            PayloadKind::RevisionState,
            &mut OperationStats::default()
        )
        .unwrap(),
        original
    );
}

#[test]
fn captured_fork_noop_uses_the_effective_branch_and_revision_metadata() {
    for branch_has_usage in [false, true] {
        let (mut conn, lineage) = setup();
        let branch = branch_id('a');
        let mut initial = initial_session_commit(&branch);
        initial.metadata.accounting_json.as_mut().unwrap()["context"] = serde_json::Value::from(1);
        let first = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &initial,
            ObjectCompression::none(),
        )
        .unwrap();
        let captured = load_branch_record(&conn, &lineage, &branch, false)
            .unwrap()
            .revision
            .id;
        let mut changed = initial.clone();
        changed.expected = first.current;
        changed.history.start = HistoryIndex::new(1);
        changed.history.items.clear();
        changed.metadata.updated_at = 2;
        changed.metadata.accounting_json = Some(if branch_has_usage {
            serde_json::json!({"context": 9, "session_usage": {"input_tokens": 20}})
        } else {
            serde_json::json!({"context": 9})
        });
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &changed,
            ObjectCompression::none(),
        )
        .unwrap();
        let fork = branch_id('b');
        fork_branch(&mut conn, &lineage, &branch, &fork, Some(&captured), 3).unwrap();
        let snapshot = lineage_session_snapshot(&conn, &lineage, &fork).unwrap();
        assert_eq!(
            snapshot.metadata.accounting_json.as_ref().unwrap()["context"],
            serde_json::Value::from(if branch_has_usage { 1 } else { 9 })
        );
        let noop = SessionCommit {
            session_id: fork.as_str().into(),
            expected: snapshot.head,
            identity: snapshot.identity,
            metadata: snapshot.metadata,
            history: crate::session_commit::HistorySuffix {
                start: HistoryIndex::new(1),
                final_len: crate::session_commit::HistoryLen::new(1),
                items: Vec::new(),
            },
            side_tables: SideTableSuffixes {
                start: HistoryIndex::new(1),
                ..SideTableSuffixes::default()
            },
            transcript_records: None,
        };
        let receipt = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &fork,
            &noop,
            ObjectCompression::none(),
        )
        .unwrap();
        assert_eq!(
            receipt.current, snapshot.head,
            "effective metadata did not change"
        );
        assert_eq!(
            load_branch_record(&conn, &lineage, &fork, false)
                .unwrap()
                .revision
                .id,
            captured
        );
        assert_eq!(
            apply_lineage_session_commit(
                &mut conn,
                &lineage,
                &fork,
                &noop,
                ObjectCompression::none()
            )
            .unwrap(),
            receipt
        );
    }
}

#[test]
fn receipt_revision_lookup_is_exact_and_missing_results_do_not_fall_back_to_head() {
    fn receipt_envelope(conn: &Connection, lineage: &LineageId, receipt: &SaveReceipt) -> Vec<u8> {
        let branch = BranchId::new(receipt.session_id.clone()).unwrap();
        let id =
            branch_revision_at_sequence(conn, lineage, &branch, receipt.current.revision.get())
                .unwrap();
        let revision = load_revision(conn, lineage, &id).unwrap();
        assert_eq!(
            revision.history_root.item_count,
            receipt.current.history_len.get()
        );
        assert_eq!(
            revision.transcript_root.item_count,
            receipt.current.transcript_record_count.get()
        );
        let mut stats = OperationStats::default();
        let StoredRevisionState::Shared(state) =
            load_revision_envelope(conn, lineage, &revision, &mut stats).unwrap()
        else {
            panic!("expected a compact revision envelope");
        };
        assert_eq!(stats.payloads_read, 1);
        assert_eq!(stats.nodes_read, 0);
        serde_json::to_vec(&state).unwrap()
    }

    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let mut initial = initial_session_commit(&branch);
    initial.side_tables.metadata_snapshots = vec![(
        HistoryIndex::new(1),
        serde_json::json!({"title": "first", "first_user_message": "retained"}),
    )];
    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial,
        ObjectCompression::none(),
    )
    .unwrap();
    let first_envelope = receipt_envelope(&conn, &lineage, &first);
    let captured =
        branch_revision_at_sequence(&conn, &lineage, &branch, first.current.revision.get())
            .unwrap();

    let mut changed = initial.clone();
    changed.expected = first.current;
    changed.history.start = HistoryIndex::new(1);
    changed.history.items.clear();
    changed.metadata = session_metadata(2, "second");
    changed.side_tables.start = HistoryIndex::new(1);
    changed.side_tables.metadata_snapshots[0].1["title"] = serde_json::Value::from("second");
    let second = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &changed,
        ObjectCompression::none(),
    )
    .unwrap();
    let second_envelope = receipt_envelope(&conn, &lineage, &second);
    assert_ne!(first_envelope, second_envelope);
    assert_eq!(receipt_envelope(&conn, &lineage, &first), first_envelope);
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &initial,
            ObjectCompression::none(),
        )
        .unwrap(),
        first,
    );

    let mut noop = changed.clone();
    noop.expected = second.current;
    let noop_receipt = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &noop,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(noop_receipt.current, second.current);
    assert_eq!(
        receipt_envelope(&conn, &lineage, &noop_receipt),
        second_envelope
    );

    let fork = branch_id('b');
    fork_branch(&mut conn, &lineage, &branch, &fork, Some(&captured), 3).unwrap();
    let snapshot = lineage_session_snapshot(&conn, &lineage, &fork).unwrap();
    let fork_noop = SessionCommit {
        session_id: fork.as_str().into(),
        expected: snapshot.head,
        identity: snapshot.identity,
        metadata: snapshot.metadata,
        history: crate::session_commit::HistorySuffix {
            start: HistoryIndex::new(1),
            final_len: crate::session_commit::HistoryLen::new(1),
            items: Vec::new(),
        },
        side_tables: snapshot.side_tables,
        transcript_records: None,
    };
    let fork_receipt = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &fork,
        &fork_noop,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(fork_receipt.current, snapshot.head);
    assert_eq!(
        receipt_envelope(&conn, &lineage, &fork_receipt),
        first_envelope
    );

    let abandoned =
        branch_revision_at_sequence(&conn, &lineage, &branch, second.current.revision.get())
            .unwrap();
    rewind_branch(&mut conn, &lineage, &branch, &abandoned, &captured, 4).unwrap();
    assert_eq!(
        receipt_envelope(&conn, &lineage, &noop_receipt),
        second_envelope
    );
    let mut complete = false;
    for _ in 0..1000 {
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        assert_eq!(receipt_envelope(&conn, &lineage, &first), first_envelope);
        assert_eq!(
            receipt_envelope(&conn, &lineage, &fork_receipt),
            first_envelope
        );
        if step.complete {
            complete = true;
            break;
        }
    }
    assert!(complete, "fixture reclamation did not finish");
    assert_integrity(branch_revision_at_sequence(
        &conn,
        &lineage,
        &branch,
        noop_receipt.current.revision.get(),
    ));
    // Legacy no-op receipts can outlive their unreachable revision association.
    // Missing outcomes must not be replaced with the branch's newer head.
    let fingerprint = crate::session_command::session_commit_fingerprint(&noop).unwrap();
    let stored = load_session_receipt(&conn, &lineage, &branch, &fingerprint, "save")
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(stored).unwrap()["save"],
        serde_json::to_value(&noop_receipt).unwrap()
    );
}

#[test]
fn v3_upgrade_preserves_exact_session_objects_and_receipts() {
    let mut conn = crate::schema::tests::v3_connection();
    let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
    create_lineage(&conn, &lineage, 1).unwrap();
    let branch = branch_id('a');
    let mut initial = initial_session_commit(&branch);
    initial.history.items.extend([
        protocol::HistoryItem::note(protocol::HistoryNote::named_context("shared", "value")),
        protocol::HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
            "normal", "plan", "mode",
        )),
    ]);
    initial.history.final_len = crate::session_commit::HistoryLen::new(3);
    let receipt = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial,
        ObjectCompression::None,
    )
    .unwrap();
    let mut current = receipt.current;
    for step in 0..10 {
        let mut command = initial.clone();
        command.expected = current;
        command.metadata.updated_at = step + 2;
        command.metadata.title = Some(format!("revision-{step}"));
        let len = current.history_len.get();
        command.history.start = HistoryIndex::new(len);
        command.history.items = if step % 2 == 0 {
            vec![protocol::HistoryItem::note(
                protocol::HistoryNote::named_context("shared", ""),
            )]
        } else {
            Vec::new()
        };
        command.history.final_len =
            crate::session_commit::HistoryLen::new(len + command.history.items.len() as u64);
        current = apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &command,
            ObjectCompression::None,
        )
        .unwrap()
        .current;
    }
    let snapshot = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
    conn.pragma_update(None, "query_only", true).unwrap();
    assert_eq!(
        history_semantic_range(
            &conn,
            &lineage,
            &snapshot.history_root,
            HistorySemantic::Context("shared"),
            0..u64::MAX,
            true
        )
        .unwrap()
        .0,
        Some((7, "shared".into()))
    );
    assert_eq!(
        history_semantic_range(
            &conn,
            &lineage,
            &snapshot.history_root,
            HistorySemantic::Mode,
            0..2,
            true
        )
        .unwrap()
        .0,
        None
    );
    assert_eq!(
        history_semantic_range(
            &conn,
            &lineage,
            &snapshot.history_root,
            HistorySemantic::BaseMode,
            2..3,
            false
        )
        .unwrap()
        .0,
        Some((2, "normal".into()))
    );
    assert_eq!(crate::schema::user_version(&conn).unwrap(), 3);
    conn.pragma_update(None, "query_only", false).unwrap();
    let tables = [
        "objects",
        "lineage_revisions",
        "lineage_sequence_roots",
        "lineage_sequence_nodes",
        "lineage_sequence_entries",
        "lineage_payload_object_refs",
        "lineage_branches",
        "lineage_branch_revisions",
        "lineage_session_receipts",
        "lineage_commit_receipts",
    ];
    let rows = |conn: &Connection| {
        tables.map(|table| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let columns = stmt.column_count();
            stmt.query_map([], |row| {
                (0..columns)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        })
    };
    let before = rows(&conn);
    crate::schema::initialize_lineage_schema(&mut conn).unwrap();
    assert_eq!(rows(&conn), before);
    assert_eq!(
        lineage_session_snapshot(&conn, &lineage, &branch).unwrap(),
        snapshot
    );
    assert_eq!(
        history_semantic_range(
            &conn,
            &lineage,
            &snapshot.history_root,
            HistorySemantic::Context("shared"),
            0..u64::MAX,
            true
        )
        .unwrap()
        .0,
        Some((7, "shared".into()))
    );
    assert_eq!(
        history_semantic_range(
            &conn,
            &lineage,
            &snapshot.history_root,
            HistorySemantic::Mode,
            0..u64::MAX,
            true
        )
        .unwrap()
        .0,
        Some((2, "plan".into()))
    );
    assert_eq!(
        history_semantic_range(
            &conn,
            &lineage,
            &snapshot.history_root,
            HistorySemantic::BaseMode,
            0..u64::MAX,
            false
        )
        .unwrap()
        .0,
        Some((2, "normal".into()))
    );
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &initial,
            ObjectCompression::None,
        )
        .unwrap(),
        receipt
    );
    assert_eq!(rows(&conn), before);
    crate::schema::validate_lineage_schema(&conn).unwrap();
}

#[test]
fn corrupt_history_rolls_back_v3_migration() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn.execute_batch(include_str!("../lineage_v3.sql"))
        .unwrap();
    conn.pragma_update(None, "user_version", 3).unwrap();
    let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
    create_lineage(&conn, &lineage, 1).unwrap();
    let branch = branch_id('a');
    let command = initial_session_commit(&branch);
    apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &command,
        ObjectCompression::None,
    )
    .unwrap();
    conn.execute("UPDATE objects SET codec = 'none', raw_size = 0, stored_size = 0, bytes = x'' WHERE hash IN (SELECT object_hash FROM lineage_payload_object_refs WHERE payload_kind = 'history')", []).unwrap();
    assert!(matches!(
        crate::schema::initialize_lineage_schema(&mut conn),
        Err(StoreError::Integrity(_))
    ));
    assert_eq!(crate::schema::user_version(&conn).unwrap(), 3);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'lineage_history_indexes'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    crate::schema::validate_lineage_schema(&conn).unwrap();
}

#[test]
fn root_publication_sql_work_is_independent_of_archive_size() {
    let (mut conn, lineage) = setup();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    let mut steps = Vec::new();
    for size in [1, 4096] {
        let items = (0..size).map(bytes).collect::<Vec<_>>();
        let (root, _) =
            append_sequence(&mut conn, &lineage, &empty, &items, ObjectCompression::None).unwrap();
        let mut statement = conn
            .prepare(
                "INSERT OR IGNORE INTO lineage_sequence_roots
             (lineage_id, root_id, root_kind, root_node_id, depth, item_count, byte_count)
             VALUES (?1, ?2, 'history', ?3, ?4, ?5, ?6)",
            )
            .unwrap();
        assert_eq!(
            statement
                .execute(rusqlite::params![
                    lineage.as_str(),
                    root.id.as_str(),
                    root.node_id.as_ref().unwrap().as_str(),
                    root.depth,
                    i64::try_from(root.item_count).unwrap(),
                    i64::try_from(root.byte_count).unwrap(),
                ])
                .unwrap(),
            0
        );
        assert_eq!(
            statement.get_status(rusqlite::StatementStatus::FullscanStep),
            0
        );
        steps.push(statement.get_status(rusqlite::StatementStatus::VmStep));
    }
    assert!(
        steps[1] <= steps[0] + 8,
        "root publication VM steps grew: {steps:?}"
    );
}

#[test]
fn rebuilding_sequences_reuses_completed_leaf_and_internal_nodes() {
    let (conn, lineage) = setup();
    for kind in [
        SequenceKind::History,
        SequenceKind::Transcript,
        SequenceKind::Data,
    ] {
        let items: Vec<_> = (0..65)
            .map(|index| format!("item {index}").into_bytes())
            .collect();
        let mut first_stats = OperationStats::default();
        let first = build_sequence_from_empty(
            &conn,
            &lineage,
            kind,
            &items,
            ObjectCompression::None,
            &mut first_stats,
        )
        .unwrap();
        insert_root(&conn, &lineage, &first, &mut first_stats).unwrap();
        assert!(first.depth > 1);
        assert!(first_stats.nodes_written > 0);
        let mut repeated_stats = OperationStats::default();
        let repeated = build_sequence_from_empty(
            &conn,
            &lineage,
            kind,
            &items,
            ObjectCompression::None,
            &mut repeated_stats,
        )
        .unwrap();
        assert_eq!(repeated, first);
        assert_eq!(repeated_stats.nodes_written, 0);
        validate_sequence(&conn, &lineage, &repeated).unwrap();
    }
}

#[test]
fn completed_nodes_reject_direct_sql_mutation_and_incomplete_publication() {
    let (mut conn, lineage) = setup();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    let (root, _) = append_sequence(
        &mut conn,
        &lineage,
        &empty,
        &[b"item".to_vec()],
        ObjectCompression::None,
    )
    .unwrap();
    let node = root.node_id.as_ref().unwrap().as_str();
    for sql in [
        "DELETE FROM lineage_sequence_entries WHERE lineage_id = ?1 AND node_id = ?2",
        "UPDATE lineage_sequence_entries SET byte_count = byte_count WHERE lineage_id = ?1 AND node_id = ?2",
        "UPDATE lineage_sequence_nodes SET byte_count = byte_count WHERE lineage_id = ?1 AND node_id = ?2",
        "DELETE FROM lineage_completed_sequence_nodes WHERE lineage_id = ?1 AND node_id = ?2",
        "UPDATE lineage_completed_sequence_nodes SET node_id = node_id WHERE lineage_id = ?1 AND node_id = ?2",
    ] {
        assert!(conn.execute(sql, (lineage.as_str(), node)).is_err(), "accepted {sql}");
    }
    for conflict in ["IGNORE", "REPLACE"] {
        let error = conn
            .execute(
                &format!(
                    "INSERT OR {conflict} INTO lineage_sequence_nodes
                     SELECT * FROM lineage_sequence_nodes WHERE lineage_id = ?1 AND node_id = ?2"
                ),
                (lineage.as_str(), node),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("lineage sequence nodes are immutable"));
    }
    // The immutable leaf remains usable for another publication.
    insert_root(&conn, &lineage, &root, &mut OperationStats::default()).unwrap();
    validate_sequence(&conn, &lineage, &root).unwrap();

    let incomplete = "a".repeat(64);
    let parent = "b".repeat(64);
    conn.execute(
        "INSERT INTO lineage_sequence_nodes VALUES (?1, ?2, 'history', 'leaf', 0, 1, 1, 4)",
        (lineage.as_str(), &incomplete),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO lineage_sequence_nodes VALUES (?1, ?2, 'history', 'internal', 1, 1, 1, 4)",
        (lineage.as_str(), &parent),
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO lineage_completed_sequence_nodes VALUES (?1, ?2)",
            (lineage.as_str(), &incomplete),
        )
        .is_err());
    assert!(conn.execute(
        "INSERT INTO lineage_sequence_entries VALUES (?1, ?2, 0, 'child', NULL, ?3, 1, 4, 1, 4)",
        (lineage.as_str(), &parent, &incomplete),
    ).is_err());
    assert!(conn
        .execute(
            "INSERT INTO lineage_sequence_roots VALUES (?1, ?2, 'history', ?3, 1, 1, 4)",
            (lineage.as_str(), "c".repeat(64), &incomplete),
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT INTO lineage_sequence_entries SELECT * FROM lineage_sequence_entries
         WHERE lineage_id = ?1 AND node_id = ?2",
            (lineage.as_str(), node),
        )
        .is_err());

    // Owner deletion is permitted after all roots release the node, including
    // foreign-key cascades to its entries and completion marker.
    conn.execute(
        "DELETE FROM lineage_sequence_roots WHERE lineage_id = ?1 AND root_id = ?2",
        (lineage.as_str(), root.id.as_str()),
    )
    .unwrap();
    conn.execute(
        "DELETE FROM lineage_sequence_nodes WHERE lineage_id = ?1 AND node_id = ?2",
        (lineage.as_str(), node),
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM lineage_completed_sequence_nodes",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM lineage_sequence_entries", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
}

#[test]
fn production_session_adapter_roundtrips_retries_and_rewinds_suffixes() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');
    let initial = initial_session_commit(&branch);

    let first = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &initial,
        ObjectCompression::None,
    )
    .unwrap();
    assert_eq!(first.previous, StoreHead::default());
    assert_eq!(first.current.revision.get(), 1);
    assert_eq!(first.current.history_len.get(), 1);
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &initial,
            ObjectCompression::None,
        )
        .unwrap(),
        first
    );

    let mut append = initial.clone();
    append.expected = first.current;
    append.metadata = session_metadata(2, "second");
    append.history = crate::session_commit::HistorySuffix {
        start: HistoryIndex::new(1),
        final_len: crate::session_commit::HistoryLen::new(2),
        items: vec![protocol::HistoryItem::system("two")],
    };
    let second = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &append,
        ObjectCompression::None,
    )
    .unwrap();
    assert_eq!(second.current.revision.get(), 2);
    assert_eq!(second.current.history_len.get(), 2);
    assert_eq!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &append,
            ObjectCompression::None,
        )
        .unwrap(),
        second
    );

    let mut replace = initial.clone();
    replace.expected = second.current;
    replace.metadata = session_metadata(3, "replacement");
    replace.history = crate::session_commit::HistorySuffix {
        start: HistoryIndex::new(1),
        final_len: crate::session_commit::HistoryLen::new(2),
        items: vec![protocol::HistoryItem::system("replacement")],
    };
    let third = apply_lineage_session_commit(
        &mut conn,
        &lineage,
        &branch,
        &replace,
        ObjectCompression::None,
    )
    .unwrap();
    assert_eq!(third.current.revision.get(), 3);
    let snapshot = lineage_session_snapshot(&conn, &lineage, &branch).unwrap();
    assert_eq!(snapshot.metadata, replace.metadata);
    assert_eq!(snapshot.head, third.current);
    assert_eq!(
        lineage_history_range(&conn, &lineage, &branch, 0, 2).unwrap(),
        vec![
            protocol::HistoryItem::system("one"),
            protocol::HistoryItem::system("replacement")
        ]
    );
    assert!(lineage_transcript_range(&conn, &lineage, &branch, 0, 0)
        .unwrap()
        .is_empty());
}

#[test]
fn session_suffixes_cannot_start_beyond_the_expected_head() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('a');

    let mut history = initial_session_commit(&branch);
    history.history = crate::session_commit::HistorySuffix {
        start: HistoryIndex::new(2),
        final_len: crate::session_commit::HistoryLen::new(2),
        items: Vec::new(),
    };
    assert!(matches!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &history,
            ObjectCompression::None,
        ),
        Err(SessionCommitFailure::InvalidHistorySuffixStart { start, current_len })
            if start == HistoryIndex::new(2)
                && current_len == crate::session_commit::HistoryLen::ZERO
    ));

    let mut records = initial_session_commit(&branch);
    records.transcript_records = Some(crate::session_commit::TranscriptRecordSuffix {
        start: crate::session_commit::TranscriptRecordIndex::new(2),
        records: Vec::new(),
    });
    assert!(matches!(
        apply_lineage_session_commit(
            &mut conn,
            &lineage,
            &branch,
            &records,
            ObjectCompression::None,
        ),
        Err(SessionCommitFailure::InvalidTranscriptRecordSuffix { start, current_len })
            if start == crate::session_commit::TranscriptRecordIndex::new(2)
                && current_len == crate::session_commit::TranscriptRecordCount::ZERO
    ));
}

#[test]
fn sequence_bounds_and_stale_root_metadata_are_rejected() {
    let (mut conn, lineage) = setup();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();

    assert_integrity(sequence_item(&conn, &lineage, &empty, 0));
    assert_integrity(sequence_item(&conn, &lineage, &empty, u64::MAX));

    let (root, _) = append_sequence(
        &mut conn,
        &lineage,
        &empty,
        &[b"one".to_vec(), b"two".to_vec()],
        ObjectCompression::default(),
    )
    .unwrap();
    assert_eq!(sequence_item(&conn, &lineage, &root, 1).unwrap().0, b"two");
    assert_integrity(sequence_item(&conn, &lineage, &root, root.item_count));
    assert_integrity(sequence_item(&conn, &lineage, &root, u64::MAX));

    let mut stale = root.clone();
    stale.item_count += 1;
    assert_integrity(append_sequence(
        &mut conn,
        &lineage,
        &stale,
        &[b"three".to_vec()],
        ObjectCompression::default(),
    ));
    assert_integrity(sequence_range(&conn, &lineage, &stale, 0, 1));
    assert_integrity(sequence_tail(&conn, &lineage, &stale, 1));
    assert_integrity(sequence_item(&conn, &lineage, &stale, 0));
    assert_integrity(split_sequence(&mut conn, &lineage, &stale, 1));
    assert_integrity(validate_sequence(&conn, &lineage, &stale));
}

#[test]
fn sequence_leaves_enforce_byte_bounds_through_append_and_split() {
    let (mut conn, lineage) = setup();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::Transcript).unwrap();
    let below_target = vec![b'a'; usize::try_from(LEAF_TARGET_BYTES - 1).unwrap()];
    let one_byte = vec![b'b'];

    let (below, _) = append_sequence(
        &mut conn,
        &lineage,
        &empty,
        std::slice::from_ref(&below_target),
        ObjectCompression::default(),
    )
    .unwrap();
    let below_leaves = reachable_leaves(&conn, &lineage, &below);
    assert_eq!(below_leaves.len(), 1);
    assert_eq!(below_leaves[0].byte_count, LEAF_TARGET_BYTES - 1);

    let (exact, _) = append_sequence(
        &mut conn,
        &lineage,
        &below,
        std::slice::from_ref(&one_byte),
        ObjectCompression::default(),
    )
    .unwrap();
    let exact_leaves = reachable_leaves(&conn, &lineage, &exact);
    assert_eq!(exact_leaves.len(), 1);
    assert_eq!(exact_leaves[0].byte_count, LEAF_TARGET_BYTES);
    assert_eq!(exact_leaves[0].entries.len(), 2);

    let (crossed, _) = append_sequence(
        &mut conn,
        &lineage,
        &exact,
        std::slice::from_ref(&one_byte),
        ObjectCompression::default(),
    )
    .unwrap();
    let crossed_leaves = reachable_leaves(&conn, &lineage, &crossed);
    assert_eq!(crossed_leaves.len(), 2);
    assert!(crossed_leaves
        .iter()
        .any(|leaf| leaf.byte_count == LEAF_TARGET_BYTES));
    assert!(crossed_leaves
        .iter()
        .any(|leaf| leaf.byte_count == 1 && leaf.entries.len() == 1));
    validate_sequence(&conn, &lineage, &crossed).unwrap();

    let ((left, right), _) = split_sequence(&mut conn, &lineage, &crossed, 1).unwrap();
    reachable_leaves(&conn, &lineage, &left);
    reachable_leaves(&conn, &lineage, &right);
    assert_eq!(
        sequence_range(&conn, &lineage, &left, 0, left.item_count)
            .unwrap()
            .0,
        vec![below_target.clone()]
    );
    assert_eq!(
        sequence_range(&conn, &lineage, &right, 0, right.item_count)
            .unwrap()
            .0,
        vec![one_byte.clone(), one_byte.clone()]
    );

    let oversized = vec![b'c'; usize::try_from(LEAF_TARGET_BYTES + 1).unwrap()];
    let (oversized_root, _) = append_sequence(
        &mut conn,
        &lineage,
        &empty,
        std::slice::from_ref(&oversized),
        ObjectCompression::default(),
    )
    .unwrap();
    let oversized_leaves = reachable_leaves(&conn, &lineage, &oversized_root);
    assert_eq!(oversized_leaves.len(), 1);
    assert_eq!(oversized_leaves[0].entries.len(), 1);
    assert_eq!(oversized_leaves[0].byte_count, LEAF_TARGET_BYTES + 1);
    validate_sequence(&conn, &lineage, &oversized_root).unwrap();
}

#[test]
fn sequence_extent_overflow_and_repeated_payload_corruption_are_rejected() {
    let overflow_entries = vec![
        NodeEntry {
            target: EntryTarget::Item(PayloadId("a".repeat(64))),
            item_count: 1,
            byte_count: u64::MAX,
            cumulative_item_count: 0,
            cumulative_byte_count: 0,
        },
        NodeEntry {
            target: EntryTarget::Item(PayloadId("b".repeat(64))),
            item_count: 1,
            byte_count: 1,
            cumulative_item_count: 0,
            cumulative_byte_count: 0,
        },
    ];
    assert_integrity(make_entries(overflow_entries));

    let (conn, lineage) = setup();
    let mut stats = OperationStats::default();
    let payload = put_payload(
        &conn,
        &lineage,
        PayloadKind::History,
        b"same",
        ObjectCompression::default(),
        &mut stats,
    )
    .unwrap();
    conn.execute_batch(
        "DROP TRIGGER lineage_sequence_entry_insert;
         DROP TRIGGER lineage_sequence_entry_complete;
         DROP TRIGGER lineage_sequence_root_insert;",
    )
    .unwrap();
    let node = create_node(
        &conn,
        &lineage,
        SequenceKind::History,
        0,
        vec![
            NodeEntry {
                target: EntryTarget::Item(payload.id.clone()),
                item_count: 1,
                byte_count: payload.byte_count,
                cumulative_item_count: 0,
                cumulative_byte_count: 0,
            },
            NodeEntry {
                target: EntryTarget::Item(payload.id),
                item_count: 1,
                byte_count: payload.byte_count + 1,
                cumulative_item_count: 0,
                cumulative_byte_count: 0,
            },
        ],
        &mut stats,
    )
    .unwrap();
    let root = make_root(&lineage, SequenceKind::History, Some(&node));
    insert_root(&conn, &lineage, &root, &mut stats).unwrap();
    let mut validation = ValidationState {
        active_nodes: HashSet::new(),
        validated_nodes: HashMap::new(),
        validated_payloads: HashMap::new(),
        stats: OperationStats::default(),
    };
    assert_integrity(validate_node(
        &conn,
        &lineage,
        &node.id,
        SequenceKind::History,
        0,
        &mut validation,
    ));
    assert_eq!(validation.stats.payloads_read, 1);
    assert!(validation.active_nodes.is_empty());
}

fn publication_row_counts(conn: &Connection) -> [i64; 5] {
    [
        "objects",
        "lineage_payload_object_refs",
        "lineage_sequence_nodes",
        "lineage_sequence_entries",
        "lineage_sequence_roots",
    ]
    .map(|table| {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    })
}

fn reclamation_work_units(conn: &Connection, lineage: &LineageId) -> usize {
    let canonical_rows = [
        "lineage_turn_transitions",
        "lineage_session_receipts",
        "lineage_commit_receipts",
        "lineage_branch_revisions",
        "lineage_turns",
        "lineage_revisions",
        "lineage_sequence_roots",
        "lineage_transcript_extent_nodes",
        "lineage_sequence_entries",
        "lineage_sequence_nodes",
        "lineage_completed_sequence_nodes",
        "lineage_history_indexes",
        "lineage_history_index_nodes",
        "lineage_transcript_record_profiles",
        "lineage_payload_nested_object_refs",
        "lineage_payload_object_refs",
    ]
    .into_iter()
    .map(|table| {
        usize::try_from(
            conn.query_row(
                &format!("SELECT count(*) FROM {table} WHERE lineage_id = ?1"),
                [lineage.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        )
        .unwrap()
    })
    .sum::<usize>();
    let uncleared_deleted_branch_heads = usize::try_from(
        conn.query_row(
            "SELECT count(*) FROM lineage_branches
             WHERE lineage_id = ?1 AND deleted_at IS NOT NULL AND head_revision_id IS NOT NULL",
            [lineage.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
    )
    .unwrap();
    let objects = usize::try_from(
        conn.query_row("SELECT count(*) FROM objects", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
    )
    .unwrap();
    canonical_rows
        .saturating_add(uncleared_deleted_branch_heads)
        .saturating_add(objects)
}

fn install_publication_abort(conn: &Connection, table: &str) {
    conn.execute_batch(&format!(
        "CREATE TEMP TRIGGER abort_lineage_publication
             AFTER INSERT ON {table}
             BEGIN SELECT RAISE(ABORT, 'abort lineage publication'); END;"
    ))
    .unwrap();
}

fn remove_publication_abort(conn: &Connection) {
    conn.execute_batch("DROP TRIGGER abort_lineage_publication")
        .unwrap();
}

fn lifecycle_snapshot(
    conn: &Connection,
    lineage: &LineageId,
    branch: &BranchId,
) -> ([i64; 10], String) {
    let counts = [
        "objects",
        "lineage_payload_object_refs",
        "lineage_sequence_nodes",
        "lineage_sequence_entries",
        "lineage_sequence_roots",
        "lineage_history_index_nodes",
        "lineage_history_indexes",
        "lineage_revisions",
        "lineage_branch_revisions",
        "lineage_commit_receipts",
    ]
    .map(|table| {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    });
    let head = conn
        .query_row(
            "SELECT head_revision_id FROM lineage_branches
                 WHERE lineage_id = ?1 AND session_id = ?2",
            (lineage.as_str(), branch.as_str()),
            |row| row.get(0),
        )
        .unwrap();
    (counts, head)
}

fn install_branch_update_abort(conn: &Connection) {
    conn.execute_batch(
        "CREATE TEMP TRIGGER abort_lineage_publication
             AFTER UPDATE OF head_revision_id ON lineage_branches
             BEGIN SELECT RAISE(ABORT, 'abort lineage publication'); END;",
    )
    .unwrap();
}

#[test]
fn sequence_publication_rolls_back_objects_payloads_nodes_entries_and_roots() {
    let (mut conn, lineage) = setup();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    let before = publication_row_counts(&conn);

    for table in [
        "lineage_payload_object_refs",
        "lineage_sequence_nodes",
        "lineage_sequence_entries",
        "lineage_sequence_roots",
    ] {
        install_publication_abort(&conn, table);
        let result = append_sequence(
            &mut conn,
            &lineage,
            &empty,
            &[format!("unique payload for {table}").into_bytes()],
            ObjectCompression::none(),
        );
        assert!(result.is_err(), "publication unexpectedly passed {table}");
        remove_publication_abort(&conn);
        assert_eq!(publication_row_counts(&conn), before, "rollback at {table}");
    }
}

#[test]
fn split_publication_rolls_back_boundary_nodes_entries_and_roots() {
    let (mut conn, lineage) = setup();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    let items: Vec<_> = (0..64).map(bytes).collect();
    let (root, _) = append_sequence(
        &mut conn,
        &lineage,
        &empty,
        &items,
        ObjectCompression::none(),
    )
    .unwrap();
    let before = publication_row_counts(&conn);

    for table in ["lineage_sequence_entries", "lineage_sequence_roots"] {
        install_publication_abort(&conn, table);
        let result = split_sequence(&mut conn, &lineage, &root, 17);
        assert!(result.is_err(), "split unexpectedly passed {table}");
        remove_publication_abort(&conn);
        assert_eq!(publication_row_counts(&conn), before, "rollback at {table}");
    }
}

#[test]
fn persistent_sequence_reconstructs_seeks_tails_and_splits_exactly() {
    let (mut conn, lineage) = setup();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    let expected: Vec<_> = (0..2_113).map(bytes).collect();
    let (root, append_stats) = append_sequence(
        &mut conn,
        &lineage,
        &empty,
        &expected,
        ObjectCompression::none(),
    )
    .unwrap();
    assert_eq!(root.kind(), SequenceKind::History);
    assert_eq!(root.item_count(), expected.len() as u64);
    assert_eq!(
        root.byte_count(),
        expected.iter().map(|item| item.len() as u64).sum::<u64>()
    );
    assert!(root.depth() >= 3);
    assert!(append_stats.nodes_written < (expected.len() * root.depth() as usize) as u64);
    validate_sequence(&conn, &lineage, &root).unwrap();

    let (all, _) = sequence_range(&conn, &lineage, &root, 0, root.item_count()).unwrap();
    assert_eq!(all, expected);
    for index in [0, 31, 32, 1_024, 2_112] {
        let (actual, stats) = sequence_item(&conn, &lineage, &root, index).unwrap();
        assert_eq!(actual, expected[index as usize]);
        assert!(stats.nodes_read <= u64::from(root.depth()));
    }
    let (tail, tail_stats) = sequence_tail(&conn, &lineage, &root, 37).unwrap();
    assert_eq!(tail, expected[expected.len() - 37..]);
    assert!(tail_stats.nodes_read < 37 + u64::from(root.depth()));

    for split_at in [0, 1, 31, 32, 33, 1_024, 2_112, 2_113] {
        let ((left, right), stats) = split_sequence(&mut conn, &lineage, &root, split_at).unwrap();
        let (left_items, _) = sequence_range(&conn, &lineage, &left, 0, left.item_count()).unwrap();
        let (right_items, _) =
            sequence_range(&conn, &lineage, &right, 0, right.item_count()).unwrap();
        assert_eq!(left_items, expected[..split_at as usize]);
        assert_eq!(right_items, expected[split_at as usize..]);
        assert!(stats.nodes_written <= u64::from(root.depth()) * 2);
    }
}

#[test]
fn bottom_up_empty_build_matches_incremental_sequence_identity() {
    let (mut conn, lineage) = setup();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    let expected: Vec<_> = (0..1_057).map(bytes).collect();
    let (bulk, bulk_stats) = append_sequence(
        &mut conn,
        &lineage,
        &empty,
        &expected,
        ObjectCompression::none(),
    )
    .unwrap();

    let transaction = conn.transaction().unwrap();
    let mut incremental = empty;
    for item in &expected {
        incremental = append_sequence_in(
            &transaction,
            &lineage,
            &incremental,
            std::slice::from_ref(item),
            ObjectCompression::none(),
        )
        .unwrap()
        .0;
    }
    transaction.commit().unwrap();

    assert_eq!(bulk, incremental);
    assert!(bulk_stats.nodes_written < expected.len() as u64 / 16);
}

#[test]
fn empty_roots_are_kind_separated_and_append_work_is_prefix_independent() {
    let (mut conn, lineage) = setup();
    let random_lineage = LineageId::random().unwrap();
    assert_ne!(random_lineage, lineage);
    let history = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    let transcript = empty_sequence(&conn, &lineage, SequenceKind::Transcript).unwrap();
    assert_ne!(history.id(), transcript.id());

    let first: Vec<_> = (0..1_024).map(bytes).collect();
    let (short, _) = append_sequence(
        &mut conn,
        &lineage,
        &history,
        &first,
        ObjectCompression::none(),
    )
    .unwrap();
    let next = vec![b"next".to_vec()];
    let (_, short_stats) = append_sequence(
        &mut conn,
        &lineage,
        &short,
        &next,
        ObjectCompression::none(),
    )
    .unwrap();

    let rest: Vec<_> = (1_024..4_096).map(bytes).collect();
    let (long, _) = append_sequence(
        &mut conn,
        &lineage,
        &short,
        &rest,
        ObjectCompression::none(),
    )
    .unwrap();
    let (_, long_stats) =
        append_sequence(&mut conn, &lineage, &long, &next, ObjectCompression::none()).unwrap();
    assert!(short_stats.nodes_read <= u64::from(short.depth()) + 1);
    assert!(long_stats.nodes_read <= u64::from(long.depth()) + 1);
    assert!(long_stats.nodes_written <= u64::from(long.depth()) + 1);
}

#[test]
fn branches_publish_revisions_fork_in_constant_work_and_rewind_by_root() {
    let (mut conn, lineage) = setup();
    let main = branch_id('2');
    let fork = branch_id('3');
    let metadata = branch_metadata();
    let (initial, initial_receipt) =
        create_initial_branch(&mut conn, &lineage, &main, &metadata, b"initial-state", 1).unwrap();
    assert_eq!(initial.id(), &initial_receipt.result_revision_id);
    assert_eq!(branch_head(&conn, &lineage, &main).unwrap(), initial);

    let history_items: Vec<_> = (0..1_024).map(bytes).collect();
    let (history, _) = append_sequence(
        &mut conn,
        &lineage,
        initial.history_root(),
        &history_items,
        ObjectCompression::none(),
    )
    .unwrap();
    let transcript_items = vec![b"request".to_vec(), b"response".to_vec()];
    let (transcript, _) = append_sequence(
        &mut conn,
        &lineage,
        initial.transcript_root(),
        &transcript_items,
        ObjectCompression::none(),
    )
    .unwrap();
    let (committed, commit_receipt) = commit_revision(
        &mut conn,
        &lineage,
        &main,
        initial.id(),
        &history,
        &transcript,
        b"committed-state",
        LineageOperation::Append,
        2,
    )
    .unwrap();
    assert_eq!(committed.id(), &commit_receipt.result_revision_id);
    assert_eq!(branch_head(&conn, &lineage, &main).unwrap(), committed);

    let (fork_receipt, fork_stats) =
        fork_branch(&mut conn, &lineage, &main, &fork, None, 3).unwrap();
    assert_eq!(fork_receipt.result_revision_id, committed.id);
    assert_eq!(fork_stats.branch_rows_written, 1);
    assert_eq!(fork_stats.receipt_rows_written, 1);
    assert_eq!(fork_stats.sequence_rows_written, 0);
    assert_eq!(branch_head(&conn, &lineage, &fork).unwrap(), committed);
    assert_ne!(
        commit_fingerprint(
            &lineage,
            &main,
            LineageOperation::Rewind,
            Some(committed.id()),
            initial.id(),
            None,
        ),
        commit_fingerprint(
            &lineage,
            &fork,
            LineageOperation::Rewind,
            Some(committed.id()),
            initial.id(),
            None,
        )
    );

    let rewind =
        rewind_branch(&mut conn, &lineage, &main, committed.id(), initial.id(), 4).unwrap();
    assert_eq!(branch_head(&conn, &lineage, &main).unwrap(), initial);
    let retried =
        rewind_branch(&mut conn, &lineage, &main, committed.id(), initial.id(), 4).unwrap();
    assert_eq!(retried, rewind);
    assert_eq!(branch_head(&conn, &lineage, &fork).unwrap(), committed);

    delete_branch(&conn, &lineage, &main, 5).unwrap();
    let report = inspect_reachability(&conn, &lineage).unwrap();
    assert!(report.reachable_revisions.contains(committed.id().as_str()));
    assert!(report.reachable_revisions.contains(initial.id().as_str()));
    assert!(report
        .reachable_roots
        .contains(committed.history_root().id().as_str()));
    assert!(!report.reachable_nodes.is_empty());
    assert!(!report.reachable_payloads.is_empty());
    assert!(!report.reachable_objects.is_empty());

    delete_branch(&conn, &lineage, &fork, 6).unwrap();
    let report = inspect_reachability(&conn, &lineage).unwrap();
    assert!(report.reachable_revisions.contains(initial.id().as_str()));
    assert!(report.reachable_revisions.contains(committed.id().as_str()));
    assert!(report.unreachable_revisions.is_empty());
    assert!(report.unreachable_roots.is_empty());
    assert!(report.unreachable_nodes.is_empty());
    assert!(report.unreachable_payloads.is_empty());
    assert!(report.unreachable_objects.is_empty());
}

#[test]
fn sqlite_full_rolls_back_lineage_revision_publication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lineage-full.db");
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;",
    )
    .unwrap();
    crate::schema::initialize_lineage_schema(&mut conn).unwrap();
    let lineage = LineageId::from_hex("2".repeat(32)).unwrap();
    create_lineage(&conn, &lineage, 1).unwrap();
    let branch = branch_id('3');
    let (initial, _) = create_initial_branch(
        &mut conn,
        &lineage,
        &branch,
        &branch_metadata(),
        b"initial",
        1,
    )
    .unwrap();
    conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let page_count = conn
        .pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0))
        .unwrap();
    conn.pragma_update(None, "max_page_count", page_count)
        .unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "max_page_count", |row| row.get::<_, i64>(0))
            .unwrap(),
        page_count
    );
    assert_eq!(
        conn.pragma_query_value(None, "freelist_count", |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );

    let mut state = Vec::with_capacity(4 * 1024 * 1024);
    let mut seed = 0x9e3779b97f4a7c15_u64;
    while state.len() < state.capacity() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        state.push(seed as u8);
    }
    let result = append_revision(
        &mut conn,
        &lineage,
        &branch,
        initial.id(),
        &[],
        &[],
        &state,
        LineageOperation::Append,
        ObjectCompression::none(),
        2,
    );
    assert!(matches!(result, Err(StoreError::Sqlite(_))), "{result:?}");
    assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), initial);
    assert_eq!(
        conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(foreign_keys.query([]).unwrap().next().unwrap().is_none());
}

#[test]
fn append_reuses_sequence_nodes_between_bounded_reclamation_steps() {
    for item_count in [2, 33] {
        for remaining_entries in [2, 1] {
            let (mut conn, lineage) = setup();
            let source = branch_id('4');
            let (initial, _) = create_initial_branch(
                &mut conn,
                &lineage,
                &source,
                &branch_metadata(),
                b"initial",
                1,
            )
            .unwrap();
            let history = (0..item_count).map(bytes).collect::<Vec<_>>();
            let (abandoned, _, _) = append_revision(
                &mut conn,
                &lineage,
                &source,
                initial.id(),
                &history,
                &[],
                b"abandoned-state",
                LineageOperation::Append,
                ObjectCompression::none(),
                2,
            )
            .unwrap();
            let node = abandoned.history_root.node_id.as_ref().unwrap();
            rewind_branch(
                &mut conn,
                &lineage,
                &source,
                abandoned.id(),
                initial.id(),
                3,
            )
            .unwrap();
            let mut found = false;
            for _ in 0..reclamation_step_limit(&conn, &lineage) {
                let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
                let (entries, completed) = conn.query_row(
                "SELECT (SELECT count(*) FROM lineage_sequence_entries WHERE lineage_id = ?1 AND node_id = ?2),
                        (SELECT count(*) FROM lineage_completed_sequence_nodes WHERE lineage_id = ?1 AND node_id = ?2)",
                (lineage.as_str(), node.as_str()),
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            ).unwrap();
                if entries == remaining_entries && completed == 0 {
                    found = true;
                    break;
                }
                assert!(
                    !step.complete,
                    "reclamation skipped the expected intermediate state"
                );
            }
            assert!(
                found,
                "did not reach an unsealed node with {remaining_entries} entries"
            );
            let head = branch_head(&conn, &lineage, &source).unwrap();
            let (restored, _, _) = append_revision(
                &mut conn,
                &lineage,
                &source,
                head.id(),
                &history,
                &[],
                b"restored-state",
                LineageOperation::Append,
                ObjectCompression::none(),
                4,
            )
            .expect("identical history must remain appendable during bounded reclamation");
            assert_eq!(restored.history_root, abandoned.history_root);
            assert_eq!(
                sequence_range(
                    &conn,
                    &lineage,
                    &restored.history_root,
                    0,
                    item_count as u64
                )
                .unwrap()
                .0,
                history
            );
            assert_eq!(conn.query_row(
            "SELECT count(*) FROM lineage_completed_sequence_nodes WHERE lineage_id = ?1 AND node_id = ?2",
            (lineage.as_str(), node.as_str()), |row| row.get::<_, i64>(0),
        ).unwrap(), 1);
            let mut complete = false;
            for _ in 0..reclamation_step_limit(&conn, &lineage) {
                if reclaim_step(&mut conn, &lineage, 1).unwrap().complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            assert_eq!(
                sequence_range(
                    &conn,
                    &lineage,
                    &restored.history_root,
                    0,
                    item_count as u64
                )
                .unwrap()
                .0,
                history,
            );
        }
    }
}

#[test]
fn bounded_reclamation_preserves_shared_and_retained_roots() {
    let (mut conn, lineage) = setup();
    let source = branch_id('4');
    let fork = branch_id('5');
    let (initial, _) = create_initial_branch(
        &mut conn,
        &lineage,
        &source,
        &branch_metadata(),
        b"initial",
        1,
    )
    .unwrap();
    let (shared, _, _) = append_revision(
        &mut conn,
        &lineage,
        &source,
        initial.id(),
        &[history_bytes("shared-history")],
        &[b"shared-transcript".to_vec()],
        b"shared-state",
        LineageOperation::Append,
        ObjectCompression::none(),
        2,
    )
    .unwrap();
    fork_branch(&mut conn, &lineage, &source, &fork, None, 3).unwrap();
    let nested_object = put_object(
        &conn,
        b"abandoned nested metadata",
        ObjectCompression::none(),
    )
    .unwrap();
    let nested_history = serde_json::to_vec(&serde_json::json!({
        "kind": "user",
        "content": "abandoned",
        "metadata": {
            crate::history::OBJECT_REF_KEY: {
                "hash": nested_object.hash(),
                "raw_size": nested_object.raw_size(),
            }
        }
    }))
    .unwrap();
    let audit_object =
        put_object(&conn, b"retained request body", ObjectCompression::none()).unwrap();
    conn.execute("INSERT INTO request_attempts (started_at) VALUES (1)", [])
        .unwrap();
    let request_attempt_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO request_object_refs (request_attempt_id, object_hash, role)
             VALUES (?1, ?2, 'response')",
        (request_attempt_id, audit_object.hash()),
    )
    .unwrap();
    let (abandoned, _, _) = append_revision(
        &mut conn,
        &lineage,
        &source,
        shared.id(),
        &[nested_history],
        &[b"abandoned-transcript".to_vec()],
        b"abandoned-state",
        LineageOperation::Append,
        ObjectCompression::none(),
        4,
    )
    .unwrap();
    rewind_branch(&mut conn, &lineage, &source, abandoned.id(), shared.id(), 5).unwrap();
    conn.execute(
        "INSERT INTO lineage_retained_revisions (
                 lineage_id, revision_id, retention_kind, retained_at
             ) VALUES (?1, ?2, 'recovery', 6)",
        (lineage.as_str(), abandoned.id().as_str()),
    )
    .unwrap();

    let work_before = reclamation_work_units(&conn, &lineage);
    let retained = reclaim_fixture(&mut conn, &lineage);
    let work_after = reclamation_work_units(&conn, &lineage);
    assert_eq!(retained, 0);
    assert_eq!(work_before.saturating_sub(work_after), retained);
    assert_eq!(
        load_revision(&conn, &lineage, abandoned.id()).unwrap(),
        abandoned
    );

    conn.execute(
        "DELETE FROM lineage_retained_revisions
             WHERE lineage_id = ?1 AND revision_id = ?2",
        (lineage.as_str(), abandoned.id().as_str()),
    )
    .unwrap();
    let mut reclaimed_rows = 0usize;
    for _ in 0..10_000 {
        let work_before = reclamation_work_units(&conn, &lineage);
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        let work_after = reclamation_work_units(&conn, &lineage);
        assert!(step.work_rows() <= 1);
        assert_eq!(
            work_before.saturating_sub(work_after),
            reclamation_mutations(step),
            "reclamation accounting must include every cascaded profile and extent row"
        );
        reclaimed_rows = reclaimed_rows.saturating_add(reclamation_mutations(step));
        if step.complete {
            break;
        }
    }
    assert!(reclaimed_rows > 0);
    assert!(load_revision(&conn, &lineage, abandoned.id()).is_err());
    assert_eq!(branch_head(&conn, &lineage, &source).unwrap(), shared);
    assert_eq!(branch_head(&conn, &lineage, &fork).unwrap(), shared);
    assert_eq!(
        sequence_range(&conn, &lineage, shared.history_root(), 0, 1)
            .unwrap()
            .0,
        vec![history_bytes("shared-history")]
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM objects WHERE hash = ?1",
            [nested_object.hash()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM objects WHERE hash = ?1",
            [audit_object.hash()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    conn.execute(
        "DELETE FROM request_attempts WHERE id = ?1",
        [request_attempt_id],
    )
    .unwrap();
    loop {
        let step = reclaim_step(&mut conn, &lineage, 256).unwrap();
        if step.complete {
            break;
        }
    }
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM objects WHERE hash = ?1",
            [audit_object.hash()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
    let report = inspect_reachability(&conn, &lineage).unwrap();
    assert!(report.unreachable_revisions.is_empty());
    assert!(report.unreachable_roots.is_empty());
    assert!(report.unreachable_nodes.is_empty());
    assert!(report.unreachable_payloads.is_empty());
    assert!(report.unreachable_objects.is_empty());
    let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(foreign_keys.query([]).unwrap().next().unwrap().is_none());
    drop(foreign_keys);
    crate::schema::validate_lineage_schema(&conn).unwrap();
    assert_integrity(reclaim_step(&mut conn, &lineage, 0));
}

#[test]
fn bounded_reclamation_removes_receipts_and_continuation_turns_bottom_up() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('6');
    let (initial, _) = create_initial_branch(
        &mut conn,
        &lineage,
        &branch,
        &branch_metadata(),
        b"initial",
        1,
    )
    .unwrap();
    let (shared, _, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        initial.id(),
        &[history_bytes("shared-history")],
        &[],
        b"shared-state",
        LineageOperation::Append,
        ObjectCompression::none(),
        2,
    )
    .unwrap();
    let (abandoned, _, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        shared.id(),
        &[history_bytes("abandoned-history")],
        &[],
        b"abandoned-state",
        LineageOperation::Append,
        ObjectCompression::none(),
        3,
    )
    .unwrap();
    let (abandoned_leaf, _, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        abandoned.id(),
        &[history_bytes("abandoned-leaf-history")],
        &[],
        b"abandoned-leaf-state",
        LineageOperation::Append,
        ObjectCompression::none(),
        4,
    )
    .unwrap();
    rewind_branch(
        &mut conn,
        &lineage,
        &branch,
        abandoned_leaf.id(),
        shared.id(),
        5,
    )
    .unwrap();

    let abandoned_hash = "a".repeat(64);
    let continuation_hash = "b".repeat(64);
    let reachable_hash = "c".repeat(64);
    conn.execute(
        "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, 1, 0, ?3, ?4, 3, 'user', 'completed', NULL, 10, 10, 11, NULL)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            abandoned_hash,
            abandoned.id().as_str()
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, 2, 0, ?3, ?4, 4, 'continuation', 'completed', 1, 12, 12, 13, NULL)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            continuation_hash,
            abandoned_leaf.id().as_str()
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO lineage_turns (
                 lineage_id, session_id, turn_id, submitted_history_idx,
                 submitted_history_hash, submitted_revision_id, submitted_sequence,
                 turn_kind, turn_state, continuation_of, created_at_ms,
                 started_at_ms, finished_at_ms, terminal_reason
             ) VALUES (?1, ?2, 3, 0, ?3, ?4, 2, 'user', 'completed', NULL, 14, 14, 15, NULL)",
        rusqlite::params![
            lineage.as_str(),
            branch.as_str(),
            reachable_hash,
            shared.id().as_str()
        ],
    )
    .unwrap();

    let abandoned_receipt = "8".repeat(64);
    let reachable_receipt = "9".repeat(64);
    for (fingerprint, turn_id, created_at) in
        [(&abandoned_receipt, 2, 13), (&reachable_receipt, 3, 15)]
    {
        conn.execute(
            "INSERT INTO lineage_session_receipts (
                     lineage_id, session_id, fingerprint, command_kind, save_receipt_json,
                     turn_id, turn_state, turn_payload_json, created_at
                 ) VALUES (?1, ?2, ?3, 'turn_transition', '{}', ?4, 'completed', NULL, ?5)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                fingerprint,
                turn_id,
                created_at
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO lineage_turn_transitions (
                     lineage_id, session_id, fingerprint, turn_id, from_state, to_state,
                     transitioned_at_ms, terminal_reason
                 ) VALUES (?1, ?2, ?3, ?4, 'running', 'completed', ?5, NULL)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                fingerprint,
                turn_id,
                created_at
            ],
        )
        .unwrap();
    }

    let mut reclaimed_rows = 0usize;
    for _ in 0..10_000 {
        let work_before = reclamation_work_units(&conn, &lineage);
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        let work_after = reclamation_work_units(&conn, &lineage);
        assert!(step.work_rows() <= 1);
        assert_eq!(
            work_before.saturating_sub(work_after),
            reclamation_mutations(step),
            "reclamation accounting must include every cascaded profile and extent row"
        );
        reclaimed_rows = reclaimed_rows.saturating_add(reclamation_mutations(step));
        if step.complete {
            break;
        }
    }
    assert!(reclaimed_rows > 0);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM lineage_turns
                 WHERE lineage_id = ?1 AND session_id = ?2 AND turn_id IN (1, 2)",
            (lineage.as_str(), branch.as_str()),
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM lineage_turns
                 WHERE lineage_id = ?1 AND session_id = ?2 AND turn_id = 3",
            (lineage.as_str(), branch.as_str()),
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM lineage_session_receipts
                 WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
            (
                lineage.as_str(),
                branch.as_str(),
                abandoned_receipt.as_str(),
            ),
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM lineage_session_receipts
                 WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
            (
                lineage.as_str(),
                branch.as_str(),
                reachable_receipt.as_str(),
            ),
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM lineage_commit_receipts
                 WHERE lineage_id = ?1
                   AND (prior_revision_id IN (?2, ?3) OR result_revision_id IN (?2, ?3))",
            (
                lineage.as_str(),
                abandoned.id().as_str(),
                abandoned_leaf.id().as_str(),
            ),
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
    assert!(load_revision(&conn, &lineage, abandoned.id()).is_err());
    assert!(load_revision(&conn, &lineage, abandoned_leaf.id()).is_err());
    assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), shared);
    let mut foreign_keys = conn.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(foreign_keys.query([]).unwrap().next().unwrap().is_none());
}

#[test]
fn append_and_split_lifecycles_are_atomic_idempotent_and_exact() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('7');
    let (initial, create_receipt) = create_initial_branch(
        &mut conn,
        &lineage,
        &branch,
        &branch_metadata(),
        b"initial",
        1,
    )
    .unwrap();
    assert_eq!(create_receipt.operation, LineageOperation::Create);
    assert_eq!(create_receipt.prior_revision_id, None);
    assert_eq!(create_receipt.coordinates, ReceiptCoordinates::default());
    assert!(conn
        .execute(
            "UPDATE lineage_commit_receipts SET created_at = created_at
                 WHERE lineage_id = ?1 AND session_id = ?2",
            (lineage.as_str(), branch.as_str()),
        )
        .is_err());
    assert!(conn
        .execute(
            "DELETE FROM lineage_commit_receipts
                 WHERE lineage_id = ?1 AND session_id = ?2",
            (lineage.as_str(), branch.as_str()),
        )
        .is_err());
    assert!(conn
        .execute(
            "UPDATE lineage_branches SET initial_revision_id = initial_revision_id
                 WHERE lineage_id = ?1 AND session_id = ?2",
            (lineage.as_str(), branch.as_str()),
        )
        .is_err());

    let history = vec![
        history_bytes("h0"),
        history_bytes("h1"),
        history_bytes("h2"),
    ];
    let transcript = vec![b"t0".to_vec(), b"t1".to_vec()];
    let (appended, append_receipt, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        initial.id(),
        &history,
        &transcript,
        b"appended",
        LineageOperation::Append,
        ObjectCompression::none(),
        2,
    )
    .unwrap();
    assert_eq!(
        append_receipt.coordinates,
        ReceiptCoordinates {
            history_start_idx: Some(0),
            history_item_count: Some(3),
            transcript_start_idx: Some(0),
            transcript_record_count: Some(2),
        }
    );
    let after_append = lifecycle_snapshot(&conn, &lineage, &branch);
    let (retried, retried_receipt, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        initial.id(),
        &history,
        &transcript,
        b"appended",
        LineageOperation::Append,
        ObjectCompression::none(),
        2,
    )
    .unwrap();
    assert_eq!(retried, appended);
    assert_eq!(retried_receipt, append_receipt);
    assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), after_append);

    assert_integrity(append_revision(
        &mut conn,
        &lineage,
        &branch,
        initial.id(),
        &[history_bytes("stale-unique-history")],
        &[b"stale-unique-transcript".to_vec()],
        b"stale-unique-state",
        LineageOperation::Append,
        ObjectCompression::none(),
        3,
    ));
    assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), after_append);

    let (split, split_receipt, _) = split_revision(
        &mut conn,
        &lineage,
        &branch,
        appended.id(),
        2,
        1,
        b"split",
        LineageOperation::Split,
        4,
    )
    .unwrap();
    assert_eq!(split_receipt.coordinates, ReceiptCoordinates::default());
    assert_eq!(
        sequence_range(&conn, &lineage, split.history_root(), 0, 2)
            .unwrap()
            .0,
        history[..2]
    );
    assert_eq!(
        sequence_range(&conn, &lineage, split.transcript_root(), 0, 1)
            .unwrap()
            .0,
        transcript[..1]
    );
    let after_split = lifecycle_snapshot(&conn, &lineage, &branch);
    let (retried, retried_receipt, _) = split_revision(
        &mut conn,
        &lineage,
        &branch,
        appended.id(),
        2,
        1,
        b"split",
        LineageOperation::Split,
        4,
    )
    .unwrap();
    assert_eq!(retried, split);
    assert_eq!(retried_receipt, split_receipt);
    assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), after_split);
    assert_integrity(split_revision(
        &mut conn,
        &lineage,
        &branch,
        split.id(),
        0,
        0,
        b"invalid-operation",
        LineageOperation::Append,
        5,
    ));
    assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), after_split);
}

#[test]
fn lifecycle_publication_rolls_back_at_every_canonical_boundary() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('8');
    let (initial, _) = create_initial_branch(
        &mut conn,
        &lineage,
        &branch,
        &branch_metadata(),
        b"initial",
        1,
    )
    .unwrap();
    let before_append = lifecycle_snapshot(&conn, &lineage, &branch);
    for table in [
        "objects",
        "lineage_payload_object_refs",
        "lineage_sequence_nodes",
        "lineage_sequence_entries",
        "lineage_sequence_roots",
        "lineage_history_index_nodes",
        "lineage_history_indexes",
        "lineage_revisions",
        "lineage_branch_revisions",
        "lineage_branches",
        "lineage_commit_receipts",
    ] {
        if table == "lineage_branches" {
            install_branch_update_abort(&conn);
        } else {
            install_publication_abort(&conn, table);
        }
        let result = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &[history_bytes(format!("history-{table}"))],
            &[format!("transcript-{table}").into_bytes()],
            format!("state-{table}").as_bytes(),
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        );
        assert!(result.is_err(), "append unexpectedly passed {table}");
        remove_publication_abort(&conn);
        assert_eq!(
            lifecycle_snapshot(&conn, &lineage, &branch),
            before_append,
            "append rollback at {table}"
        );
    }

    let history: Vec<_> = (0..40).map(bytes).collect();
    let transcript: Vec<_> = (40..80).map(bytes).collect();
    let (appended, _, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        initial.id(),
        &history,
        &transcript,
        b"successful-append",
        LineageOperation::Append,
        ObjectCompression::none(),
        3,
    )
    .unwrap();
    let before_split = lifecycle_snapshot(&conn, &lineage, &branch);
    for table in [
        "objects",
        "lineage_payload_object_refs",
        "lineage_sequence_nodes",
        "lineage_sequence_entries",
        "lineage_sequence_roots",
        "lineage_history_index_nodes",
        "lineage_history_indexes",
        "lineage_revisions",
        "lineage_branch_revisions",
        "lineage_branches",
        "lineage_commit_receipts",
    ] {
        if table == "lineage_branches" {
            install_branch_update_abort(&conn);
        } else {
            install_publication_abort(&conn, table);
        }
        let result = split_revision(
            &mut conn,
            &lineage,
            &branch,
            appended.id(),
            17,
            19,
            format!("split-state-{table}").as_bytes(),
            LineageOperation::Rewind,
            4,
        );
        assert!(result.is_err(), "split unexpectedly passed {table}");
        remove_publication_abort(&conn);
        assert_eq!(
            lifecycle_snapshot(&conn, &lineage, &branch),
            before_split,
            "split rollback at {table}"
        );
    }
}

#[test]
fn lineage_publication_is_crash_atomic_at_canonical_boundaries() {
    if let (Ok(role), Ok(path)) = (
        std::env::var(LINEAGE_CRASH_ROLE),
        std::env::var(LINEAGE_CRASH_DB),
    ) {
        let mut conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        conn.create_scalar_function(
            "smelt_test_crash",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_| -> rusqlite::Result<i64> { std::process::abort() },
        )
        .unwrap();
        let trigger = match role.as_str() {
            "node" => {
                "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER INSERT ON lineage_sequence_nodes
                     BEGIN SELECT smelt_test_crash(); END;"
            }
            "semantic_node" => {
                "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER INSERT ON lineage_history_index_nodes
                     BEGIN SELECT smelt_test_crash(); END;"
            }
            "semantic_root" => {
                "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER INSERT ON lineage_history_indexes
                     BEGIN SELECT smelt_test_crash(); END;"
            }
            "revision" => {
                "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER INSERT ON lineage_revisions
                     BEGIN SELECT smelt_test_crash(); END;"
            }
            "head" => {
                "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER UPDATE OF head_revision_id ON lineage_branches
                     BEGIN SELECT smelt_test_crash(); END;"
            }
            "receipt" => {
                "CREATE TEMP TRIGGER crash_lineage_publication
                     AFTER INSERT ON lineage_commit_receipts
                     BEGIN SELECT smelt_test_crash(); END;"
            }
            other => panic!("unknown lineage crash boundary {other}"),
        };
        conn.execute_batch(trigger).unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        let branch = branch_id('d');
        let initial = branch_head(&conn, &lineage, &branch).unwrap();
        let result = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &[history_bytes(format!("crash-history-{role}"))],
            &[format!("crash-transcript-{role}").into_bytes()],
            format!("crash-state-{role}").as_bytes(),
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        );
        panic!("lineage crash trigger did not abort: {result:?}");
    }

    let dir = tempfile::tempdir().unwrap();
    for role in [
        "node",
        "semantic_node",
        "semantic_root",
        "revision",
        "head",
        "receipt",
    ] {
        let path = dir.path().join(format!("lineage-{role}.db"));
        let (lineage, branch, initial_id, before) = {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = FULL;
                     PRAGMA foreign_keys = ON;",
            )
            .unwrap();
            crate::schema::initialize_lineage_schema(&mut conn).unwrap();
            let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
            let branch = branch_id('d');
            create_lineage(&conn, &lineage, 1).unwrap();
            let (initial, _) = create_initial_branch(
                &mut conn,
                &lineage,
                &branch,
                &branch_metadata(),
                b"initial",
                1,
            )
            .unwrap();
            let before = lifecycle_snapshot(&conn, &lineage, &branch);
            (lineage, branch, initial.id().clone(), before)
        };

        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("lineage::tests::lineage_publication_is_crash_atomic_at_canonical_boundaries")
            .arg("--nocapture")
            .env(LINEAGE_CRASH_ROLE, role)
            .env(LINEAGE_CRASH_DB, &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "child did not crash at {role}");

        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        assert_eq!(
            conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        let mut foreign_key_check = conn.prepare("PRAGMA foreign_key_check").unwrap();
        assert!(foreign_key_check
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none());
        drop(foreign_key_check);
        crate::schema::validate_lineage_schema(&conn).unwrap();
        assert_eq!(
            lifecycle_snapshot(&conn, &lineage, &branch),
            before,
            "partial publication survived crash at {role}"
        );

        let (revision, receipt, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            &initial_id,
            &[history_bytes(format!("crash-history-{role}"))],
            &[format!("crash-transcript-{role}").into_bytes()],
            format!("crash-state-{role}").as_bytes(),
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        )
        .unwrap();
        assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), revision);
        assert_eq!(
            load_receipt(&conn, &lineage, &branch, &receipt.fingerprint).unwrap(),
            Some(receipt)
        );
    }
}

#[test]
fn reclamation_crash_restores_guards_and_resumes_from_a_valid_state() {
    if let Ok(path) = std::env::var(RECLAMATION_CRASH_DB) {
        let mut conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        conn.create_scalar_function(
            "smelt_test_crash",
            0,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |_| -> rusqlite::Result<i64> { std::process::abort() },
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER crash_lineage_reclamation
                 AFTER DELETE ON lineage_commit_receipts
                 BEGIN SELECT smelt_test_crash(); END;",
        )
        .unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        loop {
            let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
            assert!(
                !step.complete,
                "reclamation completed before crash boundary"
            );
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lineage-reclamation.db");
    let (lineage, branch, shared_id, abandoned_id) = {
        let mut conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        crate::schema::initialize_lineage_schema(&mut conn).unwrap();
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        let branch = branch_id('e');
        create_lineage(&conn, &lineage, 1).unwrap();
        let (initial, _) = create_initial_branch(
            &mut conn,
            &lineage,
            &branch,
            &branch_metadata(),
            b"initial",
            1,
        )
        .unwrap();
        let (shared, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            initial.id(),
            &[history_bytes("shared")],
            &[history_bytes("shared")],
            &history_bytes("shared"),
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        )
        .unwrap();
        let (abandoned, _, _) = append_revision(
            &mut conn,
            &lineage,
            &branch,
            shared.id(),
            &[history_bytes("abandoned")],
            &[history_bytes("abandoned")],
            &history_bytes("abandoned"),
            LineageOperation::Append,
            ObjectCompression::none(),
            3,
        )
        .unwrap();
        rewind_branch(&mut conn, &lineage, &branch, abandoned.id(), shared.id(), 4).unwrap();
        (lineage, branch, shared.id().clone(), abandoned.id().clone())
    };

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("lineage::tests::reclamation_crash_restores_guards_and_resumes_from_a_valid_state")
        .arg("--nocapture")
        .env(RECLAMATION_CRASH_DB, &path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "child did not crash during reclamation");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(libc::SIGABRT),
            "reclamation child did not abort at deletion"
        );
    }

    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    assert_eq!(
        conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let mut foreign_key_check = conn.prepare("PRAGMA foreign_key_check").unwrap();
    assert!(foreign_key_check
        .query([])
        .unwrap()
        .next()
        .unwrap()
        .is_none());
    drop(foreign_key_check);
    crate::schema::validate_lineage_schema(&conn).unwrap();
    assert_eq!(
        branch_head(&conn, &lineage, &branch).unwrap().id(),
        &shared_id
    );
    assert_eq!(
        load_revision(&conn, &lineage, &abandoned_id).unwrap().id(),
        &abandoned_id
    );

    for _ in 0..10_000 {
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        if step.complete {
            break;
        }
    }
    assert!(load_revision(&conn, &lineage, &abandoned_id).is_err());
    crate::schema::validate_lineage_schema(&conn).unwrap();
}

#[test]
fn reclamation_guard_restore_error_rolls_back_deletion_and_restarts_the_pass() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    for foreign_keys in [false, true] {
        for budget in [1, 7] {
            let (mut conn, lineage) = setup();
            conn.pragma_update(None, "foreign_keys", foreign_keys)
                .unwrap();
            let branch = branch_id('f');
            let (initial, _) = create_initial_branch(
                &mut conn,
                &lineage,
                &branch,
                &branch_metadata(),
                b"initial",
                1,
            )
            .unwrap();
            let (abandoned, _, _) = append_revision(
                &mut conn,
                &lineage,
                &branch,
                initial.id(),
                &[history_bytes("abandoned")],
                &[],
                b"abandoned",
                LineageOperation::Append,
                ObjectCompression::none(),
                2,
            )
            .unwrap();
            rewind_branch(
                &mut conn,
                &lineage,
                &branch,
                abandoned.id(),
                initial.id(),
                3,
            )
            .unwrap();
            let before = lifecycle_snapshot(&conn, &lineage, &branch);
            conn.authorizer(Some(|context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::CreateTrigger {
                        trigger_name: "lineage_commit_receipt_delete",
                        ..
                    }
                ) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))
            .unwrap();
            let mut rejected = false;
            for _ in 0..reclamation_step_limit(&conn, &lineage) {
                match reclaim_step(&mut conn, &lineage, budget) {
                    Ok(step) => {
                        assert!(!step.complete);
                        assert!(step.work_rows() <= budget);
                    }
                    Err(_) => {
                        rejected = true;
                        break;
                    }
                }
            }
            conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            assert!(rejected, "guard restore boundary was not reached");
            assert!(conn.is_autocommit());
            assert_eq!(lifecycle_snapshot(&conn, &lineage, &branch), before);
            crate::schema::validate_lineage_schema(&conn).unwrap();
            assert_eq!(
                conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
                    .unwrap(),
                foreign_keys
            );
            let epoch: i64 = conn
                .query_row("SELECT epoch FROM smelt_gc_pass", [], |row| row.get(0))
                .unwrap();
            let step = reclaim_step(&mut conn, &lineage, budget).unwrap();
            assert_eq!(
                step.canonical_rows_deleted + step.objects_deleted + step.branch_heads_cleared,
                0
            );
            assert!(step.work_rows() <= budget);
            assert_eq!(
                conn.query_row("SELECT epoch FROM smelt_gc_pass", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                epoch + 1
            );
            assert!(reclaim_fixture(&mut conn, &lineage) > 0);
            assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), initial);
            assert!(load_revision(&conn, &lineage, abandoned.id()).is_err());
            crate::schema::validate_lineage_schema(&conn).unwrap();
        }
    }
}

#[test]
fn reclamation_restores_exact_installed_delete_guards() {
    let (mut conn, lineage) = setup();
    let name = "lineage_history_index_delete";
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap();
    let sql = sql.replacen("BEGIN", "BEGIN SELECT 1;", 1);
    conn.execute_batch(&format!("DROP TRIGGER {name}; {sql}"))
        .unwrap();
    let definitions = |conn: &Connection| {
        conn.prepare("SELECT name, sql FROM sqlite_schema WHERE type = 'trigger' ORDER BY name")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = definitions(&conn);
    let history = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    ensure_history_index(&conn, &lineage, &history, None).unwrap();
    crate::object::put_object(&conn, b"unreferenced", ObjectCompression::none()).unwrap();
    let mut objects_deleted = 0;
    let mut complete = false;
    for _ in 0..reclamation_step_limit(&conn, &lineage) {
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        objects_deleted += step.objects_deleted;
        assert_eq!(definitions(&conn), before);
        if step.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(objects_deleted, 1);
    assert_eq!(reclaim_fixture(&mut conn, &lineage), 0);
    assert_eq!(definitions(&conn), before);
}

#[test]
fn direct_and_derived_rewind_receipts_survive_later_head_movement() {
    let (mut conn, lineage) = setup();
    let branch = branch_id('9');
    let unrelated_branch = branch_id('a');
    let rejected_fork = branch_id('b');
    let (initial, _) = create_initial_branch(
        &mut conn,
        &lineage,
        &branch,
        &branch_metadata(),
        b"initial",
        1,
    )
    .unwrap();
    let (first, _, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        initial.id(),
        &[history_bytes("h0"), history_bytes("h1")],
        &[b"t0".to_vec(), b"t1".to_vec()],
        b"first",
        LineageOperation::Append,
        ObjectCompression::none(),
        2,
    )
    .unwrap();
    let (second, _, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        first.id(),
        &[history_bytes("h2")],
        &[b"t2".to_vec()],
        b"second",
        LineageOperation::Append,
        ObjectCompression::none(),
        3,
    )
    .unwrap();

    let direct = rewind_branch(&mut conn, &lineage, &branch, second.id(), initial.id(), 4).unwrap();
    let (moved, _, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        initial.id(),
        &[history_bytes("new-history")],
        &[b"new-transcript".to_vec()],
        b"moved",
        LineageOperation::Append,
        ObjectCompression::none(),
        5,
    )
    .unwrap();
    assert_eq!(
        rewind_branch(&mut conn, &lineage, &branch, second.id(), initial.id(), 4,).unwrap(),
        direct
    );
    assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), moved);
    assert_eq!(
        load_receipt(&conn, &lineage, &branch, &direct.fingerprint).unwrap(),
        Some(direct)
    );

    let (unrelated, _) = create_initial_branch(
        &mut conn,
        &lineage,
        &unrelated_branch,
        &branch_metadata(),
        b"unrelated",
        6,
    )
    .unwrap();
    assert_integrity(rewind_branch(
        &mut conn,
        &lineage,
        &branch,
        moved.id(),
        unrelated.id(),
        7,
    ));
    assert_integrity(fork_branch(
        &mut conn,
        &lineage,
        &branch,
        &rejected_fork,
        Some(unrelated.id()),
        7,
    ));

    let (derived, derived_receipt, _) = split_revision(
        &mut conn,
        &lineage,
        &branch,
        moved.id(),
        0,
        0,
        b"derived-rewind",
        LineageOperation::Rewind,
        8,
    )
    .unwrap();
    let (later, _, _) = append_revision(
        &mut conn,
        &lineage,
        &branch,
        derived.id(),
        &[history_bytes("later-history")],
        &[b"later-transcript".to_vec()],
        b"later",
        LineageOperation::Append,
        ObjectCompression::none(),
        9,
    )
    .unwrap();
    let (retried, retried_receipt, _) = split_revision(
        &mut conn,
        &lineage,
        &branch,
        moved.id(),
        0,
        0,
        b"derived-rewind",
        LineageOperation::Rewind,
        8,
    )
    .unwrap();
    assert_eq!(retried, derived);
    assert_eq!(retried_receipt, derived_receipt);
    assert_eq!(branch_head(&conn, &lineage, &branch).unwrap(), later);
    assert_eq!(
        load_receipt(&conn, &lineage, &branch, &derived_receipt.fingerprint).unwrap(),
        Some(derived_receipt.clone())
    );

    assert!(conn
        .execute(
            "INSERT INTO lineage_commit_receipts (
                     lineage_id, session_id, fingerprint, operation_kind,
                     prior_revision_id, result_revision_id,
                     history_start_idx, history_item_count,
                     transcript_start_idx, transcript_record_count,
                     turn_id, created_at
                 ) VALUES (?1, ?2, ?3, 'append', NULL, ?4, 0, 0, 0, 0, NULL, 10)",
            rusqlite::params![
                lineage.as_str(),
                branch.as_str(),
                "c".repeat(64),
                later.id().as_str()
            ],
        )
        .is_err());

    conn.execute_batch(
        "DROP TRIGGER lineage_commit_receipt_update;
             PRAGMA ignore_check_constraints = ON;",
    )
    .unwrap();
    conn.execute(
        "UPDATE lineage_commit_receipts SET history_start_idx = 0
             WHERE lineage_id = ?1 AND session_id = ?2 AND fingerprint = ?3",
        (
            lineage.as_str(),
            branch.as_str(),
            derived_receipt.fingerprint.as_str(),
        ),
    )
    .unwrap();
    assert_integrity(
        load_receipt(&conn, &lineage, &branch, &derived_receipt.fingerprint).map(|_| ()),
    );
}

#[test]
fn fork_receipt_survives_source_rewind_deletion_and_target_head_movement() {
    let (mut conn, lineage) = setup();
    let source = branch_id('4');
    let target = branch_id('5');
    let conflicting_target = branch_id('6');
    let (initial, _) = create_initial_branch(
        &mut conn,
        &lineage,
        &source,
        &branch_metadata(),
        b"initial",
        1,
    )
    .unwrap();
    let (first, first_receipt, _) = append_revision(
        &mut conn,
        &lineage,
        &source,
        initial.id(),
        &[history_bytes("history-1")],
        &[b"transcript-1".to_vec()],
        b"first",
        LineageOperation::Append,
        ObjectCompression::none(),
        2,
    )
    .unwrap();
    assert_eq!(
        first_receipt.coordinates,
        ReceiptCoordinates {
            history_start_idx: Some(0),
            history_item_count: Some(1),
            transcript_start_idx: Some(0),
            transcript_record_count: Some(1),
        }
    );
    let (second, _, _) = append_revision(
        &mut conn,
        &lineage,
        &source,
        first.id(),
        &[history_bytes("history-2")],
        &[b"transcript-2".to_vec()],
        b"second",
        LineageOperation::Append,
        ObjectCompression::none(),
        3,
    )
    .unwrap();
    let (fork_receipt, _) =
        fork_branch(&mut conn, &lineage, &source, &target, Some(first.id()), 4).unwrap();

    rewind_branch(&mut conn, &lineage, &source, second.id(), initial.id(), 5).unwrap();
    let (target_head, _, _) = append_revision(
        &mut conn,
        &lineage,
        &target,
        first.id(),
        &[history_bytes("fork-history")],
        &[b"fork-transcript".to_vec()],
        b"fork-head",
        LineageOperation::Append,
        ObjectCompression::none(),
        6,
    )
    .unwrap();
    assert_ne!(target_head.id(), &fork_receipt.result_revision_id);

    let (retried, stats) =
        fork_branch(&mut conn, &lineage, &source, &target, Some(first.id()), 4).unwrap();
    assert_eq!(retried, fork_receipt);
    assert_eq!(stats, ForkStats::default());
    assert_eq!(
        load_receipt(&conn, &lineage, &target, &fork_receipt.fingerprint).unwrap(),
        Some(fork_receipt.clone())
    );

    delete_branch(&conn, &lineage, &source, 7).unwrap();
    let (retried, stats) = fork_branch(&mut conn, &lineage, &source, &target, None, 4).unwrap();
    assert_eq!(retried, fork_receipt);
    assert_eq!(stats, ForkStats::default());
    assert_eq!(
        load_receipt(&conn, &lineage, &target, &fork_receipt.fingerprint).unwrap(),
        Some(fork_receipt.clone())
    );
    assert_integrity(fork_branch(
        &mut conn,
        &lineage,
        &source,
        &conflicting_target,
        None,
        8,
    ));

    conn.execute_batch("DROP TRIGGER lineage_branch_identity_update")
        .unwrap();
    conn.execute(
        "UPDATE lineage_branches SET initial_revision_id = ?1
             WHERE lineage_id = ?2 AND session_id = ?3",
        (initial.id().as_str(), lineage.as_str(), target.as_str()),
    )
    .unwrap();
    assert_integrity(load_receipt(&conn, &lineage, &target, &fork_receipt.fingerprint).map(|_| ()));
}

#[test]
fn randomized_branch_lifecycle_matches_flat_multi_branch_model() {
    #[derive(Clone)]
    struct ModelRevision {
        parent: Option<String>,
        history: Vec<Vec<u8>>,
        transcript: Vec<Vec<u8>>,
    }

    #[derive(Clone)]
    struct ModelBranch {
        id: BranchId,
        head: String,
        live: bool,
    }

    let (mut conn, lineage) = setup();
    let main = BranchId::new(format!("{:064x}", 100)).unwrap();
    let (initial, _) = create_initial_branch(
        &mut conn,
        &lineage,
        &main,
        &branch_metadata(),
        b"initial",
        1,
    )
    .unwrap();
    let mut revisions = HashMap::from([(
        initial.id().as_str().to_owned(),
        ModelRevision {
            parent: None,
            history: Vec::new(),
            transcript: Vec::new(),
        },
    )]);
    let mut branches = vec![ModelBranch {
        id: main,
        head: initial.id().as_str().to_owned(),
        live: true,
    }];
    let mut seed = 0x6a09e667f3bcc909_u64;
    let mut next_branch = 101_u64;

    for round in 0..64_u64 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let live_indices: Vec<_> = branches
            .iter()
            .enumerate()
            .filter_map(|(index, branch)| branch.live.then_some(index))
            .collect();
        let selected = live_indices[usize::try_from(seed).unwrap() % live_indices.len()];

        match round % 5 {
            0..=2 => {
                let branch = branches[selected].clone();
                let prior = revisions[&branch.head].clone();
                let history_items = vec![
                    history_bytes(format!("history-{round}-0")),
                    history_bytes(format!("history-{round}-1")),
                ];
                let transcript_items = vec![format!("transcript-{round}").into_bytes()];
                let operation = LineageOperation::Append;
                let expected_id = RevisionId::from_db(branch.head.clone()).unwrap();
                let (revision, receipt, stats) = append_revision(
                    &mut conn,
                    &lineage,
                    &branch.id,
                    &expected_id,
                    &history_items,
                    &transcript_items,
                    format!("state-{round}").as_bytes(),
                    operation,
                    ObjectCompression::none(),
                    round + 2,
                )
                .unwrap();
                assert_eq!(receipt.operation, operation);
                assert_eq!(receipt.coordinates.history_item_count, Some(2));
                assert_eq!(receipt.coordinates.transcript_record_count, Some(1));
                assert!(stats.nodes_written <= 6);
                let mut history = prior.history;
                history.extend(history_items);
                let mut transcript = prior.transcript;
                transcript.extend(transcript_items);
                revisions.insert(
                    revision.id().as_str().to_owned(),
                    ModelRevision {
                        parent: Some(branch.head),
                        history,
                        transcript,
                    },
                );
                branches[selected].head = revision.id().as_str().to_owned();
            }
            3 if branches.len() < 12 => {
                let source = branches[selected].clone();
                let mut captured = source.head.clone();
                for _ in 0..(seed % 3) {
                    let Some(parent) = revisions[&captured].parent.clone() else {
                        break;
                    };
                    captured = parent;
                }
                let target = BranchId::new(format!("{next_branch:064x}")).unwrap();
                next_branch += 1;
                let captured_id = RevisionId::from_db(captured.clone()).unwrap();
                let (receipt, stats) = fork_branch(
                    &mut conn,
                    &lineage,
                    &source.id,
                    &target,
                    Some(&captured_id),
                    round + 2,
                )
                .unwrap();
                assert_eq!(receipt.result_revision_id, captured_id);
                assert_eq!(stats.sequence_rows_written, 0);
                branches.push(ModelBranch {
                    id: target,
                    head: captured,
                    live: true,
                });
            }
            _ => {
                let branch = branches[selected].clone();
                if let Some(parent) = revisions[&branch.head].parent.clone() {
                    let expected = RevisionId::from_db(branch.head).unwrap();
                    let target = RevisionId::from_db(parent.clone()).unwrap();
                    rewind_branch(
                        &mut conn,
                        &lineage,
                        &branch.id,
                        &expected,
                        &target,
                        round + 2,
                    )
                    .unwrap();
                    branches[selected].head = parent;
                }
            }
        }

        if round % 13 == 12 {
            let live_indices: Vec<_> = branches
                .iter()
                .enumerate()
                .filter_map(|(index, branch)| branch.live.then_some(index))
                .collect();
            if live_indices.len() > 2 {
                let deleted = *live_indices.last().unwrap();
                delete_branch(&conn, &lineage, &branches[deleted].id, round + 3).unwrap();
                branches[deleted].live = false;
            }
        }

        for branch in branches.iter().filter(|branch| branch.live) {
            let record = branch_head(&conn, &lineage, &branch.id).unwrap();
            assert_eq!(record.id().as_str(), branch.head);
            let model = &revisions[&branch.head];
            assert_eq!(
                sequence_range(
                    &conn,
                    &lineage,
                    record.history_root(),
                    0,
                    record.history_root().item_count(),
                )
                .unwrap()
                .0,
                model.history
            );
            assert_eq!(
                sequence_range(
                    &conn,
                    &lineage,
                    record.transcript_root(),
                    0,
                    record.transcript_root().item_count(),
                )
                .unwrap()
                .0,
                model.transcript
            );
        }
    }

    let retained = revisions.keys().next().unwrap().clone();
    conn.execute(
        "INSERT INTO lineage_retained_revisions (
                 lineage_id, revision_id, retention_kind, retained_at
             ) VALUES (?1, ?2, 'recovery', 1000)",
        (lineage.as_str(), retained.as_str()),
    )
    .unwrap();
    for branch in branches.iter_mut().filter(|branch| branch.live) {
        delete_branch(&conn, &lineage, &branch.id, 1001).unwrap();
        branch.live = false;
    }
    let report = inspect_reachability(&conn, &lineage).unwrap();
    assert!(report.reachable_revisions.contains(&retained));
    conn.execute(
        "DELETE FROM lineage_retained_revisions
             WHERE lineage_id = ?1 AND revision_id = ?2",
        (lineage.as_str(), retained.as_str()),
    )
    .unwrap();
    let report = inspect_reachability(&conn, &lineage).unwrap();
    let initial_revisions = query_strings(
        &conn,
        "SELECT initial_revision_id FROM lineage_branches WHERE lineage_id = ?1",
        &lineage,
    )
    .unwrap();
    assert!(initial_revisions.is_subset(&report.reachable_revisions));
    assert_eq!(
        report.reachable_revisions.len() + report.unreachable_revisions.len(),
        revisions.len()
    );
}

#[test]
fn randomized_sequences_match_flat_vectors_and_preserve_shared_nodes() {
    let (mut conn, lineage) = setup();
    let empty = empty_sequence(&conn, &lineage, SequenceKind::History).unwrap();
    let mut seed = 0x4d595df4d0f33173_u64;
    let mut flat = Vec::new();
    let mut root = empty;
    for round in 0..96_u64 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let append_count = usize::try_from(seed % 19 + 1).unwrap();
        let appended: Vec<_> = (0..append_count)
            .map(|offset| format!("{round}:{offset}:{seed}").into_bytes())
            .collect();
        let (next, append_stats) = append_sequence(
            &mut conn,
            &lineage,
            &root,
            &appended,
            ObjectCompression::none(),
        )
        .unwrap();
        assert!(append_stats.nodes_read <= appended.len() as u64 * (u64::from(root.depth()) + 1));
        flat.extend(appended);
        let split_at = seed % (next.item_count() + 1);
        let ((left, right), split_stats) =
            split_sequence(&mut conn, &lineage, &next, split_at).unwrap();
        let (left_items, _) = sequence_range(&conn, &lineage, &left, 0, left.item_count()).unwrap();
        let (right_items, _) =
            sequence_range(&conn, &lineage, &right, 0, right.item_count()).unwrap();
        assert_eq!(left_items, flat[..usize::try_from(split_at).unwrap()]);
        assert_eq!(right_items, flat[usize::try_from(split_at).unwrap()..]);
        assert!(split_stats.nodes_read <= u64::from(next.depth()) + 1);
        assert!(split_stats.nodes_written <= 2 * (u64::from(next.depth()) + 1) + 2);
        let (rejoined, _) = append_sequence(
            &mut conn,
            &lineage,
            &left,
            &right_items,
            ObjectCompression::none(),
        )
        .unwrap();
        let (actual, _) =
            sequence_range(&conn, &lineage, &rejoined, 0, rejoined.item_count()).unwrap();
        assert_eq!(actual, flat);
        root = next;
    }

    let split_at = root.item_count() / 2;
    let ((left, right), _) = split_sequence(&mut conn, &lineage, &root, split_at).unwrap();
    let root_nodes = reachable_node_ids(&conn, &lineage, &root);
    let left_nodes = reachable_node_ids(&conn, &lineage, &left);
    let right_nodes = reachable_node_ids(&conn, &lineage, &right);
    assert!(!root_nodes.is_disjoint(&left_nodes));
    assert!(!root_nodes.is_disjoint(&right_nodes));
}

fn reachable_node_ids(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
) -> BTreeSet<String> {
    let Some(root_node) = root.node_id.clone() else {
        return BTreeSet::new();
    };
    let mut pending = vec![root_node];
    let mut result = BTreeSet::new();
    while let Some(node_id) = pending.pop() {
        if !result.insert(node_id.as_str().to_owned()) {
            continue;
        }
        let node = load_node_shallow(conn, lineage, &node_id, None).unwrap();
        for entry in node.entries {
            if let EntryTarget::Child(child_id) = entry.target {
                pending.push(child_id);
            }
        }
    }
    result
}

#[test]
fn exact_validation_rejects_corrupt_node_and_payload_rows() {
    let (mut conn, lineage) = setup();
    let root = empty_sequence(&conn, &lineage, SequenceKind::Transcript).unwrap();
    let (root, _) = append_sequence(
        &mut conn,
        &lineage,
        &root,
        &[b"canonical payload".to_vec()],
        ObjectCompression::none(),
    )
    .unwrap();
    conn.execute_batch(
        "DROP TRIGGER lineage_sequence_node_update;
             UPDATE lineage_sequence_nodes SET byte_count = byte_count + 1;",
    )
    .unwrap();
    assert!(matches!(
        validate_sequence(&conn, &lineage, &root),
        Err(StoreError::Integrity(_))
    ));
}
