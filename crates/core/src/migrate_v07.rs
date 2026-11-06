//! One-time import of state saved by smelt 0.7.
//!
//! The import moves the old files into `<state>/v0.7/`, imports sessions,
//! prompts and usage from there, and renames credentials to the current
//! layout. The archive stays until the user deletes it.
// COMPAT(state-v0.7)

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use smelt_store_v07 as v07;

use crate::session::{create_private_dir_all_in, SessionStorage};
use crate::transcript_model::{transcript_block_row_with_block_idx, TranscriptBlockRecord};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

/// Directory under the state root that keeps the original 0.7 sessions.
pub const ARCHIVE: &str = "v0.7";
/// Written into the archive once an import has run to the end.
const DONE: &str = "imported";
/// State entries moved into the archive and imported from there.
const ARCHIVED: [&str; 3] = ["sessions", "history", "metrics.jsonl"];
/// State entries no release reads anymore.
const REMOVED_STATE: [&str; 3] = ["state.json", "state.lock", "metrics.lock"];
/// Credentials and their names under `<state>/auth/`.
const AUTH: [(&str, &str); 4] = [
    ("codex_auth.json", "codex.json"),
    ("copilot_auth.json", "copilot.json"),
    ("kimi_code_auth.json", "kimi-code.json"),
    ("kimi_code_device_id", "kimi-code-device-id"),
];
/// Cache entries replaced by `<cache>/models/`.
const REMOVED_CACHE: [&str; 5] = [
    "acp-models",
    "codex_models.json",
    "copilot_models.json",
    "kimi_code_models.json",
    "claude-code-models.json",
];

/// What one import did.
#[derive(Debug, Default)]
pub struct Report {
    pub sessions: usize,
    /// Sessions already in the store.
    pub skipped: usize,
    pub prompts: usize,
    pub usage: usize,
    /// One line per session or file that could not be imported.
    pub failures: Vec<String>,
}

impl Report {
    /// Whether the import changed anything worth telling the user.
    pub fn is_empty(&self) -> bool {
        self.sessions == 0 && self.prompts == 0 && self.usage == 0 && self.failures.is_empty()
    }
}

/// Whether the state or cache directory still holds 0.7 files.
pub fn pending(state_root: &Path, cache_root: &Path) -> bool {
    let archive = state_root.join(ARCHIVE);
    ARCHIVED
        .iter()
        .chain(&REMOVED_STATE)
        .chain(AUTH.iter().map(|(old, _)| old))
        .any(|name| present(&state_root.join(name)))
        || REMOVED_CACHE
            .iter()
            .any(|name| present(&cache_root.join(name)))
        || (archive.is_dir() && !present(&archive.join(DONE)))
}

/// Imports the 0.7 state. Safe to repeat: sessions already in the store are
/// skipped, and imported prompt and usage files are deleted.
pub fn run(
    state_root: &Path,
    cache_root: &Path,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Report> {
    let locks = state_root.join("locks");
    create_private_dir_all_in(state_root, &locks)?;
    let lock = fs::File::create(locks.join("migrate.lock"))?;
    lock.lock()?;

    if v07_running(&state_root.join("sessions"))? {
        return Err("smelt 0.7 is still running; close it and start again".into());
    }
    let archive = state_root.join(ARCHIVE);
    for name in ARCHIVED {
        let path = state_root.join(name);
        if present(&path) {
            create_private_dir_all_in(state_root, &archive)?;
            fs::rename(&path, unused(&archive.join(name)))?;
        }
    }
    for name in REMOVED_STATE {
        remove(&state_root.join(name))?;
    }
    for (old, new) in AUTH {
        let path = state_root.join(old);
        if !present(&path) {
            continue;
        }
        let auth = state_root.join("auth");
        create_private_dir_all_in(state_root, &auth)?;
        let target = auth.join(new);
        if present(&target) {
            create_private_dir_all_in(state_root, &archive)?;
            fs::rename(&path, unused(&archive.join(old)))?;
        } else {
            fs::rename(&path, target)?;
        }
    }
    for name in REMOVED_CACHE {
        remove(&cache_root.join(name))?;
    }

    let mut report = Report::default();
    if !archive.is_dir() {
        return Ok(report);
    }
    let storage = SessionStorage::new(state_root.to_path_buf());
    import_sessions(
        &storage,
        &archived(&archive, "sessions")?,
        &mut report,
        progress,
    );
    for file in archived(&archive, "history")? {
        import_ledger(&file, &mut report, |text, report| {
            let prompts = prompts(text);
            storage.store().import_prompts(&prompts)?;
            report.prompts += prompts.len();
            Ok(())
        });
    }
    for file in archived(&archive, "metrics.jsonl")? {
        import_ledger(&file, &mut report, |text, report| {
            let (entries, unreadable) = usage(text);
            storage.store().import_usage(&entries)?;
            report.usage += entries.len();
            if unreadable > 0 {
                let file = file.display();
                report
                    .failures
                    .push(format!("{file}: {unreadable} unreadable lines"));
            }
            Ok(())
        });
    }
    fs::write(archive.join(DONE), b"")?;
    Ok(report)
}

/// Whether a running smelt 0.7 holds one of its session locks.
fn v07_running(sessions: &Path) -> Result<bool> {
    let Ok(entries) = fs::read_dir(sessions.join(".locks")) else {
        return Ok(false);
    };
    for entry in entries {
        let path = entry?.path();
        let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        if let Err(fs::TryLockError::WouldBlock) = file.try_lock() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn import_sessions(
    storage: &SessionStorage,
    roots: &[PathBuf],
    report: &mut Report,
    progress: &mut dyn FnMut(usize, usize),
) {
    let mut sessions = Vec::new();
    for root in roots {
        match v07::lineage_session_locations(root) {
            Ok(locations) => sessions.extend(locations.into_iter().map(|at| (root, at))),
            Err(error) => report.failures.push(format!("{}: {error}", root.display())),
        }
    }
    let total = sessions.len();
    for (done, (root, at)) in sessions.into_iter().enumerate() {
        progress(done, total);
        match import_session(storage, root, &at) {
            Ok(true) => report.sessions += 1,
            Ok(false) => report.skipped += 1,
            Err(error) => report
                .failures
                .push(format!("session {}: {error}", at.session_id)),
        }
    }
    progress(total, total);
}

/// Imports one session unless the store already has it.
fn import_session(
    storage: &SessionStorage,
    root: &Path,
    at: &v07::LineageSessionLocation,
) -> Result<bool> {
    let store = storage.store();
    let id = at.session_id.as_str();
    if store.exists(id)? {
        return Ok(false);
    }
    let reader = v07::LineageSessionReader::try_open_existing_in_lineage(root, &at.lineage_id, id)?
        .ok_or("missing from its lineage")?;
    let old = reader.snapshot()?;
    // One item at a time, so each 0.7 item is freed once converted.
    let history = reader
        .history_range(0, old.head.history_len.get())?
        .into_iter()
        .map(|item| convert(&item))
        .collect::<Result<_>>()?;
    let records = reader
        .transcript_range(0, old.head.transcript_record_count.get())?
        .into_iter()
        .map(|row| transcript_row(&row))
        .collect::<Result<_>>()?;
    let metadata = old.metadata;
    let rows = |rows: Vec<(v07::HistoryIndex, Value)>| {
        Some(smelt_store::SideRows {
            start: 0,
            rows: rows.into_iter().map(|(at, row)| (at.get(), row)).collect(),
        })
    };
    let checkpoint_events = match metadata.checkpoint_events_json {
        Some(Value::Array(events)) => events,
        _ => Vec::new(),
    };
    let commit = smelt_store::SessionCommit {
        metadata: smelt_store::SessionMetadata {
            title: metadata.title,
            slug: metadata.slug,
            cwd: metadata.cwd,
            mode: metadata.mode,
            reasoning_effort: metadata.reasoning_effort,
            model: metadata.model,
            fast_mode: metadata.fast_mode,
            accounting: metadata.accounting_json.map(accounting),
            context_tokens: metadata.context_tokens,
            context_tokens_history_len: metadata.context_tokens_history_len,
            display_context_tokens: metadata.display_context_tokens,
            cost_usd: metadata.session_cost_usd.get(),
            // 0.7 kept turn ids only in tables the import drops.
            last_turn_id: 0,
            updated_at: metadata.updated_at,
        },
        first_user_message: Some(metadata.first_user_message),
        checkpoint: Some(metadata.checkpoint_json),
        history: Some(smelt_store::HistorySuffix {
            start: 0,
            items: history,
        }),
        turn_metas: rows(old.side_tables.turn_metas),
        metadata_snapshots: rows(old.side_tables.metadata_snapshots),
        context_snapshots: rows(old.side_tables.context_snapshots),
        checkpoint_events: Some(smelt_store::SideRows {
            start: 0,
            rows: (0..).zip(checkpoint_events).collect(),
        }),
        transcript: Some(smelt_store::TranscriptRecordSuffix { start: 0, records }),
    };
    let identity = smelt_store::SessionIdentity {
        id: id.to_owned(),
        created_at: old.identity.created_at,
        parent_id: old.identity.parent_id,
    };
    store.writer(id)?.commit(&identity, &commit)?;
    drop(commit);
    if let Err(error) = storage.load_meta(id) {
        store.delete(id)?;
        return Err(error.into());
    }

    let artifacts = storage.artifact_dir_for_id(id);
    let old_artifacts = root.join(".artifacts").join(id);
    if present(&old_artifacts) && !present(&artifacts) {
        create_private_dir_all_in(storage.state_root(), &store.artifacts_dir())?;
        fs::rename(old_artifacts, &artifacts)?;
    }
    let digest: String = Sha256::digest(id.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let link = root.join("claude-code").join(format!("{digest}.json"));
    if present(&link) {
        create_private_dir_all_in(storage.state_root(), &artifacts)?;
        fs::rename(link, artifacts.join("claude-code.json"))?;
    }
    Ok(true)
}

/// The row as the current encoder writes it, so hydration accepts its hash.
fn transcript_row(row: &v07::StoredTranscriptBlock) -> Result<smelt_store::StoredTranscriptBlock> {
    let row: smelt_store::StoredTranscriptBlock = convert(row)?;
    let block_idx = row.block_idx;
    let mut record = TranscriptBlockRecord::try_from(row)?;
    record.content_hash = record.block.clone().normalize_content().content_hash();
    Ok(transcript_block_row_with_block_idx(0, block_idx, &record)?)
}

/// Accounting nests session usage under `session_usage`; 0.7 sometimes
/// stored the usage itself.
fn accounting(value: Value) -> Value {
    match value {
        Value::Object(fields) if fields.contains_key("session_usage") => Value::Object(fields),
        usage => serde_json::json!({ "session_usage": usage }),
    }
}

/// Imports one archived ledger file, deleting it once its rows are stored.
fn import_ledger(
    file: &Path,
    report: &mut Report,
    import: impl FnOnce(&str, &mut Report) -> Result<()>,
) {
    let result = fs::read(file)
        .map_err(Error::from)
        .and_then(|bytes| import(&String::from_utf8_lossy(&bytes), report))
        .and_then(|()| Ok(fs::remove_file(file)?));
    if let Err(error) = result {
        report.failures.push(format!("{}: {error}", file.display()));
    }
}

/// Prompts were records separated by RS (0x1e).
fn prompts(text: &str) -> Vec<String> {
    text.split('\x1e')
        .filter(|prompt| !prompt.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Usage was one JSON entry per line, without `calls` for single calls.
/// Returns the entries and the number of unreadable lines.
fn usage(text: &str) -> (Vec<smelt_store::UsageEntry>, usize) {
    let entry = |line: &str| {
        let mut value: Value = serde_json::from_str(line).ok()?;
        value.as_object_mut()?.entry("calls").or_insert(1.into());
        serde_json::from_value(value).ok()
    };
    let mut unreadable = 0;
    let entries = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let entry = entry(line);
            unreadable += usize::from(entry.is_none());
            entry
        })
        .collect();
    (entries, unreadable)
}

/// Converts a 0.7 value to its current type through JSON.
fn convert<T: DeserializeOwned>(value: &impl Serialize) -> Result<T> {
    Ok(serde_json::from_slice(&serde_json::to_vec(value)?)?)
}

/// Archived entries named `base` or `base.<n>`, in name order.
fn archived(archive: &Path, base: &str) -> Result<Vec<PathBuf>> {
    let prefix = format!("{base}.");
    let mut paths = Vec::new();
    for entry in fs::read_dir(archive)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name == base
            || name
                .strip_prefix(&prefix)
                .is_some_and(|n| n.parse::<u32>().is_ok())
        {
            paths.push(archive.join(&*name));
        }
    }
    paths.sort();
    Ok(paths)
}

/// `path`, or `path.<n>` with the first `n` not taken.
fn unused(path: &Path) -> PathBuf {
    let mut candidate = path.to_path_buf();
    let mut n = 1;
    while present(&candidate) {
        candidate = PathBuf::from(format!("{}.{n}", path.display()));
        n += 1;
    }
    candidate
}

fn present(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn remove(path: &Path) -> std::io::Result<()> {
    let result = match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(_) => return Ok(()),
    };
    match result {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use protocol::{Content, HistoryItem};

    use super::*;
    use crate::transcript_model::{transcript_block_row, Block, BlockOrigin};

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Saves one session the way smelt 0.7 did, with a stale block hash, and
    /// returns its still open writer.
    fn save_v07_session(sessions: &Path, history: &[HistoryItem]) -> v07::OwnedLineageWriter {
        let record = TranscriptBlockRecord {
            block: Block::User {
                text: "hello".into(),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            },
            content_hash: 1,
            origin: Some(BlockOrigin::History(0)),
            tool_state: None,
            tool_render_revision: 0,
        };
        let mut writer = v07::OwnedLineageWriter::open(sessions, ID).unwrap();
        writer
            .commit_session(&v07::SessionCommit {
                session_id: ID.into(),
                expected: v07::StoreHead::default(),
                identity: v07::SessionIdentity {
                    id: ID.into(),
                    created_at: 1_000,
                    parent_id: None,
                },
                metadata: convert(&serde_json::json!({
                    "title": "old session",
                    "slug": null,
                    "first_user_message": "hello",
                    "cwd": "/work",
                    "mode": null,
                    "reasoning_effort": null,
                    "model": "model",
                    "accounting_json": null,
                    "checkpoint_json": null,
                    "checkpoint_events_json": null,
                    "context_tokens": null,
                    "context_tokens_history_len": null,
                    "display_context_tokens": null,
                    "session_cost_usd": 1.5,
                    "updated_at": 2_000,
                }))
                .unwrap(),
                history: v07::HistorySuffix {
                    start: v07::HistoryIndex::new(0),
                    final_len: v07::HistoryLen::new(history.len() as u64),
                    items: convert(&history).unwrap(),
                },
                side_tables: v07::SideTableSuffixes::default(),
                transcript_records: Some(v07::TranscriptRecordSuffix {
                    start: v07::TranscriptRecordIndex::new(0),
                    records: vec![convert(&transcript_block_row(0, &record).unwrap()).unwrap()],
                }),
            })
            .unwrap();
        writer
    }

    #[test]
    fn imports_v07_state_once_after_smelt_07_closes() {
        let dir = tempfile::tempdir().unwrap();
        let (state, cache) = (dir.path().join("state"), dir.path().join("cache"));
        fs::create_dir_all(&cache).unwrap();
        let history = [
            HistoryItem::user(Content::text("hello")),
            HistoryItem::user(Content::text("again")),
        ];
        let writer = save_v07_session(&state.join("sessions"), &history);
        fs::write(state.join("history"), "one\x1etwo\x1e").unwrap();
        fs::write(
            state.join("metrics.jsonl"),
            "{\"timestamp_ms\":5,\"prompt_tokens\":3,\"completion_tokens\":4,\"model\":\"m\"}\nnot json\n",
        )
        .unwrap();
        fs::write(state.join("codex_auth.json"), "{}").unwrap();
        fs::write(state.join("state.json"), "{}").unwrap();
        fs::write(cache.join("codex_models.json"), "[]").unwrap();
        assert!(pending(&state, &cache));

        let error = run(&state, &cache, &mut |_, _| {}).unwrap_err();
        assert!(
            error.to_string().contains("0.7 is still running"),
            "{error}"
        );
        assert!(state.join("sessions").is_dir());
        drop(writer);

        let report = run(&state, &cache, &mut |_, _| {}).unwrap();
        assert_eq!((report.sessions, report.prompts, report.usage), (1, 2, 1));
        assert!(
            matches!(report.failures.as_slice(), [only] if only.ends_with("1 unreadable lines")),
            "{:?}",
            report.failures
        );
        assert!(!pending(&state, &cache));

        let storage = SessionStorage::new(state.clone());
        let session = storage.load_full(ID).unwrap();
        assert_eq!(session.title.as_deref(), Some("old session"));
        assert_eq!(session.history, history);
        assert_eq!(session.session_cost_usd, 1.5);
        let row = storage.reader(ID).unwrap().transcript_range(0, 1).unwrap();
        let record = TranscriptBlockRecord::try_from(row[0].clone()).unwrap();
        assert_eq!(
            record.content_hash,
            record.block.clone().normalize_content().content_hash()
        );
        assert_eq!(storage.store().prompts().unwrap(), ["one", "two"]);
        assert_eq!(storage.store().usage().unwrap()[0].calls, 1);
        assert!(state.join("auth/codex.json").is_file());
        assert!(!present(&state.join("state.json")));
        assert!(!present(&cache.join("codex_models.json")));
        assert!(state.join(ARCHIVE).join("sessions").is_dir());
        assert!(!present(&state.join(ARCHIVE).join("history")));

        let again = run(&state, &cache, &mut |_, _| {}).unwrap();
        assert_eq!((again.sessions, again.skipped, again.prompts), (0, 1, 0));
    }
}
