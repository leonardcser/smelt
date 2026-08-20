use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use smelt_buffer::text;

use crate::error::{Result, StoreError};
use crate::history::{StoredTranscriptBlock, TranscriptSearchCandidate, TranscriptSearchDirection};
use crate::lineage::{self, BranchId, LineageId, TranscriptSearchLeaf};

pub const SEARCH_FORMAT_VERSION: i32 = 5;
const SEARCH_DB_FILENAME: &str = "search.db";
const SEARCH_SEGMENT_BYTES: u64 = 2 * 1024 * 1024;
const SEARCH_SEGMENT_MAX_LEAVES: usize = 32;
const SEARCH_DOCUMENT_BYTES: usize = 32 * 1024;
const SEARCH_DOCUMENT_OVERLAP_BYTES: usize = 1024;
const SEARCH_QUERY_ANCHOR_BYTES: usize = 512;
const SEARCH_QUERY_ANCHOR_GRAMS: usize = 8;
const DIRECT_SCAN_BATCH_RECORDS: usize = 64;
const SEARCH_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
const SEARCH_SCHEMA: &str = r#"
CREATE TABLE search_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    format_version INTEGER NOT NULL,
    lineage_id TEXT NOT NULL,
    segment_bytes INTEGER NOT NULL,
    segment_leaves INTEGER NOT NULL,
    document_bytes INTEGER NOT NULL,
    overlap_bytes INTEGER NOT NULL,
    anchor_bytes INTEGER NOT NULL,
    anchor_grams INTEGER NOT NULL
) STRICT;
CREATE TABLE search_segments (
    segment_id INTEGER PRIMARY KEY,
    source_node_id TEXT NOT NULL UNIQUE,
    source_item_count INTEGER NOT NULL CHECK (source_item_count > 0),
    source_byte_count INTEGER NOT NULL CHECK (source_byte_count >= 0),
    min_block_idx INTEGER,
    max_block_idx INTEGER,
    logical_text_bytes INTEGER NOT NULL CHECK (logical_text_bytes >= 0),
    doc_count INTEGER NOT NULL CHECK (doc_count >= 0),
    first_doc_id INTEGER,
    last_doc_id INTEGER,
    complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
    CHECK ((doc_count = 0 AND first_doc_id IS NULL AND last_doc_id IS NULL)
        OR (doc_count > 0 AND first_doc_id IS NOT NULL AND last_doc_id IS NOT NULL
            AND last_doc_id >= first_doc_id)),
    CHECK ((min_block_idx IS NULL AND max_block_idx IS NULL)
        OR (min_block_idx IS NOT NULL AND max_block_idx IS NOT NULL
            AND max_block_idx >= min_block_idx))
) STRICT;
CREATE TABLE search_source_leaves (
    segment_id INTEGER NOT NULL,
    leaf_ordinal INTEGER NOT NULL CHECK (leaf_ordinal BETWEEN 0 AND 31),
    node_id TEXT NOT NULL,
    start_index INTEGER NOT NULL CHECK (start_index >= 0),
    item_count INTEGER NOT NULL CHECK (item_count > 0),
    byte_count INTEGER NOT NULL CHECK (byte_count >= 0),
    PRIMARY KEY (segment_id, leaf_ordinal),
    FOREIGN KEY (segment_id) REFERENCES search_segments(segment_id)
        ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
CREATE TABLE search_docs (
    doc_id INTEGER PRIMARY KEY,
    segment_id INTEGER NOT NULL,
    first_record_ordinal INTEGER NOT NULL CHECK (first_record_ordinal >= 0),
    last_record_ordinal INTEGER NOT NULL CHECK (last_record_ordinal >= first_record_ordinal),
    min_block_idx INTEGER NOT NULL CHECK (min_block_idx >= 0),
    max_block_idx INTEGER NOT NULL CHECK (max_block_idx >= min_block_idx),
    FOREIGN KEY (segment_id) REFERENCES search_segments(segment_id)
        ON DELETE CASCADE
) STRICT;
CREATE INDEX search_docs_segment_idx
    ON search_docs(segment_id, doc_id);
CREATE INDEX search_docs_block_idx
    ON search_docs(segment_id, min_block_idx, max_block_idx, doc_id);
CREATE VIRTUAL TABLE search_fts USING fts5(
    text,
    content='',
    detail=none,
    columnsize=0,
    tokenize='trigram'
);
CREATE TABLE search_short_postings (
    segment_id INTEGER NOT NULL,
    kind INTEGER NOT NULL CHECK (kind IN (1, 2)),
    gram_hash INTEGER NOT NULL,
    docs BLOB NOT NULL CHECK (length(docs) > 0),
    PRIMARY KEY (kind, gram_hash, segment_id),
    FOREIGN KEY (segment_id) REFERENCES search_segments(segment_id)
        ON DELETE CASCADE
) WITHOUT ROWID, STRICT;
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchProjectionState {
    Missing,
    Partial,
    Current,
    Incompatible,
    Corrupt,
}

impl SearchProjectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Partial => "partial",
            Self::Current => "current",
            Self::Incompatible => "incompatible",
            Self::Corrupt => "corrupt",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct SearchProjectionStatus {
    pub state: SearchProjectionState,
    pub format_version: Option<i32>,
    pub ready_segments: usize,
    pub total_segments: usize,
    pub database_bytes: u64,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SearchReclamationStep {
    pub(crate) segments_deleted: usize,
    pub(crate) complete: bool,
}

pub struct LineageSearchProjector {
    requested: Arc<AtomicU64>,
    completed: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl LineageSearchProjector {
    pub(crate) fn spawn(
        canonical_path: PathBuf,
        lineage: LineageId,
        branch: BranchId,
    ) -> Result<Self> {
        let requested = Arc::new(AtomicU64::new(0));
        let completed = Arc::new(AtomicU64::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let error = Arc::new(Mutex::new(None));
        let worker_requested = Arc::clone(&requested);
        let worker_completed = Arc::clone(&completed);
        let worker_stopping = Arc::clone(&stopping);
        let worker_wake = Arc::clone(&wake);
        let worker_error = Arc::clone(&error);
        let thread = thread::Builder::new()
            .name(format!(
                "smelt-search-{}",
                text::slice(branch.as_str(), 0..8)
            ))
            .spawn(move || {
                let mut handled = 0_u64;
                loop {
                    let target = worker_requested.load(Ordering::Acquire);
                    if worker_stopping.load(Ordering::Acquire) {
                        return;
                    }
                    if target == handled {
                        let guard = worker_wake
                            .0
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        let _ = worker_wake
                            .1
                            .wait_timeout(guard, Duration::from_secs(30))
                            .unwrap_or_else(|poison| poison.into_inner());
                        continue;
                    }
                    let cancelled = || {
                        worker_stopping.load(Ordering::Acquire)
                            || worker_requested.load(Ordering::Acquire) != target
                    };
                    let result =
                        project_current_branch(&canonical_path, &lineage, &branch, cancelled);
                    match result {
                        Ok(()) => {
                            *worker_error
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner()) = None;
                        }
                        Err(error) => {
                            *worker_error
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner()) =
                                Some(error.to_string());
                        }
                    }
                    handled = target;
                    worker_completed.store(target, Ordering::Release);
                }
            })
            .map_err(StoreError::Io)?;
        Ok(Self {
            requested,
            completed,
            stopping,
            wake,
            error,
            thread: Some(thread),
        })
    }

    pub fn request(&self) {
        self.requested.fetch_add(1, Ordering::AcqRel);
        self.wake.1.notify_one();
    }

    pub fn is_idle(&self) -> bool {
        self.completed.load(Ordering::Acquire) == self.requested.load(Ordering::Acquire)
    }

    pub fn latest_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.wake.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LineageSearchProjector {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[derive(Clone, Debug)]
struct SearchSegmentRow {
    segment_id: u64,
    source_node_id: String,
    source_item_count: u64,
    source_byte_count: u64,
    min_block_idx: Option<u64>,
    max_block_idx: Option<u64>,
    doc_count: u64,
    first_doc_id: Option<u64>,
    last_doc_id: Option<u64>,
}

#[derive(Clone, Debug)]
struct SearchDoc {
    doc_id: u64,
    first_record_ordinal: usize,
    last_record_ordinal: usize,
    min_block_idx: u64,
    max_block_idx: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchDocumentInput {
    first_record_ordinal: usize,
    last_record_ordinal: usize,
    min_block_idx: u64,
    max_block_idx: u64,
    text: String,
}

#[derive(Clone, Debug)]
struct SearchSourceSegment {
    id: String,
    item_count: u64,
    byte_count: u64,
    leaves: Vec<TranscriptSearchLeaf>,
}

fn never_cancelled() -> bool {
    false
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(StoreError::Cancelled)
    } else {
        Ok(())
    }
}

fn search_source_segments(
    leaves: Vec<TranscriptSearchLeaf>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<SearchSourceSegment>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    let mut current_bytes = 0_u64;
    for leaf in leaves {
        check_cancelled(cancelled)?;
        let combined_bytes = current_bytes.checked_add(leaf.byte_count).ok_or_else(|| {
            StoreError::Integrity("derived search source byte count overflow".into())
        })?;
        if !current.is_empty()
            && (current.len() == SEARCH_SEGMENT_MAX_LEAVES || combined_bytes > SEARCH_SEGMENT_BYTES)
        {
            segments.push(finish_search_source_segment(current)?);
            current = Vec::new();
            current_bytes = 0;
        }
        current_bytes = current_bytes.checked_add(leaf.byte_count).ok_or_else(|| {
            StoreError::Integrity("derived search source byte count overflow".into())
        })?;
        current.push(leaf);
    }
    if !current.is_empty() {
        segments.push(finish_search_source_segment(current)?);
    }
    Ok(segments)
}

fn finish_search_source_segment(leaves: Vec<TranscriptSearchLeaf>) -> Result<SearchSourceSegment> {
    let first = leaves
        .first()
        .ok_or_else(|| StoreError::Integrity("derived search source is empty".into()))?;
    let start_index = first.start_index;
    let mut item_count = 0_u64;
    let mut byte_count = 0_u64;
    let mut hash = Sha256::new();
    hash.update(b"smelt-lineage-search-source-v2\0");
    for leaf in &leaves {
        if leaf.start_index != start_index.saturating_add(item_count) {
            return Err(StoreError::Integrity(
                "derived search source leaves are not contiguous".into(),
            ));
        }
        item_count = item_count.checked_add(leaf.item_count).ok_or_else(|| {
            StoreError::Integrity("derived search source item count overflow".into())
        })?;
        byte_count = byte_count.checked_add(leaf.byte_count).ok_or_else(|| {
            StoreError::Integrity("derived search source byte count overflow".into())
        })?;
        hash.update(leaf.node_id.as_bytes());
    }
    Ok(SearchSourceSegment {
        id: crate::object::hex_lower(&hash.finalize()),
        item_count,
        byte_count,
        leaves,
    })
}

fn search_source_records(
    canonical: &Connection,
    lineage: &LineageId,
    source: &SearchSourceSegment,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<StoredTranscriptBlock>> {
    let capacity = usize::try_from(source.item_count)
        .map_err(|_| StoreError::Integrity("derived search source is too large".into()))?;
    let mut records = Vec::with_capacity(capacity);
    for leaf in &source.leaves {
        check_cancelled(cancelled)?;
        records.extend(lineage::lineage_transcript_search_leaf_records(
            canonical,
            lineage,
            &leaf.node_id,
            cancelled,
        )?);
    }
    check_cancelled(cancelled)?;
    if records.len() != capacity {
        return Err(StoreError::Integrity(format!(
            "derived search source {} reconstructed {} records for declared count {}",
            source.id,
            records.len(),
            source.item_count
        )));
    }
    Ok(records)
}

fn search_source_records_at(
    canonical: &Connection,
    lineage: &LineageId,
    source: &SearchSourceSegment,
    ordinals: &[usize],
    cancelled: &dyn Fn() -> bool,
) -> Result<HashMap<usize, StoredTranscriptBlock>> {
    let mut ordinals = ordinals.to_vec();
    ordinals.sort_unstable();
    ordinals.dedup();
    if ordinals
        .last()
        .is_some_and(|ordinal| *ordinal as u64 >= source.item_count)
    {
        return Err(StoreError::Integrity(format!(
            "derived search source {} references a missing record",
            source.id
        )));
    }

    let mut records = HashMap::with_capacity(ordinals.len());
    let mut next_ordinal = 0;
    let mut leaf_start = 0_u64;
    for leaf in &source.leaves {
        check_cancelled(cancelled)?;
        let leaf_end = leaf_start.checked_add(leaf.item_count).ok_or_else(|| {
            StoreError::Integrity("derived search leaf item count overflow".into())
        })?;
        let mut local_ordinals = Vec::new();
        while let Some(ordinal) = ordinals.get(next_ordinal).copied() {
            let ordinal = ordinal as u64;
            if ordinal >= leaf_end {
                break;
            }
            let local_ordinal = usize::try_from(ordinal.saturating_sub(leaf_start))
                .map_err(|_| StoreError::Integrity("search record ordinal overflow".into()))?;
            local_ordinals.push(local_ordinal);
            next_ordinal += 1;
        }
        if !local_ordinals.is_empty() {
            for (local_ordinal, record) in lineage::lineage_transcript_search_leaf_records_at(
                canonical,
                lineage,
                &leaf.node_id,
                &local_ordinals,
                cancelled,
            )? {
                let local_ordinal = u64::try_from(local_ordinal)
                    .map_err(|_| StoreError::Integrity("search record ordinal overflow".into()))?;
                let global_ordinal =
                    usize::try_from(leaf_start.checked_add(local_ordinal).ok_or_else(|| {
                        StoreError::Integrity("search record ordinal overflow".into())
                    })?)
                    .map_err(|_| StoreError::Integrity("search record ordinal overflow".into()))?;
                records.insert(global_ordinal, record);
            }
        }
        leaf_start = leaf_end;
    }
    if leaf_start != source.item_count || records.len() != ordinals.len() {
        return Err(StoreError::Integrity(format!(
            "derived search source {} reconstructed the wrong records",
            source.id
        )));
    }
    Ok(records)
}

fn search_document_inputs(records: &[StoredTranscriptBlock]) -> Result<Vec<SearchDocumentInput>> {
    let mut documents = Vec::new();
    let mut pending: Option<SearchDocumentInput> = None;

    for (record_ordinal, record) in records.iter().enumerate() {
        let record_text = record.indexed_text.as_str();
        if record_text.is_empty() {
            if let Some(document) = &mut pending {
                document.last_record_ordinal = record_ordinal;
                document.min_block_idx = document.min_block_idx.min(record.block_idx);
                document.max_block_idx = document.max_block_idx.max(record.block_idx);
            }
            continue;
        }

        if record_text.len() > SEARCH_DOCUMENT_BYTES {
            if let Some(document) = pending.take() {
                documents.push(document);
            }
            let mut core_start = 0;
            while core_start < record_text.len() {
                let desired_end = core_start
                    .saturating_add(SEARCH_DOCUMENT_BYTES)
                    .min(record_text.len());
                let mut core_end = text::snap(record_text, desired_end);
                if core_end <= core_start {
                    core_end = text::next_char_boundary(record_text, desired_end);
                }
                if core_end <= core_start {
                    return Err(StoreError::Integrity(
                        "search document boundary did not make progress".into(),
                    ));
                }
                let extended_end = text::snap(
                    record_text,
                    core_end
                        .saturating_add(SEARCH_DOCUMENT_OVERLAP_BYTES)
                        .min(record_text.len()),
                );
                documents.push(SearchDocumentInput {
                    first_record_ordinal: record_ordinal,
                    last_record_ordinal: record_ordinal,
                    min_block_idx: record.block_idx,
                    max_block_idx: record.block_idx,
                    text: text::slice(record_text, core_start..extended_end).to_string(),
                });
                core_start = core_end;
            }
            continue;
        }

        let would_overflow = pending.as_ref().is_some_and(|document| {
            document
                .text
                .len()
                .saturating_add(1)
                .saturating_add(record_text.len())
                > SEARCH_DOCUMENT_BYTES
        });
        if would_overflow {
            documents.push(pending.take().expect("checked pending search document"));
        }
        match &mut pending {
            Some(document) => {
                document.text.push('\n');
                document.text.push_str(record_text);
                document.last_record_ordinal = record_ordinal;
                document.min_block_idx = document.min_block_idx.min(record.block_idx);
                document.max_block_idx = document.max_block_idx.max(record.block_idx);
            }
            None => {
                pending = Some(SearchDocumentInput {
                    first_record_ordinal: record_ordinal,
                    last_record_ordinal: record_ordinal,
                    min_block_idx: record.block_idx,
                    max_block_idx: record.block_idx,
                    text: record_text.to_string(),
                });
            }
        }
    }

    if let Some(document) = pending {
        documents.push(document);
    }
    Ok(documents)
}

fn project_current_branch(
    canonical_path: &Path,
    lineage: &LineageId,
    branch: &BranchId,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    let canonical = open_canonical_reader(canonical_path)?;
    let (_, leaves) = match lineage::lineage_transcript_search_leaves_with_cancellation(
        &canonical, lineage, branch, &cancelled,
    ) {
        Err(StoreError::Cancelled) => return Ok(()),
        result => result?,
    };
    let sources = match search_source_segments(leaves, &cancelled) {
        Err(StoreError::Cancelled) => return Ok(()),
        result => result?,
    };
    if cancelled() {
        return Ok(());
    }
    let search_path = search_database_path(canonical_path)?;
    let mut search = open_search_writer(&search_path, lineage)?;
    for source in sources {
        if cancelled() {
            return Ok(());
        }
        match search_segment_row(&search, &source.id) {
            Ok(Some(row))
                if search_segment_matches_source(&row, &source)
                    && validate_search_segment_structure(&search, &row).is_ok() =>
            {
                continue;
            }
            Ok(None) => {}
            Ok(Some(_)) | Err(_) => {
                reset_search_database(&search_path)?;
                search = open_search_writer(&search_path, lineage)?;
            }
        }
        let records = match search_source_records(&canonical, lineage, &source, &cancelled) {
            Err(StoreError::Cancelled) => return Ok(()),
            result => result?,
        };
        if cancelled() {
            return Ok(());
        }
        if !build_search_segment(&mut search, &source, &records, &cancelled)? {
            return Ok(());
        }
    }
    Ok(())
}

fn reachable_search_source_ids(
    canonical: &Connection,
    lineage: &LineageId,
) -> Result<HashSet<String>> {
    let mut branches = canonical.prepare(
        "SELECT session_id FROM lineage_branches
         WHERE lineage_id = ?1 AND deleted_at IS NULL
         ORDER BY session_id",
    )?;
    let branch_ids = branches
        .query_map([lineage.as_str()], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(branches);

    let mut reachable = HashSet::new();
    for branch_id in branch_ids {
        let branch = BranchId::new(branch_id)?;
        let (_, leaves) = lineage::lineage_transcript_search_leaves(canonical, lineage, &branch)?;
        for source in search_source_segments(leaves, &never_cancelled)? {
            reachable.insert(source.id);
        }
    }
    Ok(reachable)
}

pub(crate) fn reclaim_one_obsolete_search_segment(
    canonical: &Connection,
    canonical_path: &Path,
    lineage: &LineageId,
) -> Result<SearchReclamationStep> {
    let search_path = search_database_path(canonical_path)?;
    reject_symlink(&search_path)?;
    if !search_path.exists() {
        return Ok(SearchReclamationStep {
            complete: true,
            ..SearchReclamationStep::default()
        });
    }

    let reachable = reachable_search_source_ids(canonical, lineage)?;
    let result = (|| -> Result<SearchReclamationStep> {
        let mut search = open_search_writer(&search_path, lineage)?;
        let target = {
            let mut segments = search.prepare(
                "SELECT segment_id, source_node_id, source_item_count, source_byte_count,
                    min_block_idx, max_block_idx, doc_count, first_doc_id, last_doc_id
             FROM search_segments
             WHERE complete = 1
             ORDER BY segment_id",
            )?;
            let rows = segments.query_map([], decode_search_segment_row)?;
            let mut target = None;
            for row in rows {
                let row = row?;
                validate_search_segment_structure(&search, &row)?;
                if !reachable.contains(&row.source_node_id) {
                    target = Some(row);
                    break;
                }
            }
            target
        };
        let Some(target) = target else {
            return Ok(SearchReclamationStep {
                complete: true,
                ..SearchReclamationStep::default()
            });
        };
        let source_records = match search_segment_source(&search, &target)
            .and_then(|source| search_source_records(canonical, lineage, &source, &never_cancelled))
        {
            Ok(records) => records,
            Err(_) => {
                drop(search);
                reset_search_database(&search_path)?;
                return Ok(SearchReclamationStep {
                    complete: true,
                    ..SearchReclamationStep::default()
                });
            }
        };

        let source_documents = search_document_inputs(&source_records)?;
        let tx = search.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut docs = tx.prepare(
                "SELECT doc_id, first_record_ordinal, last_record_ordinal,
                        min_block_idx, max_block_idx
                 FROM search_docs
                 WHERE segment_id = ?1
                 ORDER BY doc_id",
            )?;
            let rows = docs
                .query_map(
                    [sql_i64(target.segment_id, "search segment ID")?],
                    decode_search_doc,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if rows.len() != source_documents.len() {
                return Err(StoreError::Integrity(
                    "derived search document count does not match its source".into(),
                ));
            }
            let mut delete_fts = tx.prepare(
                "INSERT INTO search_fts(search_fts, rowid, text)
                 VALUES ('delete', ?1, ?2)",
            )?;
            for (doc, source) in rows.into_iter().zip(source_documents) {
                validate_search_doc(
                    &doc,
                    source.first_record_ordinal,
                    source.last_record_ordinal,
                    source.min_block_idx,
                    source.max_block_idx,
                    &target,
                )?;
                delete_fts.execute(params![
                    sql_i64(doc.doc_id, "search document ID")?,
                    source.text
                ])?;
            }
        }
        let deleted_segments = tx.execute(
            "DELETE FROM search_segments WHERE segment_id = ?1",
            [sql_i64(target.segment_id, "search segment ID")?],
        )?;
        if deleted_segments != 1 {
            return Err(StoreError::Integrity(
                "derived search reclamation did not delete its target segment".into(),
            ));
        }
        tx.commit()?;
        search.execute_batch("PRAGMA incremental_vacuum(256);")?;

        let complete = {
            let mut segments = search.prepare("SELECT source_node_id FROM search_segments")?;
            let rows = segments.query_map([], |row| row.get::<_, String>(0))?;
            let mut complete = true;
            for source_id in rows {
                if !reachable.contains(&source_id?) {
                    complete = false;
                    break;
                }
            }
            complete
        };
        Ok(SearchReclamationStep {
            segments_deleted: 1,
            complete,
        })
    })();
    match result {
        Ok(step) => Ok(step),
        Err(_) => {
            reset_search_database(&search_path)?;
            Ok(SearchReclamationStep {
                complete: true,
                ..SearchReclamationStep::default()
            })
        }
    }
}

fn open_canonical_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(SEARCH_BUSY_TIMEOUT)?;
    conn.pragma_update(None, "query_only", "ON")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

fn search_database_path(canonical_path: &Path) -> Result<PathBuf> {
    canonical_path
        .parent()
        .map(|directory| directory.join(SEARCH_DB_FILENAME))
        .ok_or_else(|| StoreError::Integrity("lineage database has no directory".into()))
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(StoreError::Integrity(format!(
            "refusing derived search symlink {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::Io(error)),
    }
}

fn configure_search_writer(conn: &Connection, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    conn.busy_timeout(SEARCH_BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

fn create_search_schema(conn: &Connection, lineage: &LineageId) -> Result<()> {
    conn.execute_batch(SEARCH_SCHEMA)?;
    conn.execute(
        "INSERT INTO search_meta (
             singleton, format_version, lineage_id, segment_bytes, segment_leaves,
             document_bytes, overlap_bytes, anchor_bytes, anchor_grams
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            SEARCH_FORMAT_VERSION,
            lineage.as_str(),
            sql_i64(SEARCH_SEGMENT_BYTES, "search segment bytes")?,
            sql_i64(SEARCH_SEGMENT_MAX_LEAVES, "search segment leaves")?,
            sql_i64(SEARCH_DOCUMENT_BYTES, "search document bytes")?,
            sql_i64(
                SEARCH_DOCUMENT_OVERLAP_BYTES,
                "search document overlap bytes"
            )?,
            sql_i64(SEARCH_QUERY_ANCHOR_BYTES, "search query anchor bytes")?,
            sql_i64(SEARCH_QUERY_ANCHOR_GRAMS, "search query anchor grams")?,
        ],
    )?;
    conn.pragma_update(None, "user_version", SEARCH_FORMAT_VERSION)?;
    Ok(())
}

fn validate_search_meta(conn: &Connection, lineage: &LineageId) -> Result<()> {
    let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SEARCH_FORMAT_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            expected: SEARCH_FORMAT_VERSION,
        });
    }
    let row = conn
        .query_row(
            "SELECT format_version, lineage_id, segment_bytes, segment_leaves,
                    document_bytes, overlap_bytes, anchor_bytes, anchor_grams
             FROM search_meta WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::Integrity("derived search metadata is missing".into()))?;
    if row.0 != SEARCH_FORMAT_VERSION
        || row.1 != lineage.as_str()
        || nonnegative_usize(row.2, "search segment bytes")? != SEARCH_SEGMENT_BYTES as usize
        || nonnegative_usize(row.3, "search segment leaves")? != SEARCH_SEGMENT_MAX_LEAVES
        || nonnegative_usize(row.4, "search document bytes")? != SEARCH_DOCUMENT_BYTES
        || nonnegative_usize(row.5, "search document overlap bytes")?
            != SEARCH_DOCUMENT_OVERLAP_BYTES
        || nonnegative_usize(row.6, "search query anchor bytes")? != SEARCH_QUERY_ANCHOR_BYTES
        || nonnegative_usize(row.7, "search query anchor grams")? != SEARCH_QUERY_ANCHOR_GRAMS
    {
        return Err(StoreError::Integrity(
            "derived search metadata does not match this format".into(),
        ));
    }
    let source_leaves_table: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_schema
             WHERE type = 'table' AND name = 'search_source_leaves'
         )",
        [],
        |row| row.get(0),
    )?;
    if !source_leaves_table {
        return Err(StoreError::Integrity(
            "derived search source-leaf mapping is missing".into(),
        ));
    }
    Ok(())
}

fn search_quick_check(conn: &Connection) -> Result<()> {
    let result: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(StoreError::Integrity(format!(
            "derived search quick_check failed: {result}"
        )));
    }
    Ok(())
}

fn open_search_writer(path: &Path, lineage: &LineageId) -> Result<Connection> {
    reject_symlink(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        match Connection::open(path)
            .map_err(StoreError::from)
            .and_then(|conn| {
                configure_search_writer(&conn, path)?;
                validate_search_meta(&conn, lineage)?;
                search_quick_check(&conn)?;
                let incomplete: bool = conn.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM search_segments segment
                         WHERE segment.complete != 1
                            OR NOT EXISTS (
                                SELECT 1 FROM search_source_leaves leaf
                                WHERE leaf.segment_id = segment.segment_id
                            )
                     )",
                    [],
                    |row| row.get(0),
                )?;
                if incomplete {
                    return Err(StoreError::Integrity(
                        "derived search contains an incomplete segment installation".into(),
                    ));
                }
                Ok(conn)
            }) {
            Ok(conn) => return Ok(conn),
            Err(_) => reset_search_database(path)?,
        }
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    configure_search_writer(&conn, path)?;
    create_search_schema(&conn, lineage)?;
    Ok(conn)
}

fn remove_search_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::Io(error)),
    }
}

fn reset_search_database(path: &Path) -> Result<()> {
    reject_symlink(path)?;
    remove_search_file(path)?;
    remove_search_file(&PathBuf::from(format!("{}-wal", path.display())))?;
    remove_search_file(&PathBuf::from(format!("{}-shm", path.display())))?;
    Ok(())
}

fn build_search_segment(
    conn: &mut Connection,
    source: &SearchSourceSegment,
    records: &[StoredTranscriptBlock],
    cancelled: &impl Fn() -> bool,
) -> Result<bool> {
    if records.len() as u64 != source.item_count || records.is_empty() {
        return Err(StoreError::Integrity(format!(
            "search source {} reconstructed {} records for declared count {}",
            source.id,
            records.len(),
            source.item_count
        )));
    }
    let documents = search_document_inputs(records)?;

    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO search_segments (
             source_node_id, source_item_count, source_byte_count,
             min_block_idx, max_block_idx, logical_text_bytes, doc_count,
             first_doc_id, last_doc_id, complete
         ) VALUES (?1, ?2, ?3, NULL, NULL, 0, 0, NULL, NULL, 0)",
        params![
            source.id,
            sql_i64(source.item_count, "search segment item count")?,
            sql_i64(source.byte_count, "search segment byte count")?,
        ],
    )?;
    let segment_id = u64::try_from(tx.last_insert_rowid())
        .map_err(|_| StoreError::Integrity("search segment ID is negative".into()))?;
    {
        let mut insert_leaf = tx.prepare(
            "INSERT INTO search_source_leaves (
                 segment_id, leaf_ordinal, node_id, start_index, item_count, byte_count
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for (ordinal, leaf) in source.leaves.iter().enumerate() {
            insert_leaf.execute(params![
                sql_i64(segment_id, "search segment ID")?,
                sql_i64(ordinal, "search leaf ordinal")?,
                leaf.node_id.as_str(),
                sql_i64(leaf.start_index, "search leaf start index")?,
                sql_i64(leaf.item_count, "search leaf item count")?,
                sql_i64(leaf.byte_count, "search leaf byte count")?,
            ])?;
        }
    }
    let mut next_doc_id = tx.query_row(
        "SELECT coalesce(max(doc_id), 0) + 1 FROM search_docs",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let first_doc_id = u64::try_from(next_doc_id)
        .map_err(|_| StoreError::Integrity("search document ID is negative".into()))?;
    let mut doc_count = 0_u64;
    let logical_text_bytes = records.iter().fold(0_u64, |total, record| {
        total.saturating_add(record.indexed_text.len() as u64)
    });
    let mut postings: BTreeMap<(u8, u64), Vec<u64>> = BTreeMap::new();

    {
        let mut insert_doc = tx.prepare(
            "INSERT INTO search_docs (
                 doc_id, segment_id, first_record_ordinal, last_record_ordinal,
                 min_block_idx, max_block_idx
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut insert_fts = tx.prepare("INSERT INTO search_fts(rowid, text) VALUES (?1, ?2)")?;
        for document in documents {
            if cancelled() {
                return Ok(false);
            }
            let doc_id = u64::try_from(next_doc_id)
                .map_err(|_| StoreError::Integrity("search document ID is negative".into()))?;
            insert_doc.execute(params![
                next_doc_id,
                sql_i64(segment_id, "search segment ID")?,
                sql_i64(document.first_record_ordinal, "search first record ordinal")?,
                sql_i64(document.last_record_ordinal, "search last record ordinal")?,
                sql_i64(document.min_block_idx, "search minimum block index")?,
                sql_i64(document.max_block_idx, "search maximum block index")?,
            ])?;
            insert_fts.execute(params![next_doc_id, document.text])?;
            let mut chars = HashSet::new();
            let mut bigrams = HashSet::new();
            collect_short_hashes(&document.text, &mut chars, &mut bigrams);
            for hash in chars {
                postings.entry((1, hash)).or_default().push(doc_id);
            }
            for hash in bigrams {
                postings.entry((2, hash)).or_default().push(doc_id);
            }
            next_doc_id = next_doc_id
                .checked_add(1)
                .ok_or_else(|| StoreError::Integrity("search document ID overflow".into()))?;
            doc_count = doc_count.saturating_add(1);
        }
    }

    {
        let mut insert_posting = tx.prepare(
            "INSERT INTO search_short_postings (
                 segment_id, kind, gram_hash, docs
             ) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for ((kind, hash), doc_ids) in postings {
            let packed = pack_doc_ids(&doc_ids)?;
            insert_posting.execute(params![
                sql_i64(segment_id, "search segment ID")?,
                kind,
                hash as i64,
                packed
            ])?;
        }
    }
    if cancelled() {
        return Ok(false);
    }
    let last_doc_id = doc_count
        .checked_sub(1)
        .map(|offset| first_doc_id.saturating_add(offset));
    let min_block_idx = records
        .iter()
        .map(|record| record.block_idx)
        .min()
        .expect("search segment records");
    let max_block_idx = records
        .iter()
        .map(|record| record.block_idx)
        .max()
        .expect("search segment records");
    let first_doc_id_sql = (doc_count > 0)
        .then(|| sql_i64(first_doc_id, "search first document ID"))
        .transpose()?;
    let last_doc_id_sql = last_doc_id
        .map(|id| sql_i64(id, "search last document ID"))
        .transpose()?;
    tx.execute(
        "UPDATE search_segments
         SET min_block_idx = ?2, max_block_idx = ?3,
             logical_text_bytes = ?4, doc_count = ?5,
             first_doc_id = ?6, last_doc_id = ?7, complete = 1
         WHERE source_node_id = ?1 AND complete = 0",
        params![
            source.id,
            sql_i64(min_block_idx, "search minimum block index")?,
            sql_i64(max_block_idx, "search maximum block index")?,
            sql_i64(logical_text_bytes, "search logical text bytes")?,
            sql_i64(doc_count, "search document count")?,
            first_doc_id_sql,
            last_doc_id_sql,
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

fn search_segment_matches_source(row: &SearchSegmentRow, source: &SearchSourceSegment) -> bool {
    row.source_node_id == source.id
        && row.source_item_count == source.item_count
        && row.source_byte_count == source.byte_count
}

fn search_segment_row(conn: &Connection, node_id: &str) -> Result<Option<SearchSegmentRow>> {
    conn.query_row(
        "SELECT segment_id, source_node_id, source_item_count, source_byte_count,
                min_block_idx, max_block_idx, doc_count, first_doc_id, last_doc_id
         FROM search_segments
         WHERE source_node_id = ?1 AND complete = 1",
        [node_id],
        decode_search_segment_row,
    )
    .optional()
    .map_err(StoreError::from)
}

fn search_segment_rows(
    conn: &Connection,
    sources: &[SearchSourceSegment],
) -> Result<HashMap<String, SearchSegmentRow>> {
    const SEGMENT_BATCH: usize = 500;

    let mut segments = HashMap::with_capacity(sources.len());
    for batch in sources.chunks(SEGMENT_BATCH) {
        let placeholders = (1..=batch.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT segment_id, source_node_id, source_item_count, source_byte_count,
                    min_block_idx, max_block_idx, doc_count, first_doc_id, last_doc_id
             FROM search_segments
             WHERE complete = 1 AND source_node_id IN ({placeholders})"
        );
        let parameters = batch
            .iter()
            .map(|source| rusqlite::types::Value::Text(source.id.clone()))
            .collect::<Vec<_>>();
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(parameters),
            decode_search_segment_row,
        )?;
        for row in rows {
            let row = row?;
            segments.insert(row.source_node_id.clone(), row);
        }
    }
    Ok(segments)
}

fn decode_search_segment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchSegmentRow> {
    Ok(SearchSegmentRow {
        segment_id: row_nonnegative_u64(row, 0)?,
        source_node_id: row.get(1)?,
        source_item_count: row_nonnegative_u64(row, 2)?,
        source_byte_count: row_nonnegative_u64(row, 3)?,
        min_block_idx: row_optional_nonnegative_u64(row, 4)?,
        max_block_idx: row_optional_nonnegative_u64(row, 5)?,
        doc_count: row_nonnegative_u64(row, 6)?,
        first_doc_id: row_optional_nonnegative_u64(row, 7)?,
        last_doc_id: row_optional_nonnegative_u64(row, 8)?,
    })
}

fn search_segment_source(
    conn: &Connection,
    segment: &SearchSegmentRow,
) -> Result<SearchSourceSegment> {
    let mut statement = conn.prepare(
        "SELECT leaf_ordinal, node_id, start_index, item_count, byte_count
         FROM search_source_leaves
         WHERE segment_id = ?1
         ORDER BY leaf_ordinal",
    )?;
    let rows = statement.query_map([sql_i64(segment.segment_id, "search segment ID")?], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let mut leaves = Vec::new();
    for row in rows {
        let (ordinal, node_id, start_index, item_count, byte_count) = row?;
        if nonnegative_usize(ordinal, "search leaf ordinal")? != leaves.len() {
            return Err(StoreError::Integrity(format!(
                "derived search segment {} has noncontiguous source leaves",
                segment.source_node_id
            )));
        }
        leaves.push(TranscriptSearchLeaf {
            node_id,
            start_index: nonnegative_u64(start_index, "search leaf start index")?,
            item_count: nonnegative_u64(item_count, "search leaf item count")?,
            byte_count: nonnegative_u64(byte_count, "search leaf byte count")?,
        });
    }
    let source = finish_search_source_segment(leaves)?;
    if !search_segment_matches_source(segment, &source) {
        return Err(StoreError::Integrity(format!(
            "derived search segment {} has inconsistent source leaves",
            segment.source_node_id
        )));
    }
    Ok(source)
}

fn validate_search_segment_structure(conn: &Connection, segment: &SearchSegmentRow) -> Result<()> {
    search_segment_source(conn, segment)?;
    let (doc_count, first_doc_id, last_doc_id, max_ordinal) = conn.query_row(
        "SELECT COUNT(*), MIN(doc_id), MAX(doc_id), MAX(last_record_ordinal)
         FROM search_docs WHERE segment_id = ?1",
        [sql_i64(segment.segment_id, "search segment ID")?],
        |row| {
            Ok((
                row_nonnegative_u64(row, 0)?,
                row_optional_nonnegative_u64(row, 1)?,
                row_optional_nonnegative_u64(row, 2)?,
                row_optional_nonnegative_u64(row, 3)?,
            ))
        },
    )?;
    if doc_count != segment.doc_count
        || first_doc_id != segment.first_doc_id
        || last_doc_id != segment.last_doc_id
        || max_ordinal.is_some_and(|ordinal| ordinal >= segment.source_item_count)
        || first_doc_id
            .zip(last_doc_id)
            .is_some_and(|(first, last)| last.saturating_sub(first).saturating_add(1) != doc_count)
    {
        return Err(StoreError::Integrity(format!(
            "derived search segment {} has inconsistent document metadata",
            segment.source_node_id
        )));
    }

    let mut postings =
        conn.prepare("SELECT docs FROM search_short_postings WHERE segment_id = ?1")?;
    let rows = postings.query_map([sql_i64(segment.segment_id, "search segment ID")?], |row| {
        row.get::<_, Vec<u8>>(0)
    })?;
    for packed in rows {
        for doc_id in unpack_doc_ids(&packed?)? {
            if segment
                .first_doc_id
                .zip(segment.last_doc_id)
                .is_none_or(|(first, last)| doc_id < first || doc_id > last)
            {
                return Err(StoreError::Integrity(format!(
                    "derived search segment {} has an invalid short posting",
                    segment.source_node_id
                )));
            }
        }
    }
    Ok(())
}

fn open_search_reader(path: &Path, lineage: &LineageId) -> Result<Option<Connection>> {
    reject_symlink(path)?;
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(SEARCH_BUSY_TIMEOUT)?;
    conn.pragma_update(None, "query_only", "ON")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "cache_size", -2048)?;
    validate_search_meta(&conn, lineage)?;
    Ok(Some(conn))
}

struct CandidateSearch<'a> {
    canonical: &'a Connection,
    lineage: &'a LineageId,
    query: &'a str,
    origin_block_idx: Option<u64>,
    direction: TranscriptSearchDirection,
    limit: usize,
    cancelled: &'a dyn Fn() -> bool,
}

impl CandidateSearch<'_> {
    fn check_cancelled(&self) -> Result<()> {
        check_cancelled(self.cancelled)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn search_transcript_candidate_page(
    canonical: &Connection,
    canonical_path: &Path,
    lineage: &LineageId,
    branch: &BranchId,
    query: &str,
    origin_block_idx: Option<u64>,
    direction: TranscriptSearchDirection,
    limit: usize,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<TranscriptSearchCandidate>> {
    let _perf = smelt_perf::perf::begin("store:lineage:derived_search_candidates");
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let request = CandidateSearch {
        canonical,
        lineage,
        query,
        origin_block_idx,
        direction,
        limit,
        cancelled,
    };
    request.check_cancelled()?;
    let (_, leaves) = lineage::lineage_transcript_search_leaves_with_cancellation(
        canonical, lineage, branch, cancelled,
    )?;
    request.check_cancelled()?;
    let sources = search_source_segments(leaves, cancelled)?;
    let search_path = search_database_path(canonical_path)?;
    let search = open_search_reader(&search_path, lineage).ok().flatten();
    let mut segments = search
        .as_ref()
        .and_then(|search| search_segment_rows(search, &sources).ok())
        .unwrap_or_default();
    let mut plans = Vec::with_capacity(sources.len());
    for source in sources {
        request.check_cancelled()?;
        let segment = segments
            .remove(&source.id)
            .filter(|segment| search_segment_matches_source(segment, &source));
        plans.push((source, segment));
    }
    plans.sort_by(|(_, left), (_, right)| match request.direction {
        TranscriptSearchDirection::Forward => left
            .as_ref()
            .and_then(|segment| segment.min_block_idx)
            .unwrap_or(0)
            .cmp(
                &right
                    .as_ref()
                    .and_then(|segment| segment.min_block_idx)
                    .unwrap_or(0),
            ),
        TranscriptSearchDirection::Backward => right
            .as_ref()
            .and_then(|segment| segment.max_block_idx)
            .unwrap_or(u64::MAX)
            .cmp(
                &left
                    .as_ref()
                    .and_then(|segment| segment.max_block_idx)
                    .unwrap_or(u64::MAX),
            ),
    });

    request.check_cancelled()?;
    let output = if query.chars().count() >= 3 {
        search_long_candidate_page(&request, search.as_ref(), plans)?
    } else {
        search_short_candidate_page(&request, search.as_ref(), plans)?
    };
    smelt_perf::perf::record_value(
        "store:lineage:derived_search_candidates_loaded",
        output.len() as u64,
    );
    Ok(output)
}

fn search_long_candidate_page(
    request: &CandidateSearch<'_>,
    search: Option<&Connection>,
    plans: Vec<(SearchSourceSegment, Option<SearchSegmentRow>)>,
) -> Result<Vec<TranscriptSearchCandidate>> {
    let mut projected = Vec::new();
    let mut direct = Vec::new();
    for (source, segment) in plans {
        match (search, segment) {
            (Some(_), Some(segment)) => projected.push((source, segment)),
            _ => direct.push(source),
        }
    }

    let mut output = Vec::new();
    if let Some(search) = search {
        match search_ready_fts_sources(request, search, &projected) {
            Ok(candidates) => output = candidates,
            Err(StoreError::Cancelled) => return Err(StoreError::Cancelled),
            Err(_) => direct.extend(projected.into_iter().map(|(source, _)| source)),
        }
    }
    for source in direct {
        request.check_cancelled()?;
        output.extend(direct_search_segment(request, &source)?);
        trim_candidate_page(&mut output, request.direction, request.limit);
    }
    trim_candidate_page(&mut output, request.direction, request.limit);
    Ok(output)
}

fn search_short_candidate_page(
    request: &CandidateSearch<'_>,
    search: Option<&Connection>,
    plans: Vec<(SearchSourceSegment, Option<SearchSegmentRow>)>,
) -> Result<Vec<TranscriptSearchCandidate>> {
    let mut projected = Vec::new();
    let mut direct = Vec::new();
    for (source, segment) in plans {
        match (search, segment) {
            (Some(_), Some(segment)) => projected.push((source, segment)),
            _ => direct.push(source),
        }
    }

    let mut output = Vec::new();
    if let Some(search) = search {
        match search_ready_short_sources(request, search, &projected) {
            Ok(candidates) => output = candidates,
            Err(StoreError::Cancelled) => return Err(StoreError::Cancelled),
            Err(_) => direct.extend(projected.into_iter().map(|(source, _)| source)),
        }
    }
    for source in direct {
        request.check_cancelled()?;
        output.extend(direct_search_segment(request, &source)?);
        trim_candidate_page(&mut output, request.direction, request.limit);
    }
    trim_candidate_page(&mut output, request.direction, request.limit);
    Ok(output)
}

fn search_ready_short_sources(
    request: &CandidateSearch<'_>,
    search: &Connection,
    sources: &[(SearchSourceSegment, SearchSegmentRow)],
) -> Result<Vec<TranscriptSearchCandidate>> {
    const SEGMENT_BATCH: usize = 500;

    let scalars = request
        .query
        .chars()
        .map(|ch| ch as u32)
        .collect::<Vec<_>>();
    if scalars.is_empty() || scalars.len() > 2 {
        return Ok(Vec::new());
    }
    let kind = scalars.len() as u8;
    let hash = hash_scalars(kind, &scalars);
    let order = match request.direction {
        TranscriptSearchDirection::Forward => "ASC",
        TranscriptSearchDirection::Backward => "DESC",
    };
    let bound_column = match request.direction {
        TranscriptSearchDirection::Forward => "s.min_block_idx",
        TranscriptSearchDirection::Backward => "s.max_block_idx",
    };
    let doc_bound_column = match request.direction {
        TranscriptSearchDirection::Forward => "min_block_idx",
        TranscriptSearchDirection::Backward => "max_block_idx",
    };
    let mut output: Vec<TranscriptSearchCandidate> = Vec::new();
    let mut seen = HashSet::new();
    for batch in sources.chunks(SEGMENT_BATCH) {
        request.check_cancelled()?;
        let placeholders = (0..batch.len())
            .map(|index| format!("?{}", index + 3))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT p.segment_id, p.docs, s.min_block_idx, s.max_block_idx
             FROM search_short_postings p
             JOIN search_segments s ON s.segment_id = p.segment_id
             WHERE p.kind = ?1 AND p.gram_hash = ?2
               AND p.segment_id IN ({placeholders})
             ORDER BY {bound_column} {order}"
        );
        let mut parameters = Vec::with_capacity(batch.len() + 2);
        parameters.push(rusqlite::types::Value::Integer(i64::from(kind)));
        parameters.push(rusqlite::types::Value::Integer(hash as i64));
        for (_, segment) in batch {
            parameters.push(rusqlite::types::Value::Integer(sql_i64(
                segment.segment_id,
                "search segment ID",
            )?));
        }
        let source_by_segment = batch
            .iter()
            .enumerate()
            .map(|(index, (_, segment))| (segment.segment_id, index))
            .collect::<HashMap<_, _>>();
        let mut statement = search.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(parameters))?;
        while let Some(row) = rows.next()? {
            request.check_cancelled()?;
            let segment_id = row_nonnegative_u64(row, 0)?;
            let min_block_idx = row_optional_nonnegative_u64(row, 2)?;
            let max_block_idx = row_optional_nonnegative_u64(row, 3)?;
            if output.len() == request.limit {
                let outside_page = match request.direction {
                    TranscriptSearchDirection::Forward => min_block_idx
                        .is_some_and(|min| min > output.last().expect("search page").block_idx),
                    TranscriptSearchDirection::Backward => max_block_idx
                        .is_some_and(|max| max < output.first().expect("search page").block_idx),
                };
                if outside_page {
                    break;
                }
            }
            let Some(&source_index) = source_by_segment.get(&segment_id) else {
                return Err(StoreError::Integrity(
                    "derived short posting references an unreachable segment".into(),
                ));
            };
            let (source, segment) = &batch[source_index];
            let ids = unpack_doc_ids(&row.get::<_, Vec<u8>>(1)?)?
                .into_iter()
                .collect::<HashSet<_>>();
            let origin_filter =
                request
                    .origin_block_idx
                    .map_or_else(String::new, |_| match request.direction {
                        TranscriptSearchDirection::Forward => "AND max_block_idx >= ?2".into(),
                        TranscriptSearchDirection::Backward => "AND min_block_idx <= ?2".into(),
                    });
            let docs_sql = format!(
                "SELECT doc_id, first_record_ordinal, last_record_ordinal,
                        min_block_idx, max_block_idx
                 FROM search_docs
                 WHERE segment_id = ?1 {origin_filter}
                 ORDER BY {doc_bound_column} {order}, doc_id {order}"
            );
            let mut docs_statement = search.prepare(&docs_sql)?;
            let segment_id_sql = sql_i64(segment_id, "search segment ID")?;
            let mut docs = match request.origin_block_idx {
                Some(origin) => docs_statement.query(params![
                    segment_id_sql,
                    sql_i64(origin, "search origin block index")?
                ])?,
                None => docs_statement.query([segment_id_sql])?,
            };
            while let Some(doc_row) = docs.next()? {
                request.check_cancelled()?;
                let doc = decode_search_doc(doc_row)?;
                if !ids.contains(&doc.doc_id) {
                    continue;
                }
                if candidate_doc_is_beyond_page(&doc, &output, request) {
                    break;
                }
                append_hydrated_search_doc_matches(
                    &doc,
                    source,
                    segment,
                    request,
                    &mut seen,
                    &mut output,
                )?;
                trim_candidate_page(&mut output, request.direction, request.limit);
            }
            trim_candidate_page(&mut output, request.direction, request.limit);
        }
    }
    Ok(output)
}

fn search_ready_fts_sources(
    request: &CandidateSearch<'_>,
    search: &Connection,
    sources: &[(SearchSourceSegment, SearchSegmentRow)],
) -> Result<Vec<TranscriptSearchCandidate>> {
    const SEGMENT_BATCH: usize = 500;

    let expression = fts_anchor_expression(request.query);
    let order = match request.direction {
        TranscriptSearchDirection::Forward => "ASC",
        TranscriptSearchDirection::Backward => "DESC",
    };
    let doc_bound_column = match request.direction {
        TranscriptSearchDirection::Forward => "d.min_block_idx",
        TranscriptSearchDirection::Backward => "d.max_block_idx",
    };
    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for batch in sources.chunks(SEGMENT_BATCH) {
        request.check_cancelled()?;
        let placeholders = (0..batch.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(",");
        let origin_parameter = batch.len() + 2;
        let origin_filter =
            request
                .origin_block_idx
                .map_or_else(String::new, |_| match request.direction {
                    TranscriptSearchDirection::Forward => {
                        format!("AND d.max_block_idx >= ?{origin_parameter}")
                    }
                    TranscriptSearchDirection::Backward => {
                        format!("AND d.min_block_idx <= ?{origin_parameter}")
                    }
                });
        let sql = format!(
            "SELECT d.segment_id, d.doc_id, d.first_record_ordinal,
                    d.last_record_ordinal, d.min_block_idx, d.max_block_idx
             FROM search_fts f
             JOIN search_docs d ON d.doc_id = f.rowid
             WHERE search_fts MATCH ?1
               AND d.segment_id IN ({placeholders})
               {origin_filter}
             ORDER BY {doc_bound_column} {order}, d.doc_id {order}"
        );
        let mut parameters = Vec::with_capacity(batch.len() + 2);
        parameters.push(rusqlite::types::Value::Text(expression.clone()));
        for (_, segment) in batch {
            parameters.push(rusqlite::types::Value::Integer(sql_i64(
                segment.segment_id,
                "search segment ID",
            )?));
        }
        if let Some(origin) = request.origin_block_idx {
            parameters.push(rusqlite::types::Value::Integer(sql_i64(
                origin,
                "search origin block index",
            )?));
        }

        let source_by_segment = batch
            .iter()
            .enumerate()
            .map(|(index, (_, segment))| (segment.segment_id, index))
            .collect::<HashMap<_, _>>();
        let mut statement = search.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(parameters))?;
        while let Some(row) = rows.next()? {
            request.check_cancelled()?;
            let segment_id = row_nonnegative_u64(row, 0)?;
            let Some(&source_index) = source_by_segment.get(&segment_id) else {
                return Err(StoreError::Integrity(
                    "derived FTS candidate references an unreachable segment".into(),
                ));
            };
            let (source, segment) = &batch[source_index];
            let doc = SearchDoc {
                doc_id: row_nonnegative_u64(row, 1)?,
                first_record_ordinal: row_nonnegative_usize(row, 2)?,
                last_record_ordinal: row_nonnegative_usize(row, 3)?,
                min_block_idx: row_nonnegative_u64(row, 4)?,
                max_block_idx: row_nonnegative_u64(row, 5)?,
            };
            if candidate_doc_is_beyond_page(&doc, &output, request) {
                break;
            }
            append_hydrated_search_doc_matches(
                &doc,
                source,
                segment,
                request,
                &mut seen,
                &mut output,
            )?;
            trim_candidate_page(&mut output, request.direction, request.limit);
        }
        trim_candidate_page(&mut output, request.direction, request.limit);
    }
    Ok(output)
}

fn candidate_doc_is_beyond_page(
    doc: &SearchDoc,
    candidates: &[TranscriptSearchCandidate],
    request: &CandidateSearch<'_>,
) -> bool {
    if candidates.len() < request.limit {
        return false;
    }
    match request.direction {
        TranscriptSearchDirection::Forward => {
            doc.min_block_idx
                > candidates
                    .last()
                    .expect("bounded forward search page")
                    .block_idx
        }
        TranscriptSearchDirection::Backward => {
            doc.max_block_idx
                < candidates
                    .first()
                    .expect("bounded backward search page")
                    .block_idx
        }
    }
}

fn trim_candidate_page(
    candidates: &mut Vec<TranscriptSearchCandidate>,
    direction: TranscriptSearchDirection,
    limit: usize,
) {
    candidates.sort_unstable_by_key(|candidate| candidate.block_idx);
    if matches!(direction, TranscriptSearchDirection::Backward) && candidates.len() > limit {
        candidates.drain(..candidates.len() - limit);
    } else {
        candidates.truncate(limit);
    }
}

fn validate_search_doc(
    doc: &SearchDoc,
    first_record_ordinal: usize,
    last_record_ordinal: usize,
    min_block_idx: u64,
    max_block_idx: u64,
    segment: &SearchSegmentRow,
) -> Result<()> {
    if doc.first_record_ordinal != first_record_ordinal
        || doc.last_record_ordinal != last_record_ordinal
        || doc.min_block_idx != min_block_idx
        || doc.max_block_idx != max_block_idx
        || doc.first_record_ordinal > doc.last_record_ordinal
        || doc.min_block_idx > doc.max_block_idx
        || doc.last_record_ordinal as u64 >= segment.source_item_count
        || segment
            .first_doc_id
            .zip(segment.last_doc_id)
            .is_none_or(|(first, last)| doc.doc_id < first || doc.doc_id > last)
    {
        return Err(StoreError::Integrity(
            "derived search document does not match its canonical records".into(),
        ));
    }
    Ok(())
}

fn append_hydrated_search_doc_matches(
    doc: &SearchDoc,
    source: &SearchSourceSegment,
    segment: &SearchSegmentRow,
    request: &CandidateSearch<'_>,
    seen: &mut HashSet<u64>,
    candidates: &mut Vec<TranscriptSearchCandidate>,
) -> Result<()> {
    let record_ordinals = search_doc_record_ordinals(doc, request.direction);
    let records = search_source_records_at(
        request.canonical,
        request.lineage,
        source,
        &record_ordinals,
        request.cancelled,
    )?;
    append_search_doc_matches(
        doc,
        segment,
        request,
        |ordinal| records.get(&ordinal),
        seen,
        candidates,
    )
}

fn append_search_doc_matches<'a>(
    doc: &SearchDoc,
    segment: &SearchSegmentRow,
    request: &CandidateSearch<'_>,
    record_at: impl Fn(usize) -> Option<&'a StoredTranscriptBlock>,
    seen: &mut HashSet<u64>,
    candidates: &mut Vec<TranscriptSearchCandidate>,
) -> Result<()> {
    if doc.first_record_ordinal > doc.last_record_ordinal
        || doc.last_record_ordinal as u64 >= segment.source_item_count
    {
        return Err(StoreError::Integrity(
            "derived search document has an invalid record range".into(),
        ));
    }
    let mut min_block_idx = u64::MAX;
    let mut max_block_idx = 0;
    let mut matches = Vec::new();
    for ordinal in search_doc_record_ordinals(doc, request.direction) {
        let record = record_at(ordinal).ok_or_else(|| {
            StoreError::Integrity("derived search document references a missing record".into())
        })?;
        min_block_idx = min_block_idx.min(record.block_idx);
        max_block_idx = max_block_idx.max(record.block_idx);
        if origin_allows(
            record.block_idx,
            request.origin_block_idx,
            request.direction,
        ) && record.indexed_text.contains(request.query)
        {
            matches.push(TranscriptSearchCandidate {
                block_idx: record.block_idx,
                history_idx: record.history_idx,
            });
        }
    }
    validate_search_doc(
        doc,
        doc.first_record_ordinal,
        doc.last_record_ordinal,
        min_block_idx,
        max_block_idx,
        segment,
    )?;
    smelt_perf::perf::record_value(
        "store:lineage:derived_search_records_verified",
        doc.last_record_ordinal
            .saturating_sub(doc.first_record_ordinal)
            .saturating_add(1) as u64,
    );
    candidates.extend(
        matches
            .into_iter()
            .filter(|candidate| seen.insert(candidate.block_idx)),
    );
    Ok(())
}

fn search_doc_record_ordinals(doc: &SearchDoc, direction: TranscriptSearchDirection) -> Vec<usize> {
    let ordinals = doc.first_record_ordinal..=doc.last_record_ordinal;
    match direction {
        TranscriptSearchDirection::Forward => ordinals.collect(),
        TranscriptSearchDirection::Backward => ordinals.rev().collect(),
    }
}

fn direct_search_segment(
    request: &CandidateSearch<'_>,
    source: &SearchSourceSegment,
) -> Result<Vec<TranscriptSearchCandidate>> {
    let item_count = usize::try_from(source.item_count)
        .map_err(|_| StoreError::Integrity("derived search source is too large".into()))?;
    let mut candidates = Vec::new();
    let mut scan_batch = |ordinals: Vec<usize>| -> Result<bool> {
        request.check_cancelled()?;
        let mut records = search_source_records_at(
            request.canonical,
            request.lineage,
            source,
            &ordinals,
            request.cancelled,
        )?;
        for ordinal in ordinals {
            request.check_cancelled()?;
            let record = records.remove(&ordinal).ok_or_else(|| {
                StoreError::Integrity(format!(
                    "derived search source {} omitted record {ordinal}",
                    source.id
                ))
            })?;
            if origin_allows(
                record.block_idx,
                request.origin_block_idx,
                request.direction,
            ) && record.indexed_text.contains(request.query)
            {
                candidates.push(TranscriptSearchCandidate {
                    block_idx: record.block_idx,
                    history_idx: record.history_idx,
                });
                if candidates.len() == request.limit {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    };

    match request.direction {
        TranscriptSearchDirection::Forward => {
            for start in (0..item_count).step_by(DIRECT_SCAN_BATCH_RECORDS) {
                let end = start
                    .saturating_add(DIRECT_SCAN_BATCH_RECORDS)
                    .min(item_count);
                if !scan_batch((start..end).collect())? {
                    break;
                }
            }
        }
        TranscriptSearchDirection::Backward => {
            let mut end = item_count;
            while end > 0 {
                let start = end.saturating_sub(DIRECT_SCAN_BATCH_RECORDS);
                if !scan_batch((start..end).rev().collect())? {
                    break;
                }
                end = start;
            }
        }
    }
    candidates.sort_unstable_by_key(|candidate| candidate.block_idx);
    Ok(candidates)
}

fn origin_allows(
    block_idx: u64,
    origin_block_idx: Option<u64>,
    direction: TranscriptSearchDirection,
) -> bool {
    origin_block_idx.is_none_or(|origin| match direction {
        TranscriptSearchDirection::Forward => block_idx >= origin,
        TranscriptSearchDirection::Backward => block_idx <= origin,
    })
}

fn decode_search_doc(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchDoc> {
    Ok(SearchDoc {
        doc_id: row_nonnegative_u64(row, 0)?,
        first_record_ordinal: row_nonnegative_usize(row, 1)?,
        last_record_ordinal: row_nonnegative_usize(row, 2)?,
        min_block_idx: row_nonnegative_u64(row, 3)?,
        max_block_idx: row_nonnegative_u64(row, 4)?,
    })
}

pub(crate) fn search_projection_status(
    canonical: &Connection,
    canonical_path: &Path,
    lineage: &LineageId,
    branch: &BranchId,
) -> Result<SearchProjectionStatus> {
    let (_, leaves) = lineage::lineage_transcript_search_leaves(canonical, lineage, branch)?;
    let sources = search_source_segments(leaves, &never_cancelled)?;
    let search_path = search_database_path(canonical_path)?;
    let database_bytes = sqlite_physical_bytes(&search_path);
    if !search_path.is_file() {
        return Ok(SearchProjectionStatus {
            state: SearchProjectionState::Missing,
            format_version: None,
            ready_segments: 0,
            total_segments: sources.len(),
            database_bytes,
            error: None,
        });
    }
    let found_version = Connection::open_with_flags(
        &search_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
    .and_then(|conn| {
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i32>(0))
            .ok()
    });
    let search = match open_search_reader(&search_path, lineage) {
        Ok(Some(search)) => search,
        Ok(None) => unreachable!("search path existence checked"),
        Err(error) => {
            let state = if matches!(error, StoreError::UnsupportedSchema { .. }) {
                SearchProjectionState::Incompatible
            } else {
                SearchProjectionState::Corrupt
            };
            return Ok(SearchProjectionStatus {
                state,
                format_version: found_version,
                ready_segments: 0,
                total_segments: sources.len(),
                database_bytes,
                error: Some(error.to_string()),
            });
        }
    };
    if let Err(error) = search_quick_check(&search) {
        return Ok(SearchProjectionStatus {
            state: SearchProjectionState::Corrupt,
            format_version: found_version,
            ready_segments: 0,
            total_segments: sources.len(),
            database_bytes,
            error: Some(error.to_string()),
        });
    }
    let mut ready_segments = 0;
    for source in &sources {
        let Some(segment) = search_segment_row(&search, &source.id)? else {
            continue;
        };
        if !search_segment_matches_source(&segment, source) {
            continue;
        }
        if let Err(error) = validate_search_segment_structure(&search, &segment) {
            return Ok(SearchProjectionStatus {
                state: SearchProjectionState::Corrupt,
                format_version: found_version,
                ready_segments,
                total_segments: sources.len(),
                database_bytes,
                error: Some(error.to_string()),
            });
        }
        ready_segments += 1;
    }
    Ok(SearchProjectionStatus {
        state: if ready_segments == sources.len() {
            SearchProjectionState::Current
        } else {
            SearchProjectionState::Partial
        },
        format_version: found_version,
        ready_segments,
        total_segments: sources.len(),
        database_bytes,
        error: None,
    })
}

fn sqlite_physical_bytes(path: &Path) -> u64 {
    [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ]
    .into_iter()
    .filter_map(|path| fs::metadata(path).ok().map(|metadata| metadata.len()))
    .sum()
}

fn collect_short_hashes(text: &str, chars: &mut HashSet<u64>, bigrams: &mut HashSet<u64>) {
    let mut previous = None;
    for ch in text.chars() {
        chars.insert(hash_scalars(1, &[ch as u32]));
        if let Some(left) = previous {
            bigrams.insert(hash_scalars(2, &[left, ch as u32]));
        }
        previous = Some(ch as u32);
    }
}

fn hash_scalars(kind: u8, scalars: &[u32]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ u64::from(kind);
    for scalar in scalars {
        for byte in scalar.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    hash
}

fn pack_doc_ids(ids: &[u64]) -> Result<Vec<u8>> {
    let mut packed = Vec::with_capacity(ids.len());
    let mut previous = None;
    for id in ids.iter().copied() {
        let delta = previous.map_or(id, |previous| id.saturating_sub(previous));
        if previous.is_some() && delta == 0 {
            return Err(StoreError::Integrity(
                "short posting document IDs are not strictly increasing".into(),
            ));
        }
        write_varint(delta, &mut packed);
        previous = Some(id);
    }
    Ok(packed)
}

fn unpack_doc_ids(bytes: &[u8]) -> Result<Vec<u64>> {
    let mut ids = Vec::new();
    let mut offset = 0_usize;
    let mut previous: Option<u64> = None;
    while offset < bytes.len() {
        let delta = read_varint(bytes, &mut offset)?;
        if previous.is_some() && delta == 0 {
            return Err(StoreError::Integrity(
                "short posting document IDs are not strictly increasing".into(),
            ));
        }
        let id = previous.map_or(Ok(delta), |previous| {
            previous
                .checked_add(delta)
                .ok_or_else(|| StoreError::Integrity("short posting delta overflow".into()))
        })?;
        ids.push(id);
        previous = Some(id);
    }
    if ids.is_empty() {
        return Err(StoreError::Integrity("short posting is empty".into()));
    }
    Ok(ids)
}

fn write_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn read_varint(bytes: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    loop {
        let byte = *bytes
            .get(*offset)
            .ok_or_else(|| StoreError::Integrity("truncated short posting varint".into()))?;
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 64 {
            return Err(StoreError::Integrity(
                "short posting varint overflow".into(),
            ));
        }
    }
}

fn fts_anchor_expression(query: &str) -> String {
    query_anchor_grams(query)
        .into_iter()
        .map(|gram| {
            let text = gram
                .into_iter()
                .filter_map(char::from_u32)
                .collect::<String>();
            format!("\"{}\"", text.replace('"', "\"\""))
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn query_anchor_grams(query: &str) -> Vec<[u32; 3]> {
    let anchor_end = text::snap(query, SEARCH_QUERY_ANCHOR_BYTES.min(query.len()));
    let chars = text::slice(query, 0..anchor_end)
        .chars()
        .map(|ch| ch as u32)
        .collect::<Vec<_>>();
    let gram_count = chars.len().saturating_sub(2);
    if gram_count == 0 {
        return Vec::new();
    }
    let selected = SEARCH_QUERY_ANCHOR_GRAMS.min(gram_count);
    let mut grams = Vec::with_capacity(selected);
    for index in 0..selected {
        let start = if selected == 1 {
            0
        } else {
            index * (gram_count - 1) / (selected - 1)
        };
        let gram = [chars[start], chars[start + 1], chars[start + 2]];
        if !grams.contains(&gram) {
            grams.push(gram);
        }
    }
    grams
}

fn sql_i64(value: impl TryInto<i64>, field: &str) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| StoreError::Integrity(format!("{field} exceeds SQLite INTEGER")))
}

fn nonnegative_usize(value: i64, field: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| StoreError::Integrity(format!("{field} is negative or too large")))
}

fn nonnegative_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is negative")))
}

fn row_nonnegative_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn row_optional_nonnegative_u64(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| {
            u64::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

fn row_nonnegative_usize(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<usize> {
    let value = row.get::<_, i64>(index)?;
    usize::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postings_roundtrip_and_reject_duplicates() {
        let ids = [4, 7, 128, 65_000];
        let packed = pack_doc_ids(&ids).unwrap();
        assert_eq!(unpack_doc_ids(&packed).unwrap(), ids);
        assert!(pack_doc_ids(&[4, 4]).is_err());
    }

    #[test]
    fn anchor_grams_include_both_ends_of_the_bounded_prefix() {
        let query = "abcdefghijklmnopqrstuvwxyz".repeat(40);
        let grams = query_anchor_grams(&query);
        let anchor_end = text::snap(&query, SEARCH_QUERY_ANCHOR_BYTES);
        let chars = text::slice(&query, 0..anchor_end)
            .chars()
            .collect::<Vec<_>>();
        assert!(grams.contains(&[chars[0] as u32, chars[1] as u32, chars[2] as u32,]));
        assert!(grams.contains(&[
            chars[chars.len() - 3] as u32,
            chars[chars.len() - 2] as u32,
            chars[chars.len() - 1] as u32,
        ]));
    }

    #[test]
    fn source_groups_keep_sealed_byte_and_leaf_boundaries_stable() {
        let leaf = |index: u64, byte_count: u64| TranscriptSearchLeaf {
            node_id: format!("{index:064x}"),
            start_index: index,
            item_count: 1,
            byte_count,
        };

        let at_byte_limit = vec![leaf(0, SEARCH_SEGMENT_BYTES - 1), leaf(1, 1)];
        let byte_limit_group =
            search_source_segments(at_byte_limit.clone(), &never_cancelled).unwrap();
        assert_eq!(byte_limit_group.len(), 1);
        assert_eq!(byte_limit_group[0].byte_count, SEARCH_SEGMENT_BYTES);

        let mut over_byte_limit = at_byte_limit;
        over_byte_limit.push(leaf(2, 1));
        let over_byte_limit = search_source_segments(over_byte_limit, &never_cancelled).unwrap();
        assert_eq!(over_byte_limit.len(), 2);
        assert_eq!(over_byte_limit[0].id, byte_limit_group[0].id);
        assert_eq!(over_byte_limit[0].leaves.len(), 2);
        assert_eq!(over_byte_limit[1].leaves.len(), 1);

        let at_leaf_limit = (0..SEARCH_SEGMENT_MAX_LEAVES)
            .map(|index| leaf(index as u64, 1))
            .collect::<Vec<_>>();
        let leaf_limit_group =
            search_source_segments(at_leaf_limit.clone(), &never_cancelled).unwrap();
        assert_eq!(leaf_limit_group.len(), 1);
        assert_eq!(leaf_limit_group[0].leaves.len(), SEARCH_SEGMENT_MAX_LEAVES);

        let mut over_leaf_limit = at_leaf_limit;
        over_leaf_limit.push(leaf(SEARCH_SEGMENT_MAX_LEAVES as u64, 1));
        let over_leaf_limit = search_source_segments(over_leaf_limit, &never_cancelled).unwrap();
        assert_eq!(over_leaf_limit.len(), 2);
        assert_eq!(over_leaf_limit[0].id, leaf_limit_group[0].id);
        assert_eq!(over_leaf_limit[0].leaves.len(), SEARCH_SEGMENT_MAX_LEAVES);
        assert_eq!(over_leaf_limit[1].leaves.len(), 1);
    }

    #[test]
    fn search_documents_pack_short_records_and_overlap_long_records() {
        let record = |block_idx: u64, text: String| StoredTranscriptBlock {
            block_idx,
            history_idx: None,
            kind: "assistant".into(),
            tool_call_id: None,
            tool_name: None,
            content_hash: format!("{block_idx:064x}"),
            estimated_text_bytes: text.len() as u64,
            preview_text: text.clone(),
            block_json: "{}".into(),
            indexed_text: text,
            origin_json: None,
            tool_state_json: None,
        };
        let long_text = "é".repeat(SEARCH_DOCUMENT_BYTES / 2 + 100);
        let records = vec![
            record(10, "alpha".into()),
            record(100, String::new()),
            record(30, "beta".into()),
            record(40, long_text),
        ];

        let documents = search_document_inputs(&records).unwrap();
        assert_eq!(documents.len(), 3);
        assert_eq!(
            documents[0],
            SearchDocumentInput {
                first_record_ordinal: 0,
                last_record_ordinal: 2,
                min_block_idx: 10,
                max_block_idx: 100,
                text: "alpha\nbeta".into(),
            }
        );
        for document in &documents[1..] {
            assert_eq!(document.first_record_ordinal, 3);
            assert_eq!(document.last_record_ordinal, 3);
            assert_eq!(document.min_block_idx, 40);
            assert_eq!(document.max_block_idx, 40);
            assert!(
                document.text.len()
                    <= SEARCH_DOCUMENT_BYTES.saturating_add(SEARCH_DOCUMENT_OVERLAP_BYTES)
            );
        }
        assert!(documents[1].text.len() > SEARCH_DOCUMENT_BYTES);
    }

    #[test]
    fn cancelled_segment_build_rolls_back_every_derived_row() {
        let lineage = LineageId::from_hex("1".repeat(32)).unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        create_search_schema(&conn, &lineage).unwrap();
        let text = "cancel-safe needle ".repeat(4096);
        let record = StoredTranscriptBlock {
            block_idx: 2,
            history_idx: Some(1),
            kind: "assistant".into(),
            tool_call_id: None,
            tool_name: None,
            content_hash: "2".repeat(64),
            estimated_text_bytes: text.len() as u64,
            preview_text: text.clone(),
            block_json: serde_json::json!({"Text": {"content": text.clone()}}).to_string(),
            indexed_text: text,
            origin_json: None,
            tool_state_json: None,
        };
        let leaf = TranscriptSearchLeaf {
            node_id: "3".repeat(64),
            start_index: 0,
            item_count: 1,
            byte_count: 1,
        };
        let source = finish_search_source_segment(vec![leaf]).unwrap();
        let cancellation_checks = std::cell::Cell::new(0_usize);
        let cancelled = || {
            let check = cancellation_checks.get();
            cancellation_checks.set(check + 1);
            check >= 2
        };
        assert!(!build_search_segment(
            &mut conn,
            &source,
            std::slice::from_ref(&record),
            &cancelled,
        )
        .unwrap());
        for table in ["search_segments", "search_docs", "search_short_postings"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "cancellation left rows in {table}");
        }
        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search_fts WHERE search_fts MATCH 'nee'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_count, 0, "cancellation left rows in search_fts");

        assert!(
            build_search_segment(&mut conn, &source, std::slice::from_ref(&record), &|| false,)
                .unwrap()
        );
        let segment = search_segment_row(&conn, &source.id).unwrap().unwrap();
        assert!(segment.doc_count > 1);
        validate_search_segment_structure(&conn, &segment).unwrap();
    }
}
