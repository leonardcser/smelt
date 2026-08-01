use crate::session::{
    ContextCheckpoint, Session, SessionHeader, SessionStoreLocation, SessionStoreRef,
};
use protocol::{history_item_message_count, HistoryItem};
use std::ops::Range;
use std::path::{Path, PathBuf};

const HISTORY_SEMANTIC_SCAN_CHUNK_ITEMS: usize = 256;

enum LiveStoreReader {
    // COMPAT(session-lineage-v1): read-only live sessions can represent an
    // explicitly unmigrated previous-format session.
    Legacy(smelt_store::SessionReader),
    Lineage(smelt_store::LineageSessionReader),
}

impl LiveStoreReader {
    fn read_history_items_range(
        &self,
        range: Range<usize>,
    ) -> smelt_store::Result<Vec<HistoryItem>> {
        match self {
            Self::Legacy(reader) => reader.read_history_items_range(range),
            Self::Lineage(reader) => reader.history_range(range.start as u64, range.end as u64),
        }
    }

    fn read_history_items_tail(
        &self,
        end: usize,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> smelt_store::Result<Vec<HistoryItem>> {
        match self {
            Self::Legacy(reader) => reader.read_history_items_tail(end, max_items, max_bytes),
            Self::Lineage(reader) => reader.history_tail(end, max_items, max_bytes),
        }
    }

    fn history_note_projection_at(
        &self,
        index: usize,
    ) -> smelt_store::Result<Option<protocol::HistoryNoteProjection>> {
        if let Self::Legacy(reader) = self {
            return reader.history_note_projection_at(index);
        }
        Ok(self
            .read_history_items_range(index..index.saturating_add(1))?
            .into_iter()
            .next()
            .and_then(|item| {
                item.as_note().map(|note| protocol::HistoryNoteProjection {
                    kind: note.kind(),
                    mode: note.mode().map(str::to_owned),
                })
            }))
    }

    fn history_last_context_note_index_before(
        &self,
        end: usize,
        name: &str,
    ) -> smelt_store::Result<Option<usize>> {
        if let Self::Legacy(reader) = self {
            return reader.history_last_context_note_index_before(end, name);
        }
        let mut cursor = end;
        while cursor > 0 {
            let start = cursor.saturating_sub(HISTORY_SEMANTIC_SCAN_CHUNK_ITEMS);
            let items = self.read_history_items_range(start..cursor)?;
            if let Some(index) = items.iter().rposition(|item| {
                item.as_note().and_then(protocol::HistoryNote::context_name) == Some(name)
            }) {
                return Ok(Some(start + index));
            }
            cursor = start;
        }
        Ok(None)
    }

    fn history_any_transcript_visible_before(&self, end: usize) -> smelt_store::Result<bool> {
        match self {
            Self::Legacy(reader) => return reader.history_any_transcript_visible_before(end),
            Self::Lineage(reader) => {
                let state = reader.snapshot()?;
                let history_len = state.head.history_len.as_usize().unwrap_or(usize::MAX);
                if end >= history_len {
                    return Ok(state.transcript_len > 0);
                }
            }
        }
        let mut start = 0;
        while start < end {
            let next = start
                .saturating_add(HISTORY_SEMANTIC_SCAN_CHUNK_ITEMS)
                .min(end);
            if self
                .read_history_items_range(start..next)?
                .iter()
                .any(HistoryItem::is_transcript_visible)
            {
                return Ok(true);
            }
            start = next;
        }
        Ok(false)
    }

    fn history_mode_before(&self, end: usize) -> smelt_store::Result<Option<String>> {
        if let Self::Legacy(reader) = self {
            return reader.history_mode_before(end);
        }
        let mut cursor = end;
        while cursor > 0 {
            let start = cursor.saturating_sub(HISTORY_SEMANTIC_SCAN_CHUNK_ITEMS);
            if let Some(mode) = self
                .read_history_items_range(start..cursor)?
                .iter()
                .rev()
                .filter_map(HistoryItem::as_note)
                .find_map(protocol::HistoryNote::mode)
            {
                return Ok(Some(mode.to_owned()));
            }
            cursor = start;
        }
        Ok(None)
    }

    fn history_base_mode_range(&self, range: Range<usize>) -> smelt_store::Result<Option<String>> {
        if let Self::Legacy(reader) = self {
            return reader.history_base_mode_range(range);
        }
        let mut start = range.start;
        while start < range.end {
            let next = start
                .saturating_add(HISTORY_SEMANTIC_SCAN_CHUNK_ITEMS)
                .min(range.end);
            if let Some(mode) = self
                .read_history_items_range(start..next)?
                .iter()
                .filter_map(HistoryItem::as_note)
                .find_map(protocol::HistoryNote::base_mode)
            {
                return Ok(Some(mode.to_owned()));
            }
            start = next;
        }
        Ok(None)
    }
}

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

        let live = select_history_tail(&self.live_history, max_items, max_bytes)?;
        let mut live_rows = self.live_history[live.start..].to_vec();
        if live.budget.limit_reached() || live_rows.len() == max_items || self.live_start == 0 {
            return Ok(live_rows);
        }

        let remaining_items = max_items.saturating_sub(live_rows.len());
        let remaining_bytes = live.budget.remaining_bytes();
        let db = self.open_store()?;
        let mut stored_rows = db
            .read_history_items_tail(self.live_start, remaining_items, remaining_bytes)
            .map_err(|err| format!("read session history tail: {err}"))?;
        stored_rows.append(&mut live_rows);
        Ok(stored_rows)
    }

    pub fn plan_history_append(
        &self,
        append: &protocol::HistoryAppend,
    ) -> Result<protocol::HistoryAppendPlan, String> {
        protocol::plan_history_append(self, append)
    }

    fn last_note_projection(&self) -> Result<Option<protocol::HistoryNoteProjection>, String> {
        if let Some(item) = self.live_history.last() {
            return Ok(item.as_note().map(|note| protocol::HistoryNoteProjection {
                kind: note.kind(),
                mode: note.mode().map(str::to_string),
            }));
        }
        if self.live_start == 0 {
            return Ok(None);
        }
        self.open_store()?
            .history_note_projection_at(self.live_start - 1)
            .map_err(|err| format!("read final session history note: {err}"))
    }

    fn last_context_note_index(&self, name: &str) -> Result<Option<usize>, String> {
        if let Some(index) = self.live_history.iter().rposition(|item| {
            item.as_note().and_then(protocol::HistoryNote::context_name) == Some(name)
        }) {
            return Ok(Some(self.live_start.saturating_add(index)));
        }
        if self.live_start == 0 {
            return Ok(None);
        }
        self.open_store()?
            .history_last_context_note_index_before(self.live_start, name)
            .map_err(|err| format!("find session context note: {err}"))
    }

    fn history_item_matches(&self, index: usize, item: &HistoryItem) -> Result<bool, String> {
        if index >= self.live_start {
            return Ok(self.live_history.get(index - self.live_start) == Some(item));
        }
        Ok(self.history_range(index..index.saturating_add(1))?.first() == Some(item))
    }

    pub fn any_transcript_visible_before(&self, end: usize) -> Result<bool, String> {
        let end = end.min(self.history_len());
        let stored_end = end.min(self.live_start);
        if stored_end > 0
            && self
                .open_store()?
                .history_any_transcript_visible_before(stored_end)
                .map_err(|err| format!("read session history visibility: {err}"))?
        {
            return Ok(true);
        }
        let live_end = end.saturating_sub(self.live_start);
        Ok(self.live_history[..live_end]
            .iter()
            .any(HistoryItem::is_transcript_visible))
    }

    pub fn effective_mode_at(&self, hist_idx: usize, fallback: &str) -> Result<String, String> {
        let end = hist_idx.min(self.history_len());
        let live_end = end.saturating_sub(self.live_start);
        if let Some(mode) = self.live_history[..live_end]
            .iter()
            .rev()
            .filter_map(HistoryItem::as_note)
            .find_map(protocol::HistoryNote::mode)
        {
            return Ok(mode.to_string());
        }

        let db = (self.live_start > 0)
            .then(|| self.open_store())
            .transpose()?;
        let stored_end = end.min(self.live_start);
        if stored_end > 0 {
            let mode = db
                .as_ref()
                .expect("stored history has a database")
                .history_mode_before(stored_end)
                .map_err(|err| format!("read session history mode: {err}"))?;
            if let Some(mode) = mode {
                return Ok(mode);
            }
        }

        if end < self.live_start {
            let base_mode = db
                .as_ref()
                .expect("stored history has a database")
                .history_base_mode_range(end..self.live_start)
                .map_err(|err| format!("read session history base mode: {err}"))?;
            if let Some(base_mode) = base_mode {
                return Ok(base_mode);
            }
        }
        let live_start = end.saturating_sub(self.live_start);
        if let Some(base_mode) = self.live_history[live_start..]
            .iter()
            .filter_map(HistoryItem::as_note)
            .find_map(protocol::HistoryNote::base_mode)
        {
            return Ok(base_mode.to_string());
        }

        Ok(fallback.to_string())
    }

    pub fn model_history_source(
        &self,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> protocol::ModelHistorySource {
        let (prefix, first_live_index) = if let Some(checkpoint) = checkpoint {
            (
                vec![HistoryItem::user(protocol::compaction_summary_content(
                    &checkpoint.summary,
                ))],
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

    fn open_store(&self) -> Result<LiveStoreReader, String> {
        let location = self.store.as_ref().map(|store| &store.location);
        match location {
            Some(SessionStoreLocation::Lineage { root, session_id }) => {
                smelt_store::LineageSessionReader::open_existing(root, session_id)
                    .map(LiveStoreReader::Lineage)
                    .map_err(|error| {
                        format!(
                            "open lineage session {session_id} in {}: {error}",
                            root.display()
                        )
                    })
            }
            Some(SessionStoreLocation::LegacyDatabase(db_path)) => {
                smelt_store::SessionReader::open_database(db_path)
                    .map(LiveStoreReader::Legacy)
                    .map_err(|error| {
                        format!("open session database {}: {error}", db_path.display())
                    })
            }
            None => {
                if let Some(root) = self.session_dir.parent() {
                    match smelt_store::LineageSessionReader::try_open_existing(root, self.id()) {
                        Ok(Some(reader)) => return Ok(LiveStoreReader::Lineage(reader)),
                        Ok(None) => {}
                        Err(error) => {
                            return Err(format!(
                                "locate lineage session {} in {}: {error}",
                                self.id(),
                                root.display()
                            ));
                        }
                    }
                }
                // COMPAT(session-lineage-v1): store-less callers historically
                // inferred an unmigrated database from the session directory.
                let db_path = self.session_dir.join("session.db");
                smelt_store::SessionReader::open_database(&db_path)
                    .map(LiveStoreReader::Legacy)
                    .map_err(|error| {
                        format!("open session database {}: {error}", db_path.display())
                    })
            }
        }
    }
}

impl protocol::HistoryAppendView for LiveSession {
    type Error = String;

    fn history_len(&self) -> usize {
        LiveSession::history_len(self)
    }

    fn last_note_projection(&self) -> Result<Option<protocol::HistoryNoteProjection>, Self::Error> {
        LiveSession::last_note_projection(self)
    }

    fn last_context_note_index(&self, name: &str) -> Result<Option<usize>, Self::Error> {
        LiveSession::last_context_note_index(self, name)
    }

    fn history_item_matches(&self, index: usize, item: &HistoryItem) -> Result<bool, Self::Error> {
        LiveSession::history_item_matches(self, index, item)
    }

    fn effective_mode_at(&self, index: usize, fallback: &str) -> Result<String, Self::Error> {
        LiveSession::effective_mode_at(self, index, fallback)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HistoryTailSelection {
    start: usize,
    budget: protocol::HistoryTailBudget,
}

fn select_history_tail(
    items: &[HistoryItem],
    max_items: usize,
    max_bytes: Option<usize>,
) -> Result<HistoryTailSelection, String> {
    let mut start = items.len();
    let mut budget = protocol::HistoryTailBudget::new(max_items, max_bytes);
    for (index, item) in items.iter().enumerate().rev() {
        if !budget
            .try_prepend(item)
            .map_err(|err| format!("serialize session history item: {err}"))?
        {
            break;
        }
        start = index;
    }
    Ok(HistoryTailSelection { start, budget })
}

pub fn bounded_history_tail(
    items: &[HistoryItem],
    max_items: usize,
    max_bytes: Option<usize>,
) -> Result<Vec<HistoryItem>, String> {
    let selection = select_history_tail(items, max_items, max_bytes)?;
    Ok(items[selection.start..].to_vec())
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
    use protocol::HistoryAppendPlan;

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
                checkpoint_events_json: None,
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
            transcript_records: None,
        };
        db.apply_session_commit(&command)
            .expect("seed session store");
    }

    fn apply_planned_append(
        history: &mut Vec<HistoryItem>,
        append: &protocol::HistoryAppend,
        plan: HistoryAppendPlan,
    ) -> protocol::HistoryAppendResult {
        match plan {
            HistoryAppendPlan::Unchanged => {}
            HistoryAppendPlan::Push => history.push(append.item.clone()),
            HistoryAppendPlan::ReplaceLast => {
                *history
                    .last_mut()
                    .expect("replace-last plan requires history") = append.item.clone();
            }
            HistoryAppendPlan::RemoveLast => {
                history.pop().expect("remove-last plan requires history");
            }
        }
        plan.result()
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
                authoritative_context_tokens: None,
                display_context_tokens: None,
                history_len: Some(2),
                checkpoint: None,
                checkpoint_events: Vec::new(),
                text_bytes: None,
            },
            history_len: 2,
            revision: 1,
            degraded_warnings: Vec::new(),
        };
        let mut live = LiveSession::from_store(
            header,
            SessionStoreRef::legacy(dir.path().to_path_buf(), db_path),
        );
        live.append_history(HistoryItem::user(protocol::Content::text("three")));

        let rows = live.history_range(1..3).expect("range");
        assert_eq!(rows.len(), 2);
        assert_eq!(live.history_len(), 3);
        assert_eq!(
            live.history_tail(2, None).unwrap(),
            vec![
                HistoryItem::user(protocol::Content::text("two")),
                HistoryItem::user(protocol::Content::text("three")),
            ]
        );
        let newest = HistoryItem::user(protocol::Content::text("three"));
        let newest_bytes = serde_json::to_vec(&newest).unwrap().len();
        assert_eq!(
            live.history_tail(3, Some(newest_bytes)).unwrap(),
            vec![newest]
        );
        assert!(live
            .history_tail(3, Some(newest_bytes.saturating_sub(1)))
            .unwrap()
            .is_empty());
        let source = live.model_history_source(None);
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
        let source = live.model_history_source(Some(&checkpoint));
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
                authoritative_context_tokens: None,
                display_context_tokens: None,
                history_len: Some(3),
                checkpoint: None,
                checkpoint_events: Vec::new(),
                text_bytes: None,
            },
            history_len: 3,
            revision: 1,
            degraded_warnings: Vec::new(),
        };
        let live = LiveSession::from_store(
            header,
            SessionStoreRef::legacy(dir.path().to_path_buf(), db_path),
        );

        assert!(live.any_transcript_visible_before(2).unwrap());
        assert_eq!(live.effective_mode_at(3, "normal").unwrap(), "plan");
        assert_eq!(live.effective_mode_at(0, "normal").unwrap(), "normal");
    }

    #[test]
    fn store_backed_history_append_plans_exact_semantic_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("session.db");
        let mut db = smelt_store::SessionDb::open(&db_path).expect("open db");
        let mut history = vec![HistoryItem::note(protocol::HistoryNote::context(
            "old context",
        ))];
        history.extend(
            (0..130).map(|index| HistoryItem::user(protocol::Content::text(index.to_string()))),
        );
        seed_store(&mut db, "append-plan", Some("normal"), history.clone());
        let header = SessionHeader {
            meta: crate::session::SessionMeta {
                id: "append-plan".into(),
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
                authoritative_context_tokens: None,
                display_context_tokens: None,
                history_len: Some(history.len()),
                checkpoint: None,
                checkpoint_events: Vec::new(),
                text_bytes: None,
            },
            history_len: history.len(),
            revision: 1,
            degraded_warnings: Vec::new(),
        };
        let mut live = LiveSession::from_store(
            header,
            SessionStoreRef::legacy(dir.path().to_path_buf(), db_path),
        );

        let duplicate_context = protocol::HistoryAppend::set_context(
            HistoryItem::note(protocol::HistoryNote::context("old context")),
            protocol::DEFAULT_CONTEXT_NOTE_NAME,
        );
        assert_eq!(
            live.plan_history_append(&duplicate_context).unwrap(),
            HistoryAppendPlan::Unchanged
        );

        let context = protocol::HistoryAppend::set_context(
            HistoryItem::note(protocol::HistoryNote::context("new context")),
            protocol::DEFAULT_CONTEXT_NOTE_NAME,
        );
        let context_plan = live.plan_history_append(&context).unwrap();
        assert_eq!(context_plan, HistoryAppendPlan::Push);
        let mut canonical = history.clone();
        let canonical_result = protocol::apply_history_append(&mut canonical, &context);
        let mut planned = history.clone();
        let planned_result = apply_planned_append(&mut planned, &context, context_plan);
        assert_eq!(planned_result, canonical_result);
        assert_eq!(planned, canonical);

        let plan = protocol::HistoryAppend::mode_change(
            HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal",
                "plan",
                "plan mode",
            )),
            protocol::AgentMode::normal(),
        );
        let mode_plan = live.plan_history_append(&plan).unwrap();
        assert_eq!(mode_plan, HistoryAppendPlan::Push);
        let mut canonical = history.clone();
        let canonical_result = protocol::apply_history_append(&mut canonical, &plan);
        let mut planned = history.clone();
        let planned_result = apply_planned_append(&mut planned, &plan, mode_plan);
        assert_eq!(planned_result, canonical_result);
        assert_eq!(planned, canonical);
        history = planned;
        live.append_history(plan.item.clone());

        let normal = protocol::HistoryAppend::mode_change(
            HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal",
                "normal",
                "normal mode",
            )),
            protocol::AgentMode::normal(),
        );
        let normal_plan = live.plan_history_append(&normal).unwrap();
        assert_eq!(normal_plan, HistoryAppendPlan::RemoveLast);
        let mut canonical = history.clone();
        let canonical_result = protocol::apply_history_append(&mut canonical, &normal);
        let mut planned = history;
        let planned_result = apply_planned_append(&mut planned, &normal, normal_plan);
        assert_eq!(planned_result, canonical_result);
        assert_eq!(planned, canonical);
    }
}
