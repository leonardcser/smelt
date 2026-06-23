use crate::session::{ContextCheckpoint, Session, SessionHeader, SessionStoreRef};
use protocol::{history_item_message_count, HistoryItem};
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveDirtyState {
    pub history_from: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct LiveSideTables;

#[derive(Clone, Debug)]
pub struct LiveSession {
    pub header: SessionHeader,
    pub session_dir: PathBuf,
    pub store: Option<SessionStoreRef>,
    pub persisted_history_len: usize,
    pub store_revision: u64,
    pub live_start: usize,
    pub live_history: Vec<HistoryItem>,
    pub checkpoint: Option<ContextCheckpoint>,
    pub side_tables: LiveSideTables,
    pub dirty: LiveDirtyState,
}

impl LiveSession {
    pub fn from_store(header: SessionHeader, store: SessionStoreRef) -> Self {
        Self::from_parts(header, store.session_dir.clone(), Some(store))
    }

    pub fn from_parts(
        header: SessionHeader,
        session_dir: PathBuf,
        store: Option<SessionStoreRef>,
    ) -> Self {
        let persisted_history_len = header.history_len;
        let store_revision = header.revision;
        let checkpoint = header.meta.checkpoint.clone();
        Self {
            header,
            session_dir,
            store,
            persisted_history_len,
            store_revision,
            live_start: persisted_history_len,
            live_history: Vec::new(),
            checkpoint,
            side_tables: LiveSideTables,
            dirty: LiveDirtyState::default(),
        }
    }

    pub fn id(&self) -> &str {
        &self.header.meta.id
    }

    pub fn dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn history_len(&self) -> usize {
        if self.live_start < self.persisted_history_len {
            self.live_start.saturating_add(self.live_history.len())
        } else {
            self.persisted_history_len
                .saturating_add(self.live_history.len())
        }
    }

    pub fn is_empty(&self) -> bool {
        self.history_len() == 0
    }

    pub fn persisted_history_len(&self) -> usize {
        self.persisted_history_len
    }

    pub fn set_checkpoint(&mut self, checkpoint: Option<ContextCheckpoint>) {
        self.checkpoint = checkpoint;
    }

    pub fn append_history(&mut self, item: HistoryItem) -> usize {
        let idx = self.history_len();
        if self.live_history.is_empty() {
            self.live_start = idx;
        }
        self.live_history.push(item);
        self.mark_dirty_from(idx);
        idx
    }

    pub fn truncate_from(&mut self, index: usize) {
        let index = index.min(self.history_len());
        if index >= self.live_start {
            self.live_history.truncate(index - self.live_start);
        } else {
            self.live_start = index;
            self.live_history.clear();
        }
        self.mark_dirty_from(index);
    }

    pub fn mark_persisted(&mut self, persisted_history_len: usize, revision: u64) {
        self.persisted_history_len = persisted_history_len;
        self.store_revision = revision;
        self.live_start = persisted_history_len;
        self.live_history.clear();
        self.dirty.history_from = None;
        self.header.history_len = persisted_history_len;
        self.header.revision = revision;
        self.header.meta.history_len = Some(persisted_history_len);
        self.header.meta.checkpoint = self.checkpoint.clone();
    }

    pub fn history_range(&self, range: Range<usize>) -> Result<Vec<HistoryItem>, String> {
        let end = range.end.min(self.history_len());
        let start = range.start.min(end);
        if start == end {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        let persisted_end = end.min(self.persisted_history_len).min(self.live_start);
        if start < persisted_end {
            let db = self.open_store()?;
            out.extend(
                db.read_history_items_range(start..persisted_end)
                    .map_err(|err| format!("read session history range: {err}"))?,
            );
        }

        let live_start = start.max(self.live_start);
        if live_start < end {
            let offset_start = live_start - self.live_start;
            let offset_end = end - self.live_start;
            out.extend(self.live_history[offset_start..offset_end].iter().cloned());
        }
        Ok(out)
    }

    pub fn history_tail(
        &self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<HistoryItem>, String> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let len = self.history_len();
        let start = len.saturating_sub(max_items);
        let mut rows = self.history_range(start..len)?;
        if let Some(max_bytes) = max_bytes {
            while history_json_bytes(&rows) > max_bytes && !rows.is_empty() {
                rows.remove(0);
            }
        }
        Ok(rows)
    }

    pub fn model_history_source(&self, summary_prefix: &str) -> protocol::ModelHistorySource {
        let (prefix, first_live_index) = if let Some(checkpoint) = self.checkpoint.as_ref() {
            (
                vec![HistoryItem::user(protocol::Content::text(format!(
                    "{}\n{}",
                    summary_prefix.trim_end(),
                    checkpoint.summary
                )))],
                checkpoint.first_live_index,
            )
        } else {
            (Vec::new(), 0)
        };
        let store_end_index = self.live_start.min(self.persisted_history_len);
        let store_start_index = first_live_index.min(store_end_index);
        let suffix_start = first_live_index.saturating_sub(self.live_start);
        let suffix = self
            .live_history
            .get(suffix_start..)
            .unwrap_or(&[])
            .to_vec();
        protocol::ModelHistorySource::store_with_suffix(
            prefix,
            store_start_index,
            store_end_index,
            suffix,
        )
    }

    pub fn first_live_history_index_for_model_message(
        &self,
        first_live_message_index: usize,
    ) -> Result<Option<usize>, String> {
        if first_live_message_index == 0 {
            return Ok(None);
        }

        let mut message_index = 0usize;
        let first_history_index = if let Some(checkpoint) = &self.checkpoint {
            message_index = 1;
            checkpoint.first_live_index
        } else {
            0
        };

        const SCAN_CHUNK_ITEMS: usize = 128;
        let mut history_index = first_history_index;
        while history_index < self.history_len() {
            let chunk_end = history_index
                .saturating_add(SCAN_CHUNK_ITEMS)
                .min(self.history_len());
            let rows = self.history_range(history_index..chunk_end)?;
            for item in &rows {
                if first_live_message_index == message_index {
                    return Ok(Some(history_index));
                }
                let next_message_index =
                    message_index.saturating_add(history_item_message_count(item));
                if first_live_message_index < next_message_index {
                    return Ok(None);
                }
                message_index = next_message_index;
                history_index = history_index.saturating_add(1);
            }
            if rows.is_empty() {
                break;
            }
        }

        Ok((first_live_message_index == message_index).then_some(self.history_len()))
    }

    pub fn materialize_full_session(
        &self,
        template: &Session,
        reason: &'static str,
    ) -> Result<Session, String> {
        smelt_perf::perf::record_value("compat:session:full_materialized", 1);
        smelt_perf::perf::record_value(reason, 1);
        let mut session = template.clone();
        session.history = self.history_range(0..self.history_len())?;
        session.checkpoint = self.checkpoint.clone();
        Ok(session)
    }

    fn mark_dirty_from(&mut self, index: usize) {
        self.dirty.history_from = Some(
            self.dirty
                .history_from
                .map_or(index, |current| current.min(index)),
        );
    }

    fn open_store(&self) -> Result<smelt_store::SessionDb, String> {
        let db_path = self
            .store
            .as_ref()
            .map(|store| store.db_path.clone())
            .unwrap_or_else(|| self.session_dir.join("session.db"));
        smelt_store::SessionDb::open_read_only(&db_path)
            .map_err(|err| format!("open session database {}: {err}", db_path.display()))
    }
}

fn history_json_bytes(items: &[HistoryItem]) -> usize {
    items
        .iter()
        .map(|item| serde_json::to_vec(item).map_or(0, |json| json.len()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_store::{SessionSnapshot, SessionState};

    #[test]
    fn store_backed_live_session_reads_persisted_range_and_live_suffix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let db = smelt_store::SessionDb::open(&db_path).expect("open db");
        let persisted = vec![
            HistoryItem::user(protocol::Content::text("one")),
            HistoryItem::user(protocol::Content::text("two")),
        ];
        db.save_session_snapshot_for_import(&SessionSnapshot {
            state: SessionState {
                id: "live".into(),
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                parent_id: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: 0.0,
                revision: 1,
                history_len: persisted.len() as u64,
                created_at: 1,
                updated_at: 2,
            },
            meta_json: None,
            history_start_idx: 0,
            history_len: persisted.len(),
            history: persisted,
            turn_metas: Vec::new(),
            metadata_snapshots: Vec::new(),
            accounting_snapshots: Vec::new(),
        })
        .expect("save snapshot");

        let header = SessionHeader {
            meta: crate::session::SessionMeta {
                id: "live".into(),
                title: None,
                slug: None,
                first_user_message: None,
                created_at_ms: 1,
                updated_at_ms: 2,
                mode: None,
                reasoning_effort: None,
                model: None,
                cwd: None,
                parent_id: None,
                context_tokens: None,
                history_len: Some(2),
                checkpoint: None,
                text_bytes: None,
                migration: None,
            },
            history_len: 2,
            revision: 1,
        };
        let mut live = LiveSession::from_store(
            header,
            SessionStoreRef {
                session_dir: dir.path().to_path_buf(),
                db_path,
            },
        );
        live.append_history(HistoryItem::user(protocol::Content::text("three")));

        let rows = live.history_range(1..3).expect("range");
        assert_eq!(rows.len(), 2);
        assert_eq!(live.history_len(), 3);
        assert_eq!(live.dirty.history_from, Some(2));
        let source = live.model_history_source("summary:");
        match source {
            protocol::ModelHistorySource::Store {
                first_live_index,
                end_index,
                suffix,
                ..
            } => {
                assert_eq!(first_live_index, 0);
                assert_eq!(end_index, 2);
                assert_eq!(suffix.len(), 1);
            }
            protocol::ModelHistorySource::Items(_) => panic!("expected store-backed model history"),
        }
    }
}
