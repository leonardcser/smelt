use crate::session::{ContextCheckpoint, Session, SessionHeader, SessionStoreRef};
use protocol::{history_item_message_count, HistoryItem};
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct LiveSideTables;

#[derive(Clone, Debug)]
pub struct LiveSession {
    pub header: SessionHeader,
    pub session_dir: PathBuf,
    pub store: Option<SessionStoreRef>,
    pub live_start: usize,
    pub live_history: Vec<HistoryItem>,
    pub side_tables: LiveSideTables,
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
        let live_start = header.history_len;
        Self {
            header,
            session_dir,
            store,
            live_start,
            live_history: Vec::new(),
            side_tables: LiveSideTables,
        }
    }

    pub fn id(&self) -> &str {
        &self.header.meta.id
    }

    pub fn dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn history_len(&self) -> usize {
        self.live_start.saturating_add(self.live_history.len())
    }

    pub fn is_empty(&self) -> bool {
        self.history_len() == 0
    }

    pub fn live_suffix_len(&self) -> usize {
        self.live_history.len()
    }

    pub fn live_suffix_bytes(&self) -> usize {
        history_json_bytes(&self.live_history)
    }

    pub fn append_history(&mut self, item: HistoryItem) -> usize {
        let idx = self.history_len();
        if self.live_history.is_empty() {
            self.live_start = idx;
        }
        self.live_history.push(item);
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
    }

    pub fn compact_saved_prefix(
        &mut self,
        saved_history_len: usize,
        revision: u64,
        checkpoint: Option<&ContextCheckpoint>,
    ) {
        let saved_history_len = saved_history_len.min(self.history_len());
        if saved_history_len <= self.live_start {
            self.live_start = saved_history_len;
            self.live_history.clear();
            self.header.history_len = saved_history_len;
            self.header.revision = revision;
            self.header.meta.history_len = Some(saved_history_len);
            self.header.meta.checkpoint = checkpoint.cloned();
            return;
        }
        let drop_count = saved_history_len - self.live_start;
        if drop_count >= self.live_history.len() {
            self.live_history.clear();
        } else {
            self.live_history.drain(..drop_count);
        }
        self.live_start = saved_history_len;
        self.header.history_len = saved_history_len;
        self.header.revision = revision;
        self.header.meta.history_len = Some(saved_history_len);
        self.header.meta.checkpoint = checkpoint.cloned();
    }

    pub fn replace_header(&mut self, header: SessionHeader) {
        self.header = header;
        self.live_start = self.header.history_len;
        self.live_history.clear();
    }

    pub fn history_range(&self, range: Range<usize>) -> Result<Vec<HistoryItem>, String> {
        let end = range.end.min(self.history_len());
        let start = range.start.min(end);
        if start == end {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        let stored_end = end.min(self.live_start);
        if start < stored_end {
            let db = self.open_store()?;
            out.extend(
                db.read_history_items_range(start..stored_end)
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

    pub fn any_transcript_visible_before(&self, end: usize) -> Result<bool, String> {
        const SCAN_CHUNK_ITEMS: usize = 128;
        let end = end.min(self.history_len());
        let mut start = 0usize;
        while start < end {
            let chunk_end = start.saturating_add(SCAN_CHUNK_ITEMS).min(end);
            if self
                .history_range(start..chunk_end)?
                .iter()
                .any(HistoryItem::is_transcript_visible)
            {
                return Ok(true);
            }
            start = chunk_end;
        }
        Ok(false)
    }

    pub fn effective_mode_at(&self, hist_idx: usize, fallback: &str) -> Result<String, String> {
        const SCAN_CHUNK_ITEMS: usize = 128;
        let end = hist_idx.min(self.history_len());
        let mut chunk_end = end;
        while chunk_end > 0 {
            let chunk_start = chunk_end.saturating_sub(SCAN_CHUNK_ITEMS);
            let rows = self.history_range(chunk_start..chunk_end)?;
            if let Some(mode) = rows
                .iter()
                .rev()
                .filter_map(HistoryItem::as_note)
                .find_map(protocol::HistoryNote::mode)
            {
                return Ok(mode.to_string());
            }
            chunk_end = chunk_start;
        }

        let mut start = end;
        while start < self.history_len() {
            let chunk_end = start
                .saturating_add(SCAN_CHUNK_ITEMS)
                .min(self.history_len());
            let rows = self.history_range(start..chunk_end)?;
            if let Some(mode) = rows
                .iter()
                .filter_map(HistoryItem::as_note)
                .find_map(protocol::HistoryNote::base_mode)
            {
                return Ok(mode.to_string());
            }
            start = chunk_end;
        }

        Ok(fallback.to_string())
    }

    pub fn model_history_source(
        &self,
        summary_prefix: &str,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> protocol::ModelHistorySource {
        let (prefix, first_live_index) = if let Some(checkpoint) = checkpoint {
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
        let store_end_index = self.live_start;
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
            first_live_index,
        )
    }

    pub fn first_live_history_index_for_model_message(
        &self,
        checkpoint: Option<&ContextCheckpoint>,
        first_live_message_index: usize,
    ) -> Result<Option<usize>, String> {
        if first_live_message_index == 0 {
            return Ok(None);
        }

        let mut message_index = 0usize;
        let first_history_index = if let Some(checkpoint) = checkpoint {
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
        smelt_perf::perf::record_value("session:full_materialized", 1);
        smelt_perf::perf::record_value(reason, 1);
        let mut session = template.clone();
        session.history = self.history_range(0..self.history_len())?;
        Ok(session)
    }

    fn open_store(&self) -> Result<smelt_store::SessionReader, String> {
        let db_path = self
            .store
            .as_ref()
            .map(|store| store.db_path.clone())
            .unwrap_or_else(|| self.session_dir.join("session.db"));
        smelt_store::SessionReader::open_database(&db_path)
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

    fn seed_store(
        db: &mut smelt_store::SessionDb,
        id: &str,
        mode: Option<&str>,
        history: Vec<HistoryItem>,
    ) {
        let command = smelt_store::SessionCommit {
            session_id: id.into(),
            expected: smelt_store::StoreHead::default(),
            identity: smelt_store::SessionIdentity {
                id: id.into(),
                created_at: 1,
                parent_id: None,
            },
            metadata: smelt_store::SessionMetadata {
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: mode.map(str::to_owned),
                reasoning_effort: None,
                model: None,
                fast_mode: None,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: smelt_store::SessionCostUsd::new(0.0).unwrap(),
                updated_at: 2,
            },
            history: smelt_store::HistorySuffix {
                start: smelt_store::HistoryIndex::ZERO,
                final_len: smelt_store::HistoryLen::new(history.len() as u64),
                items: history,
            },
            side_tables: smelt_store::SideTableSuffixes::default(),
            descriptors: None,
        };
        db.apply_session_commit(&command)
            .expect("seed session store");
    }

    #[test]
    fn store_backed_live_session_reads_persisted_range_and_live_suffix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let mut db = smelt_store::SessionDb::open(&db_path).expect("open db");
        let persisted = vec![
            HistoryItem::user(protocol::Content::text("one")),
            HistoryItem::user(protocol::Content::text("two")),
        ];
        seed_store(&mut db, "live", None, persisted);

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
                fast_mode: None,
                cwd: None,
                parent_id: None,
                context_tokens: None,
                context_token_identity: None,
                display_context_token_identity: None,
                history_len: Some(2),
                checkpoint: None,
                text_bytes: None,
            },
            history_len: 2,
            revision: 1,
            degraded_warnings: Vec::new(),
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
        let source = live.model_history_source("summary:", None);
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
            protocol::ModelHistorySource::Items { .. } => {
                panic!("expected store-backed model history")
            }
        }

        let checkpoint = ContextCheckpoint {
            summary: "summary through live suffix".into(),
            first_live_index: 3,
            ..Default::default()
        };
        let source = live.model_history_source("SUMMARY:", Some(&checkpoint));
        assert_eq!(source.coordinates().canonical_start().get(), 3);
        assert_eq!(source.coordinates().model_prefix_len(), 1);
        assert!(matches!(
            source,
            protocol::ModelHistorySource::Store {
                first_live_index: 2,
                ref suffix,
                ..
            } if suffix.is_empty()
        ));
    }

    #[test]
    fn store_backed_live_session_scans_mode_and_visibility_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let mut db = smelt_store::SessionDb::open(&db_path).expect("open db");
        let history = vec![
            HistoryItem::user(protocol::Content::text("hello")),
            HistoryItem::Note(protocol::HistoryNote::mode_change_for_transition(
                "normal", "plan", "switch",
            )),
            HistoryItem::user(protocol::Content::text("after mode")),
        ];
        seed_store(&mut db, "live-scan", Some("normal"), history);
        let header = SessionHeader {
            meta: crate::session::SessionMeta {
                id: "live-scan".into(),
                title: None,
                slug: None,
                first_user_message: None,
                created_at_ms: 1,
                updated_at_ms: 2,
                mode: Some("normal".into()),
                reasoning_effort: None,
                model: None,
                fast_mode: None,
                cwd: None,
                parent_id: None,
                context_tokens: None,
                context_token_identity: None,
                display_context_token_identity: None,
                history_len: Some(3),
                checkpoint: None,
                text_bytes: None,
            },
            history_len: 3,
            revision: 1,
            degraded_warnings: Vec::new(),
        };
        let live = LiveSession::from_store(
            header,
            SessionStoreRef {
                session_dir: dir.path().to_path_buf(),
                db_path,
            },
        );

        assert!(live.any_transcript_visible_before(2).unwrap());
        assert_eq!(live.effective_mode_at(3, "normal").unwrap(), "plan");
        assert_eq!(live.effective_mode_at(0, "normal").unwrap(), "normal");
    }
}
