#![no_main]

//! State-machine fuzzing for session storage. Commands run through the public
//! writer and reader APIs. An in-memory model checks history, side rows,
//! transcript records, metadata, running totals, ownership, fork and backup.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use protocol::{AssistantStep, Content, HistoryItem};
use smelt_store::{
    HistorySuffix, SessionCommit, SessionIdentity, SessionMetadata, SessionWriter, SideRows,
    Store, StoreError, StoredTranscriptBlock, TranscriptRecordSuffix,
};
use std::path::Path;

const SESSION_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FORK_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const MAX_OPS: usize = 48;
const MAX_ITEMS: usize = 8;
const MAX_TEXT_CHARS: usize = 96;
const SIDE_TABLES: usize = 4;
const WIDTHS: [u16; 3] = [20, 80, 240];

#[derive(Arbitrary, Debug)]
struct Input {
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    Commit(CommitInput),
    InvalidSuffix { transcript: bool },
    Contend,
    Reopen,
    Fork,
    Backup,
}

#[derive(Arbitrary, Debug)]
struct CommitInput {
    history: Option<(u8, Vec<Entry>)>,
    transcript: Option<(u8, Vec<Entry>)>,
    side: Option<(u8, u8, u8)>,
    title: Option<String>,
    first_user_message: Option<Option<String>>,
    checkpoint: Option<Option<u8>>,
}

#[derive(Arbitrary, Debug)]
enum Entry {
    User(String),
    Assistant(String),
}

#[derive(Clone, Debug, Default)]
struct Model {
    created: bool,
    metadata: SessionMetadata,
    first_user_message: Option<String>,
    checkpoint: Option<serde_json::Value>,
    history: Vec<HistoryItem>,
    side: [Vec<(u64, serde_json::Value)>; SIDE_TABLES],
    transcript: Vec<StoredTranscriptBlock>,
}

fuzz_target!(|input: Input| run(input));

fn run(input: Input) {
    let temp = tempfile::tempdir().expect("create store fuzz tempdir");
    let store = Store::new(temp.path().join("sessions"));
    let mut writer = Some(store.writer(SESSION_ID).expect("open writer"));
    let mut model = Model::default();
    let mut backups = 0;

    for op in input.ops.into_iter().take(MAX_OPS) {
        let active = writer.as_mut().expect("writer open");
        match op {
            Op::Commit(commit_input) => commit(active, &mut model, commit_input),
            Op::InvalidSuffix { transcript } => invalid_suffix(active, &model, transcript),
            Op::Contend => assert!(matches!(
                store.writer(SESSION_ID),
                Err(StoreError::Owned { .. })
            )),
            Op::Reopen => {
                drop(writer.take());
                let reopened = store.writer(SESSION_ID).expect("reopen writer");
                assert_eq!(reopened.exists(), model.created);
                writer = Some(reopened);
            }
            Op::Fork => fork(active, &store, &model),
            Op::Backup => {
                backups += 1;
                backup(&store, temp.path(), backups, &model);
            }
        }
        assert_session(&store, SESSION_ID, &model, None);
    }

    drop(writer);
    assert!(store.integrity_issues().expect("integrity check").is_empty());
    if model.created {
        store.delete(SESSION_ID).expect("delete session");
        assert!(!store.exists(SESSION_ID).expect("check deleted session"));
    }
}

fn commit(writer: &mut SessionWriter, model: &mut Model, input: CommitInput) {
    let mut commit = SessionCommit {
        metadata: model.metadata.clone(),
        ..Default::default()
    };
    if let Some(title) = input.title {
        commit.metadata.title = Some(clip(title));
    }
    if let Some((keep, entries)) = input.history {
        let start = usize::from(keep) % (model.history.len() + 1);
        let items: Vec<HistoryItem> = entries.into_iter().take(MAX_ITEMS).map(history_item).collect();
        model.history.truncate(start);
        model.history.extend(items.iter().cloned());
        commit.history = Some(HistorySuffix {
            start: start as u64,
            items,
        });
    }
    if let Some((keep, entries)) = input.transcript {
        let start = usize::from(keep) % (model.transcript.len() + 1);
        let records: Vec<StoredTranscriptBlock> = entries
            .into_iter()
            .take(MAX_ITEMS)
            .enumerate()
            .map(|(offset, entry)| transcript_record(start + offset, entry))
            .collect();
        model.transcript.truncate(start);
        model.transcript.extend(records.iter().cloned());
        commit.transcript = Some(TranscriptRecordSuffix {
            start: start as u64,
            records,
        });
    }
    if let Some((table, start, count)) = input.side {
        let table = usize::from(table) % SIDE_TABLES;
        let start = u64::from(start % 32);
        let rows: Vec<(u64, serde_json::Value)> = (0..u64::from(count % 6))
            .map(|offset| (start + offset * 2, serde_json::json!({ "row": start + offset })))
            .collect();
        model.side[table].retain(|(key, _)| *key < start);
        model.side[table].extend(rows.iter().cloned());
        let suffix = Some(SideRows { start, rows });
        match table {
            0 => commit.turn_metas = suffix,
            1 => commit.metadata_snapshots = suffix,
            2 => commit.context_snapshots = suffix,
            _ => commit.checkpoint_events = suffix,
        }
    }
    if let Some(message) = input.first_user_message {
        let message = message.map(clip);
        model.first_user_message = message.clone();
        commit.first_user_message = Some(message);
    }
    if let Some(checkpoint) = input.checkpoint {
        let checkpoint = checkpoint.map(|at| serde_json::json!({ "first_live_index": at }));
        model.checkpoint = checkpoint.clone();
        commit.checkpoint = Some(checkpoint);
    }
    model.metadata = commit.metadata.clone();

    let head = writer.commit(&identity(SESSION_ID, None), &commit).expect("commit");
    model.created = true;
    assert_eq!(head.history_len.as_usize(), model.history.len());
    assert_eq!(head.transcript_record_count.as_usize(), model.transcript.len());
    assert_eq!(writer.head(), head);
}

fn invalid_suffix(writer: &mut SessionWriter, model: &Model, transcript: bool) {
    let mut commit = SessionCommit {
        metadata: SessionMetadata {
            title: Some("rejected".into()),
            ..model.metadata.clone()
        },
        ..Default::default()
    };
    if transcript {
        let start = model.transcript.len() + 1;
        commit.transcript = Some(TranscriptRecordSuffix {
            start: start as u64,
            records: vec![transcript_record(start, Entry::User("gap".into()))],
        });
    } else {
        commit.history = Some(HistorySuffix {
            start: model.history.len() as u64 + 1,
            items: vec![history_item(Entry::User("gap".into()))],
        });
    }
    let before = writer.head();
    assert!(matches!(
        writer.commit(&identity(SESSION_ID, None), &commit),
        Err(StoreError::InvalidSuffix { .. })
    ));
    assert_eq!(writer.head(), before);
}

fn fork(writer: &mut SessionWriter, store: &Store, model: &Model) {
    let result = writer.fork_to(FORK_ID, 7, store);
    if !model.created {
        assert!(matches!(result, Err(StoreError::NotFound { .. })));
        return;
    }
    let head = result.expect("fork session");
    assert_eq!(head.history_len.as_usize(), model.history.len());
    assert_session(store, FORK_ID, model, Some(SESSION_ID));
    assert!(matches!(
        writer.fork_to(FORK_ID, 8, store),
        Err(StoreError::AlreadyExists { .. })
    ));
    store.delete(FORK_ID).expect("delete fork");
    assert!(!store.exists(FORK_ID).expect("check deleted fork"));
}

fn backup(store: &Store, root: &Path, index: u32, model: &Model) {
    let backup_root = root.join(format!("backup-{index}"));
    std::fs::create_dir(&backup_root).expect("create backup root");
    let copy = Store::new(&backup_root);
    store.backup_to(&copy.database_path()).expect("back up store");
    assert!(store.backup_to(&copy.database_path()).is_err());
    assert_session(&copy, SESSION_ID, model, None);
}

fn assert_session(store: &Store, id: &str, model: &Model, parent: Option<&str>) {
    if !model.created {
        assert!(!store.exists(id).expect("check missing session"));
        return;
    }
    let reader = store.reader(id).expect("open reader");
    let state = reader.state().expect("read state");
    assert_eq!(state.identity.id, id);
    assert_eq!(state.identity.parent_id.as_deref(), parent);
    assert_eq!(state.metadata.title, model.metadata.title);
    assert_eq!(state.first_user_message, model.first_user_message);
    assert_eq!(state.checkpoint, model.checkpoint);
    assert_eq!(state.head.history_len.as_usize(), model.history.len());
    assert_eq!(state.head.transcript_record_count.as_usize(), model.transcript.len());
    let side = [
        &state.turn_metas,
        &state.metadata_snapshots,
        &state.context_snapshots,
        &state.checkpoint_events,
    ];
    for (stored, expected) in side.into_iter().zip(&model.side) {
        let mut expected = expected.clone();
        expected.sort_by_key(|(key, _)| *key);
        assert_eq!(stored, &expected);
    }

    let history_len = model.history.len() as u64;
    assert_eq!(reader.history_range(0, history_len).expect("read history"), model.history);
    let record_count = model.transcript.len() as u64;
    assert_eq!(
        reader.transcript_range(0, record_count).expect("read transcript"),
        model.transcript
    );

    let stats = reader.storage_stats(store).expect("storage stats");
    assert_eq!(stats.history_rows, history_len);
    assert_eq!(stats.transcript_rows, record_count);
    assert_eq!(state.history_text_bytes, stats.history_text_bytes);
    for width in WIDTHS {
        let expected: u64 = model
            .transcript
            .iter()
            .map(|record| record.extent_profile().estimated_rows(width))
            .sum();
        assert_eq!(
            reader
                .transcript_total_estimated_rows(width)
                .expect("estimate rows"),
            expected
        );
    }
}

fn identity(id: &str, parent_id: Option<&str>) -> SessionIdentity {
    SessionIdentity {
        id: id.into(),
        created_at: 1,
        parent_id: parent_id.map(Into::into),
    }
}

fn history_item(entry: Entry) -> HistoryItem {
    match entry {
        Entry::User(text) => HistoryItem::user(Content::text(clip(text))),
        Entry::Assistant(text) => HistoryItem::Assistant(AssistantStep::terminal(
            Some(Content::text(clip(text))),
            None,
            Vec::new(),
        )),
    }
}

fn transcript_record(block_idx: usize, entry: Entry) -> StoredTranscriptBlock {
    let (kind, text) = match entry {
        Entry::User(text) => ("user", clip(text)),
        Entry::Assistant(text) => ("text", clip(text)),
    };
    StoredTranscriptBlock {
        block_idx: block_idx as u64,
        history_idx: None,
        kind: kind.into(),
        tool_call_id: None,
        tool_name: None,
        content_hash: format!("{block_idx:016x}"),
        estimated_text_bytes: text.len() as u64,
        indexed_text: text.clone(),
        block_json: serde_json::json!({ "text": text }).to_string(),
        origin_json: None,
        tool_state_json: None,
        tool_render_revision: 0,
    }
}

fn clip(text: String) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}
