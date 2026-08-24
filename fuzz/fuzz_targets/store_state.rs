#![no_main]

//! File-backed state-machine fuzzing for canonical lineage persistence. Commands
//! run through the public writer and reader APIs. An independent in-memory model
//! checks canonical history, side tables, transcript records, revisions,
//! idempotency, rollback, reopen, backup, reclamation, and vacuuming.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use protocol::{AssistantStep, Content, HistoryItem};
use smelt_store::{
    HistoryIndex, HistoryLen, HistorySuffix, LineageSessionReader, LineageSessionState,
    OwnedLineageWriter, Revision, SaveReceipt, SessionCommit, SessionCommitFailure, SessionCostUsd,
    SessionIdentity, SessionMetadata, SideTableSuffixes, StoreHead, StoredTranscriptBlock,
    TranscriptRecordIndex, TranscriptRecordSuffix,
};
use std::path::Path;

const SESSION_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MAX_OPS: usize = 48;
const MAX_REPLACEMENT_ITEMS: usize = 8;
const MAX_TEXT_CHARS: usize = 96;

#[derive(Arbitrary, Debug)]
struct Input {
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    Commit {
        keep: u8,
        items: Vec<Entry>,
        state: StateInput,
        side_seed: u8,
    },
    RepeatLast,
    RejectStale,
    RejectInvalid {
        kind: u8,
    },
    Reopen,
    Backup,
    Maintain {
        kind: u8,
    },
}

#[derive(Arbitrary, Debug)]
enum Entry {
    User { text: String, image_units: u8 },
    Assistant { text: String },
}

#[derive(Arbitrary, Debug)]
struct StateInput {
    title: String,
    mode: u8,
    model: String,
    fast_mode: Option<bool>,
    checkpoint: bool,
    checkpoint_at: u8,
    cost_cents: u16,
}

#[derive(Clone, Default)]
struct Model {
    head: StoreHead,
    identity: Option<SessionIdentity>,
    metadata: Option<SessionMetadata>,
    history: Vec<HistoryItem>,
    turn_metas: Vec<(u64, serde_json::Value)>,
    metadata_snapshots: Vec<(u64, serde_json::Value)>,
    context_snapshots: Vec<(u64, serde_json::Value)>,
    transcript_records: Vec<StoredTranscriptBlock>,
    last_commit: Option<(SessionCommit, SaveReceipt)>,
    next_sequence: u64,
    backup_id: u64,
}

#[derive(Debug, PartialEq)]
struct StoreObservation {
    snapshot: Option<LineageSessionState>,
    transcript_records: Vec<StoredTranscriptBlock>,
    history_rows: u64,
    transcript_record_rows: u64,
    object_rows: u64,
    request_rows: u64,
}

fuzz_target!(|input: Input| run(input));

fn run(input: Input) {
    let temp = tempfile::tempdir().expect("create store fuzz tempdir");
    let sessions_root = temp.path().join("primary");
    std::fs::create_dir(&sessions_root).expect("create primary sessions root");
    let mut writer = Some(open_writer(&sessions_root));
    let mut model = Model::default();
    assert_store(&sessions_root, &model);

    for op in input.ops.into_iter().take(MAX_OPS) {
        match op {
            Op::Commit {
                keep,
                items,
                state,
                side_seed,
            } => commit(
                writer.as_mut().expect("writer open"),
                &mut model,
                keep,
                items,
                state,
                side_seed,
            ),
            Op::RepeatLast => repeat_last(
                writer.as_mut().expect("writer open"),
                &model,
                &sessions_root,
            ),
            Op::RejectStale => reject_stale(
                writer.as_mut().expect("writer open"),
                &model,
                &sessions_root,
            ),
            Op::RejectInvalid { kind } => reject_invalid(
                writer.as_mut().expect("writer open"),
                &model,
                &sessions_root,
                kind,
            ),
            Op::Reopen => reopen(&sessions_root, &mut writer),
            Op::Backup => backup(temp.path(), &sessions_root, &mut model),
            Op::Maintain { kind } => maintain(&mut writer, &model, kind),
        }
        assert_store(&sessions_root, &model);
    }

    writer.take().expect("writer open").release().unwrap();
    assert_store(&sessions_root, &model);
    if model.identity.is_some() {
        OwnedLineageWriter::open_existing(&sessions_root, SESSION_ID)
            .expect("clean release should relinquish lineage ownership")
            .release()
            .unwrap();
    }
}

fn commit(
    writer: &mut OwnedLineageWriter,
    model: &mut Model,
    keep: u8,
    items: Vec<Entry>,
    state_input: StateInput,
    side_seed: u8,
) {
    let keep = usize::from(keep) % (model.history.len() + 1);
    let mut history = model.history[..keep].to_vec();
    history.extend(
        items
            .into_iter()
            .take(MAX_REPLACEMENT_ITEMS)
            .map(history_item),
    );
    let side_tables = side_tables(history.len(), side_seed);
    let transcript_records = transcript_records(&history);
    let sequence = model.next_sequence;
    model.next_sequence = model.next_sequence.saturating_add(1);
    let command = SessionCommit {
        session_id: SESSION_ID.to_string(),
        expected: model.head,
        identity: session_identity(),
        metadata: session_metadata(&state_input, history.len(), sequence),
        history: HistorySuffix {
            start: HistoryIndex::new(keep as u64),
            final_len: HistoryLen::new(history.len() as u64),
            items: history[keep..].to_vec(),
        },
        side_tables: typed_side_tables(&side_tables),
        transcript_records: Some(TranscriptRecordSuffix {
            start: TranscriptRecordIndex::ZERO,
            records: transcript_records.clone(),
        }),
    };

    let receipt = writer.commit_session(&command).unwrap();
    assert_eq!(receipt.previous, model.head);
    assert_eq!(receipt.current.history_len.get(), history.len() as u64);
    assert_eq!(
        receipt.current.transcript_record_count.get(),
        transcript_records.len() as u64
    );
    model.head = receipt.current;
    model.identity = Some(command.identity.clone());
    model.metadata = Some(command.metadata.clone());
    model.history = history;
    model.turn_metas = side_tables.0;
    model.metadata_snapshots = side_tables.1;
    model.context_snapshots = side_tables.2;
    model.transcript_records = transcript_records;
    model.last_commit = Some((command, receipt));
}

fn repeat_last(writer: &mut OwnedLineageWriter, model: &Model, sessions_root: &Path) {
    let Some((command, receipt)) = &model.last_commit else {
        return;
    };
    let before = observe_store(sessions_root);
    let repeated = writer.commit_session(command).unwrap();
    assert_eq!(&repeated, receipt, "idempotent commit changed its receipt");
    assert_eq!(
        observe_store(sessions_root),
        before,
        "idempotent commit changed persisted state"
    );
}

fn reject_stale(writer: &mut OwnedLineageWriter, model: &Model, sessions_root: &Path) {
    let before = observe_store(sessions_root);
    let mut command = current_command(model);
    command.expected.revision = Revision::new(model.head.revision.get().saturating_add(1));
    let error = writer.commit_session(&command).unwrap_err();
    assert_eq!(
        error,
        SessionCommitFailure::StaleBase {
            expected: command.expected,
            current: model.head,
        }
    );
    assert_eq!(
        observe_store(sessions_root),
        before,
        "stale commit did not roll back exactly"
    );
}

fn reject_invalid(writer: &mut OwnedLineageWriter, model: &Model, sessions_root: &Path, kind: u8) {
    let before = observe_store(sessions_root);
    let mut command = current_command(model);
    let expected = match kind % 4 {
        0 => {
            command
                .history
                .items
                .push(HistoryItem::user(Content::text("extra")));
            "history"
        }
        1 => {
            command.transcript_records = Some(TranscriptRecordSuffix {
                start: TranscriptRecordIndex::new(
                    model.head.transcript_record_count.get().saturating_add(1),
                ),
                records: Vec::new(),
            });
            "transcript_record"
        }
        2 => {
            command.side_tables.turn_metas.push((
                HistoryIndex::new(model.head.history_len.get().saturating_add(1)),
                serde_json::json!({"invalid": true}),
            ));
            "side_table"
        }
        _ => {
            command.identity.id = "wrong-session".to_string();
            "session"
        }
    };
    let error = writer.commit_session(&command).unwrap_err();
    match (expected, error) {
        ("history", SessionCommitFailure::InvalidHistorySuffix { .. })
        | ("transcript_record", SessionCommitFailure::InvalidTranscriptRecordSuffix { .. })
        | ("side_table", SessionCommitFailure::InvalidSideTableRow { .. })
        | ("session", SessionCommitFailure::SessionMismatch { .. }) => {}
        (expected, actual) => panic!("expected {expected} rejection, got {actual:?}"),
    }
    assert_eq!(
        observe_store(sessions_root),
        before,
        "invalid commit did not roll back exactly"
    );
}

fn reopen(sessions_root: &Path, writer: &mut Option<OwnedLineageWriter>) {
    writer.take().expect("writer open").release().unwrap();
    *writer = Some(open_writer(sessions_root));
}

fn backup(root: &Path, sessions_root: &Path, model: &mut Model) {
    if model.identity.is_none() {
        return;
    }
    model.backup_id = model.backup_id.saturating_add(1);
    let path = root.join(format!("backup-{}.db", model.backup_id));
    let reader = LineageSessionReader::open_existing(sessions_root, SESSION_ID).unwrap();
    reader.backup_to(&path).unwrap();
    let report = smelt_store::verify_lineage_backup(&path, reader.lineage_id()).unwrap();
    assert!(report.healthy, "backup doctor issues: {:?}", report.issues);
}

fn maintain(
    writer: &mut Option<OwnedLineageWriter>,
    model: &Model,
    kind: u8,
) {
    if model.identity.is_none() {
        return;
    }
    let active = writer.as_mut().expect("writer open");
    match kind % 2 {
        0 => {
            for _ in 0..256 {
                if active.reclaim_step(16).unwrap().complete {
                    break;
                }
            }
        }
        _ => {
            active.vacuum().unwrap();
        }
    }
}

fn assert_store(sessions_root: &Path, model: &Model) {
    let reader = LineageSessionReader::try_open_existing(sessions_root, SESSION_ID).unwrap();
    match (reader, model.identity.is_some()) {
        (None, false) => {}
        (Some(reader), true) => assert_reader(&reader, model),
        (None, true) => panic!("modeled session has no canonical lineage branch"),
        (Some(_), false) => panic!("empty model published a lineage branch"),
    }
}

fn observe_store(sessions_root: &Path) -> StoreObservation {
    let Some(reader) = LineageSessionReader::try_open_existing(sessions_root, SESSION_ID).unwrap()
    else {
        return StoreObservation {
            snapshot: None,
            transcript_records: Vec::new(),
            history_rows: 0,
            transcript_record_rows: 0,
            object_rows: 0,
            request_rows: 0,
        };
    };
    let stats = reader.storage_stats().unwrap();
    let snapshot = reader.snapshot().unwrap();
    StoreObservation {
        transcript_records: reader.transcript_range(0, snapshot.transcript_len).unwrap(),
        snapshot: Some(snapshot),
        history_rows: stats.history_rows,
        transcript_record_rows: stats.transcript_record_rows,
        object_rows: stats.object_rows,
        request_rows: stats.request_rows,
    }
}

fn assert_reader(reader: &LineageSessionReader, model: &Model) {
    let doctor = reader.doctor_report().unwrap();
    assert!(doctor.healthy, "store doctor issues: {:?}", doctor.issues);

    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.identity, model.identity.clone().unwrap());
    assert_eq!(snapshot.metadata, model.metadata.clone().unwrap());
    assert_eq!(snapshot.head, model.head);
    assert_eq!(
        snapshot.side_tables,
        typed_side_tables(&(
            model.turn_metas.clone(),
            model.metadata_snapshots.clone(),
            model.context_snapshots.clone(),
        ))
    );
    assert_eq!(
        reader
            .history_range(0, snapshot.head.history_len.get())
            .unwrap(),
        model.history
    );
    assert_eq!(
        reader.transcript_range(0, snapshot.transcript_len).unwrap(),
        model.transcript_records
    );
    assert!(
        reader.storage_stats().unwrap().object_rows >= attachment_object_count(&model.history),
        "store lost a reachable attachment object"
    );
}

fn current_command(model: &Model) -> SessionCommit {
    let identity = model.identity.clone().unwrap_or_else(session_identity);
    let metadata = model
        .metadata
        .clone()
        .unwrap_or_else(empty_session_metadata);
    SessionCommit {
        session_id: SESSION_ID.to_string(),
        expected: model.head,
        identity,
        metadata,
        history: HistorySuffix {
            start: HistoryIndex::new(model.history.len() as u64),
            final_len: HistoryLen::new(model.history.len() as u64),
            items: Vec::new(),
        },
        side_tables: SideTableSuffixes {
            start: HistoryIndex::new(model.history.len() as u64),
            ..SideTableSuffixes::default()
        },
        transcript_records: None,
    }
}

fn open_writer(sessions_root: &Path) -> OwnedLineageWriter {
    OwnedLineageWriter::open(sessions_root, SESSION_ID).unwrap()
}

fn history_item(entry: Entry) -> HistoryItem {
    match entry {
        Entry::User { text, image_units } => {
            let text = small_text(text);
            let images = if image_units % 3 == 0 {
                Vec::new()
            } else {
                let units = usize::from(image_units % 32) + 1;
                vec![(
                    "fuzz.png".to_string(),
                    format!("data:image/png;base64,{}", "AAAA".repeat(units)),
                )]
            };
            HistoryItem::user(Content::with_images(text, images))
        }
        Entry::Assistant { text } => HistoryItem::Assistant(AssistantStep::terminal(
            Some(Content::text(small_text(text))),
            None,
            Vec::new(),
        )),
    }
}

fn transcript_records(history: &[HistoryItem]) -> Vec<StoredTranscriptBlock> {
    history
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let kind = match item {
                HistoryItem::User { .. } => "user",
                HistoryItem::Assistant(_) => "assistant",
                HistoryItem::System { .. } | HistoryItem::Note(_) => "assistant",
            };
            let preview = format!("{kind}-{index}");
            StoredTranscriptBlock {
                block_idx: index as u64,
                history_idx: Some(index as u64),
                kind: kind.to_string(),
                tool_call_id: None,
                tool_name: None,
                content_hash: format!("fuzz-{kind}-{index}"),
                estimated_text_bytes: preview.len() as u64,
                preview_text: preview.clone(),
                indexed_text: preview.clone(),
                block_json: serde_json::json!({
                    "kind": kind,
                    "text": preview,
                })
                .to_string(),
                origin_json: Some(serde_json::json!({"History": index}).to_string()),
                tool_state_json: None,
                tool_render_revision: 0,
            }
        })
        .collect()
}

type SideTables = (
    Vec<(u64, serde_json::Value)>,
    Vec<(u64, serde_json::Value)>,
    Vec<(u64, serde_json::Value)>,
);

fn attachment_object_count(history: &[HistoryItem]) -> u64 {
    let mut urls = std::collections::BTreeSet::new();
    let value = serde_json::to_value(history).expect("serialize modeled history");
    collect_attachment_urls(&value, &mut urls);
    urls.len() as u64
}

fn collect_attachment_urls(
    value: &serde_json::Value,
    urls: &mut std::collections::BTreeSet<String>,
) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(url) = object
                .get("image_url")
                .and_then(serde_json::Value::as_object)
                .and_then(|image| image.get("url"))
                .and_then(serde_json::Value::as_str)
            {
                urls.insert(url.to_string());
            }
            for value in object.values() {
                collect_attachment_urls(value, urls);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_attachment_urls(value, urls);
            }
        }
        _ => {}
    }
}

fn side_tables(history_len: usize, seed: u8) -> SideTables {
    let mut turn = Vec::new();
    let mut metadata = Vec::new();
    let mut context = Vec::new();
    for index in 0..=history_len {
        let selector = seed.wrapping_add(index as u8);
        if selector & 1 != 0 {
            turn.push((index as u64, serde_json::json!({"turn": selector})));
        }
        if selector & 2 != 0 {
            metadata.push((index as u64, serde_json::json!({"meta": selector})));
        }
        if selector & 4 != 0 {
            context.push((index as u64, serde_json::json!({"tokens": index})));
        }
    }
    (turn, metadata, context)
}

fn typed_side_tables(side_tables: &SideTables) -> SideTableSuffixes {
    SideTableSuffixes {
        start: HistoryIndex::ZERO,
        turn_metas: side_tables
            .0
            .iter()
            .map(|(index, value)| (HistoryIndex::new(*index), value.clone()))
            .collect(),
        metadata_snapshots: side_tables
            .1
            .iter()
            .map(|(index, value)| (HistoryIndex::new(*index), value.clone()))
            .collect(),
        context_snapshots: side_tables
            .2
            .iter()
            .map(|(index, value)| (HistoryIndex::new(*index), value.clone()))
            .collect(),
    }
}

fn session_identity() -> SessionIdentity {
    SessionIdentity {
        id: SESSION_ID.to_string(),
        created_at: 1,
        parent_id: None,
    }
}

fn session_metadata(input: &StateInput, history_len: usize, sequence: u64) -> SessionMetadata {
    let first_live_index = if history_len == 0 {
        0
    } else {
        usize::from(input.checkpoint_at) % (history_len + 1)
    };
    SessionMetadata {
        title: nonempty(small_text(input.title.clone())),
        slug: Some(format!("fuzz-{sequence}")),
        first_user_message: None,
        cwd: Some("/tmp/smelt-fuzz".to_string()),
        mode: Some(["ask", "plan", "apply", "yolo"][usize::from(input.mode % 4)].to_string()),
        reasoning_effort: Some("medium".to_string()),
        model: nonempty(small_text(input.model.clone())),
        fast_mode: input.fast_mode,
        accounting_json: Some(serde_json::json!({"sequence": sequence})),
        checkpoint_json: input.checkpoint.then(|| {
            serde_json::json!({
                "kind": "compaction",
                "summary": format!("summary-{sequence}"),
                "first_live_index": first_live_index,
                "created_at_ms": sequence,
            })
        }),
        checkpoint_events_json: input.checkpoint.then(|| {
            serde_json::json!([{
                "kind": "compaction",
                "summary": format!("summary-{sequence}"),
                "first_live_index": first_live_index,
                "completed_at_history_len": history_len,
                "created_at_ms": sequence,
            }])
        }),
        context_tokens: Some(history_len as u64 * 10),
        context_tokens_history_len: Some(history_len as u64),
        display_context_tokens: Some(history_len as u64 * 8),
        session_cost_usd: SessionCostUsd::new(f64::from(input.cost_cents) / 100.0).unwrap(),
        updated_at: sequence as i64,
    }
}

fn empty_session_metadata() -> SessionMetadata {
    SessionMetadata {
        title: None,
        slug: None,
        first_user_message: None,
        cwd: None,
        mode: None,
        reasoning_effort: None,
        model: None,
        fast_mode: None,
        accounting_json: None,
        checkpoint_json: None,
        checkpoint_events_json: None,
        context_tokens: None,
        context_tokens_history_len: None,
        display_context_tokens: None,
        session_cost_usd: SessionCostUsd::new(0.0).unwrap(),
        updated_at: 1,
    }
}

fn small_text(value: String) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_TEXT_CHARS)
        .collect()
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
