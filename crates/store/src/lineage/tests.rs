use super::*;

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

fn bytes(index: usize) -> Vec<u8> {
    format!("item-{index}-{}", "x".repeat(index % 17)).into_bytes()
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
    conn.execute_batch("DROP TRIGGER lineage_sequence_entry_insert")
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
) -> ([i64; 8], String) {
    let counts = [
        "objects",
        "lineage_payload_object_refs",
        "lineage_sequence_nodes",
        "lineage_sequence_entries",
        "lineage_sequence_roots",
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
        &[b"shared-history".to_vec()],
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

    let retained = reclaim_step(&mut conn, &lineage, 1).unwrap();
    assert!(retained.complete);
    assert_eq!(retained.work_rows(), 0);
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
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        reclaimed_rows = reclaimed_rows.saturating_add(step.work_rows());
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
        vec![b"shared-history".to_vec()]
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
        &[b"shared-history".to_vec()],
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
        &[b"abandoned-history".to_vec()],
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
        &[b"abandoned-leaf-history".to_vec()],
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
        let step = reclaim_step(&mut conn, &lineage, 1).unwrap();
        assert!(step.work_rows() <= 1);
        reclaimed_rows = reclaimed_rows.saturating_add(step.work_rows());
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

    let history = vec![b"h0".to_vec(), b"h1".to_vec(), b"h2".to_vec()];
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
        &[b"stale-unique-history".to_vec()],
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
            &[format!("history-{table}").into_bytes()],
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
            &[format!("crash-history-{role}").into_bytes()],
            &[format!("crash-transcript-{role}").into_bytes()],
            format!("crash-state-{role}").as_bytes(),
            LineageOperation::Append,
            ObjectCompression::none(),
            2,
        );
        panic!("lineage crash trigger did not abort: {result:?}");
    }

    let dir = tempfile::tempdir().unwrap();
    for role in ["node", "revision", "head", "receipt"] {
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
            &[format!("crash-history-{role}").into_bytes()],
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
            &[b"shared".to_vec()],
            &[b"shared".to_vec()],
            b"shared",
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
            &[b"abandoned".to_vec()],
            &[b"abandoned".to_vec()],
            b"abandoned",
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
        &[b"h0".to_vec(), b"h1".to_vec()],
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
        &[b"h2".to_vec()],
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
        &[b"new-history".to_vec()],
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
        &[b"later-history".to_vec()],
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
        &[b"history-1".to_vec()],
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
        &[b"history-2".to_vec()],
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
        &[b"fork-history".to_vec()],
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
                    format!("history-{round}-0").into_bytes(),
                    format!("history-{round}-1").into_bytes(),
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
