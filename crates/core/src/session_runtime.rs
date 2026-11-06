use crate::session::{ContextCheckpoint, Session, SessionStoreAddress};
use protocol::{history_item_message_count, HistoryItem};
use std::ops::Range;
use std::path::{Path, PathBuf};

struct LiveStoreReader(smelt_store::SessionReader);

impl LiveStoreReader {
    fn read_history_items_range(
        &self,
        range: Range<usize>,
    ) -> smelt_store::Result<Vec<HistoryItem>> {
        self.0.history_range(range.start as u64, range.end as u64)
    }

    fn read_history_items_tail(
        &self,
        end: usize,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> smelt_store::Result<Vec<HistoryItem>> {
        self.0.history_tail(end, max_items, max_bytes)
    }

    fn history_note_projection_at(
        &self,
        index: usize,
    ) -> smelt_store::Result<Option<protocol::HistoryNoteProjection>> {
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
        let _perf = smelt_perf::perf::begin("session:history_semantic:context_note");
        Ok(self
            .0
            .history_last_context_note_index_before(end as u64, name)?
            .map(|index| index as usize))
    }

    fn history_any_transcript_visible_before(&self, end: usize) -> smelt_store::Result<bool> {
        self.0.history_any_transcript_visible_before(end as u64)
    }
}

/// A lightweight navigation row, maintained when history changes.
#[derive(Clone, Debug)]
pub struct HistoryUserTurn {
    pub history_idx: usize,
    pub label: String,
    ordinal: usize,
}

pub const REWIND_LABEL_BYTES: usize = 512;

pub fn rewind_label(item: &HistoryItem) -> Option<String> {
    let HistoryItem::User {
        content, display, ..
    } = item
    else {
        return None;
    };
    if !matches!(
        protocol::classify_user_history_content(content),
        protocol::UserHistoryContent::Plain
    ) {
        return None;
    }
    let text = display
        .as_deref()
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| content.text_content());
    let first_line = smelt_buffer::text::slice(&text, 0..REWIND_LABEL_BYTES)
        .lines()
        .next()
        .unwrap_or("");
    let images = content.image_labels();
    let label = if images.is_empty() || text.contains('\n') {
        first_line.to_owned()
    } else if first_line.is_empty() {
        images.join(" ")
    } else {
        format!("{first_line} {}", images.join(" "))
    };
    Some(smelt_buffer::text::slice(&label, 0..REWIND_LABEL_BYTES).to_owned())
}

/// Mode preparation separates live lookup from the indexed stored reads it needs.
#[derive(Clone, Debug)]
pub enum EffectiveModePlan {
    Ready(String),
    Stored {
        before: usize,
        base_range: Range<usize>,
        fallback: String,
    },
}

impl EffectiveModePlan {
    pub fn resolve(self, reader: &smelt_store::SessionReader) -> Result<String, String> {
        match self {
            Self::Ready(mode) => Ok(mode),
            Self::Stored {
                before,
                base_range,
                fallback,
            } => {
                if before > 0 {
                    if let Some(mode) = reader
                        .history_mode_before(before as u64)
                        .map_err(|error| format!("read session history mode: {error}"))?
                    {
                        return Ok(mode);
                    }
                }
                if !base_range.is_empty() {
                    if let Some(mode) = reader
                        .history_base_mode_range(base_range.start as u64..base_range.end as u64)
                        .map_err(|error| format!("read session history base mode: {error}"))?
                    {
                        return Ok(mode);
                    }
                }
                Ok(fallback)
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HistoryNavigation {
    stored_user_count: usize,
    users: std::collections::BTreeMap<usize, HistoryUserTurn>,
    extra_message_totals: std::collections::BTreeMap<usize, usize>,
    modes: std::collections::BTreeMap<usize, String>,
    base_modes: std::collections::BTreeMap<usize, String>,
}

impl HistoryNavigation {
    pub fn from_history(history: &[HistoryItem]) -> Self {
        let mut index = Self::default();
        for (coordinate, item) in history.iter().enumerate() {
            index.append(coordinate, item);
        }
        index
    }

    pub fn user_turn_count(&self) -> usize {
        self.stored_user_count + self.users.len()
    }

    /// Exact count for a boundary covered by this navigation index.
    pub fn user_turn_count_before(&self, end: usize) -> usize {
        self.users
            .range(..end)
            .next_back()
            .map_or(self.stored_user_count, |(_, turn)| turn.ordinal)
    }

    /// Exact provider-message count for a navigation index covering materialized history.
    pub fn materialized_message_count(&self, history_len: usize) -> usize {
        history_len.saturating_add(
            self.extra_message_totals
                .last_key_value()
                .map_or(0, |(_, count)| *count),
        )
    }

    pub fn append(&mut self, coordinate: usize, item: &HistoryItem) {
        let extra_messages = history_item_message_count(item).saturating_sub(1);
        if extra_messages > 0 {
            let total = self
                .extra_message_totals
                .last_key_value()
                .map_or(0, |(_, count)| *count);
            self.extra_message_totals
                .insert(coordinate, total.saturating_add(extra_messages));
        }
        if let Some(label) = rewind_label(item) {
            debug_assert!(self
                .users
                .last_key_value()
                .is_none_or(|(last, _)| *last < coordinate));
            let ordinal = self.user_turn_count() + 1;
            self.users.insert(
                coordinate,
                HistoryUserTurn {
                    history_idx: coordinate,
                    label,
                    ordinal,
                },
            );
        }
        if let Some(note) = item.as_note() {
            if let Some(mode) = note.mode() {
                self.modes.insert(coordinate, mode.to_owned());
            }
            if let Some(mode) = note.base_mode() {
                self.base_modes.insert(coordinate, mode.to_owned());
            }
        }
    }

    pub fn truncate(&mut self, end: usize) {
        self.users.split_off(&end);
        self.extra_message_totals.split_off(&end);
        self.modes.split_off(&end);
        self.base_modes.split_off(&end);
    }

    fn compact(&mut self, start: usize) {
        self.stored_user_count = self.user_turn_count_before(start);
        self.users = self.users.split_off(&start);
        self.extra_message_totals = self.extra_message_totals.split_off(&start);
        self.modes = self.modes.split_off(&start);
        self.base_modes = self.base_modes.split_off(&start);
    }

    pub fn user_turns_before(
        &self,
        before: usize,
        limit: usize,
    ) -> impl Iterator<Item = &HistoryUserTurn> {
        self.users
            .range(..before)
            .rev()
            .map(|(_, turn)| turn)
            .take(limit)
            .inspect(|_| smelt_perf::perf::record_value("session:navigation:user_rows", 1))
    }

    pub fn effective_mode_plan(
        &self,
        end: usize,
        stored_end: usize,
        fallback: &str,
    ) -> EffectiveModePlan {
        if let Some((_, mode)) = self.modes.range(..end).next_back() {
            return EffectiveModePlan::Ready(mode.clone());
        }
        let fallback = self
            .base_modes
            .range(end..)
            .next()
            .map_or(fallback, |(_, mode)| mode)
            .to_owned();
        let before = end.min(stored_end);
        let base_range = end.min(stored_end)..stored_end;
        if before == 0 && base_range.is_empty() {
            EffectiveModePlan::Ready(fallback)
        } else {
            EffectiveModePlan::Stored {
                before,
                base_range,
                fallback,
            }
        }
    }
}

/// The history of an open session. The model context since the last checkpoint
/// stays in memory, so requests never read storage; older history is read from
/// the store when the user looks at it.
#[derive(Clone, Debug)]
pub struct LiveSession {
    id: String,
    artifact_dir: PathBuf,
    pub store_address: Option<SessionStoreAddress>,
    /// History index of `live_history[0]`. Earlier items are only in the store.
    pub live_start: usize,
    live_history: Vec<HistoryItem>,
    navigation: HistoryNavigation,
}

impl LiveSession {
    /// A resumed session holding `context`, the history from `context_start` on.
    pub fn from_resume(
        store: SessionStoreAddress,
        context_start: usize,
        context: Vec<HistoryItem>,
        user_turn_count: usize,
    ) -> Self {
        let mut live = Self::from_parts(
            store.session_id.clone(),
            store.artifact_dir(),
            Some(store),
            context_start,
            user_turn_count,
        );
        for item in context {
            live.append_history(item);
        }
        live
    }

    /// A session whose first `history_len` items, holding `user_turn_count`
    /// user turns, are only in the store.
    pub fn from_parts(
        id: String,
        artifact_dir: PathBuf,
        store_address: Option<SessionStoreAddress>,
        history_len: usize,
        user_turn_count: usize,
    ) -> Self {
        Self {
            id,
            artifact_dir,
            store_address,
            live_start: history_len,
            live_history: Vec::new(),
            navigation: HistoryNavigation {
                stored_user_count: user_turn_count,
                ..Default::default()
            },
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn artifact_dir(&self) -> &Path {
        &self.artifact_dir
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

    pub fn append_history(&mut self, item: HistoryItem) -> usize {
        let idx = self.history_len();
        if self.live_history.is_empty() {
            self.live_start = idx;
        }
        self.navigation.append(idx, &item);
        self.live_history.push(item);
        idx
    }

    pub fn live_history(&self) -> &[HistoryItem] {
        &self.live_history
    }

    pub fn navigation(&self) -> &HistoryNavigation {
        &self.navigation
    }

    /// A count covered by live navigation needs no storage preparation.
    pub fn local_user_turn_count_before(&self, end: usize) -> Option<usize> {
        if end == 0 {
            return Some(0);
        }
        (end >= self.live_start).then(|| {
            self.navigation
                .user_turn_count_before(end.min(self.history_len()))
        })
    }

    /// Explicit synchronous mutations use a bounded stored lookup when the prefix is cold.
    pub fn user_turn_count_before(&self, end: usize) -> Result<usize, String> {
        if let Some(count) = self.local_user_turn_count_before(end) {
            return Ok(count);
        }
        let count = self
            .open_store()?
            .0
            .history_user_turn_count_before(end as u64)
            .map_err(|error| format!("read session user count: {error}"))?;
        usize::try_from(count).map_err(|_| "session user count exceeds platform limits".into())
    }

    /// Stored-prefix truncation requires its count prepared before any mutation.
    pub fn truncate_from(&mut self, index: usize, user_turn_count: Option<usize>) {
        let index = index.min(self.history_len());
        let count = self
            .local_user_turn_count_before(index)
            .or(user_turn_count)
            .expect("stored truncation requires a prepared user count");
        self.navigation.truncate(index);
        if index >= self.live_start {
            self.live_history.truncate(index - self.live_start);
        } else {
            self.navigation.stored_user_count = count;
            self.live_start = index;
            self.live_history.clear();
        }
        debug_assert_eq!(self.navigation.user_turn_count(), count);
    }

    /// Drops items before `index` from memory. They must already be stored.
    pub fn release_before(&mut self, index: usize) {
        let index = index.min(self.history_len());
        if index <= self.live_start {
            return;
        }
        self.navigation.compact(index);
        self.live_history.drain(..index - self.live_start);
        self.live_start = index;
    }

    /// Reads stored items so that memory holds history from `index` on.
    pub fn load_from(&mut self, index: usize) -> Result<(), String> {
        if index >= self.live_start {
            return Ok(());
        }
        let _perf = smelt_perf::perf::begin("session:live:load_from");
        let store = self.open_store()?;
        let (count, mut items) = store
            .0
            .read_snapshot(|reader| {
                Ok((
                    reader.history_user_turn_count_before(index as u64)?,
                    reader.history_range(index as u64, self.live_start as u64)?,
                ))
            })
            .map_err(|err| format!("read session history: {err}"))?;
        if items.len() != self.live_start - index {
            return Err(format!(
                "read session history {index}..{}: found {} items",
                self.live_start,
                items.len()
            ));
        }
        items.append(&mut self.live_history);
        self.live_start = index;
        self.navigation = HistoryNavigation {
            stored_user_count: count as usize,
            ..Default::default()
        };
        for (offset, item) in items.iter().enumerate() {
            self.navigation.append(index + offset, item);
        }
        self.live_history = items;
        Ok(())
    }

    pub fn history_range(&self, range: Range<usize>) -> Result<Vec<HistoryItem>, String> {
        self.history_range_read(range).read()
    }

    pub fn scan_history(
        &self,
        range: Range<usize>,
        chunk_items: usize,
        mut visit: impl FnMut(usize, &[HistoryItem]),
    ) -> Result<(), String> {
        let end = range.end.min(self.history_len());
        let mut start = range.start.min(end);
        if start == end {
            return Ok(());
        }
        let chunk_items = chunk_items.max(1);

        let stored_end = end.min(self.live_start);
        if start < stored_end {
            let db = self.open_store()?;
            while start < stored_end {
                let chunk_end = start.saturating_add(chunk_items).min(stored_end);
                let items = db
                    .read_history_items_range(start..chunk_end)
                    .map_err(|err| format!("read session history range: {err}"))?;
                if items.len() != chunk_end - start {
                    return Err(format!(
                        "read session history range {start}..{chunk_end}: expected {} items, found {}",
                        chunk_end - start,
                        items.len()
                    ));
                }
                visit(start, &items);
                start = chunk_end;
            }
        }

        let live_start = start.max(self.live_start);
        if live_start < end {
            let offset_start = live_start - self.live_start;
            let offset_end = end - self.live_start;
            for (chunk_offset, items) in self.live_history[offset_start..offset_end]
                .chunks(chunk_items)
                .enumerate()
            {
                visit(
                    live_start.saturating_add(chunk_offset.saturating_mul(chunk_items)),
                    items,
                );
            }
        }
        Ok(())
    }

    pub fn history_tail(
        &self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<HistoryItem>, String> {
        self.history_tail_read(max_items, max_bytes)?.read()
    }

    /// Captures `history_range(range)`, to finish where storage reads may block.
    pub fn history_range_read(&self, range: Range<usize>) -> HistoryRead {
        let end = range.end.min(self.history_len());
        let start = range.start.min(end);
        let stored_end = end.min(self.live_start);
        let live_start = start.max(self.live_start);
        let live = if live_start < end {
            self.live_history[live_start - self.live_start..end - self.live_start].to_vec()
        } else {
            Vec::new()
        };
        HistoryRead {
            stored: (start < stored_end)
                .then(|| self.stored_read(StoredHistory::Range(start..stored_end))),
            live,
        }
    }

    /// Captures `history_tail(max_items, max_bytes)`, to finish where storage
    /// reads may block.
    pub fn history_tail_read(
        &self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<HistoryRead, String> {
        let mut read = HistoryRead::loaded(Vec::new());
        if max_items == 0 {
            return Ok(read);
        }
        let live = select_history_tail(&self.live_history, max_items, max_bytes)?;
        read.live = self.live_history[live.start..].to_vec();
        if !live.budget.limit_reached() && read.live.len() < max_items && self.live_start > 0 {
            read.stored = Some(self.stored_read(StoredHistory::Tail {
                end: self.live_start,
                max_items: max_items - read.live.len(),
                max_bytes: live.budget.remaining_bytes(),
            }));
        }
        Ok(read)
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

    pub fn effective_mode_plan(&self, hist_idx: usize, fallback: &str) -> EffectiveModePlan {
        self.navigation.effective_mode_plan(
            hist_idx.min(self.history_len()),
            self.live_start,
            fallback,
        )
    }

    pub fn effective_mode_at(&self, hist_idx: usize, fallback: &str) -> Result<String, String> {
        match self.effective_mode_plan(hist_idx, fallback) {
            EffectiveModePlan::Ready(mode) => Ok(mode),
            plan => self
                .open_store()?
                .0
                .read_snapshot(|reader| Ok(plan.resolve(reader)))
                .map_err(|error| format!("read session mode snapshot: {error}"))?,
        }
    }

    /// The history index where the model context starts.
    pub fn model_context_start(&self, checkpoint: Option<&ContextCheckpoint>) -> usize {
        checkpoint
            .map_or(0, |checkpoint| checkpoint.first_live_index)
            .min(self.history_len())
    }

    /// The provider history: the checkpoint summary, then the history since it.
    /// Context not in memory is read from storage.
    pub fn model_history_source(
        &self,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> Result<protocol::ModelHistorySource, String> {
        let first_live_index = self.model_context_start(checkpoint);
        if first_live_index < self.live_start {
            smelt_perf::perf::record_value("live_session:context_storage_read", 1);
        }
        let mut items = Vec::new();
        if let Some(checkpoint) = checkpoint {
            items.push(HistoryItem::user(protocol::compaction_summary_content(
                &checkpoint.summary,
            )));
        }
        let prefix_len = items.len();
        items.extend(self.history_range(first_live_index..self.history_len())?);
        Ok(protocol::ModelHistorySource::projected_items(
            items,
            protocol::ModelHistoryCoordinates::projected(prefix_len, first_live_index),
        ))
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
        open_store(self.store_address.as_ref(), self.id())
    }

    fn stored_read(&self, part: StoredHistory) -> StoredRead {
        StoredRead {
            session_id: self.id().to_owned(),
            store: self.store_address.clone(),
            part,
        }
    }
}

fn open_store(store: Option<&SessionStoreAddress>, id: &str) -> Result<LiveStoreReader, String> {
    let _perf = smelt_perf::perf::begin("session:live_store:open");
    let store = store.ok_or_else(|| format!("session {id} has no storage root"))?;
    store
        .reader()
        .map(LiveStoreReader)
        .map_err(|error| format!("open session {}: {error}", store.session_id))
}

/// A history read captured from a live session. Rows still in memory are
/// copied, so the rest can be read from storage on any thread.
pub struct HistoryRead {
    stored: Option<StoredRead>,
    live: Vec<HistoryItem>,
}

/// The part of a `HistoryRead` that precedes the rows in memory.
struct StoredRead {
    session_id: String,
    store: Option<SessionStoreAddress>,
    part: StoredHistory,
}

enum StoredHistory {
    Range(Range<usize>),
    Tail {
        end: usize,
        max_items: usize,
        max_bytes: Option<usize>,
    },
}

impl HistoryRead {
    pub fn loaded(items: Vec<HistoryItem>) -> Self {
        Self {
            stored: None,
            live: items,
        }
    }

    pub fn needs_storage(&self) -> bool {
        self.stored.is_some()
    }

    pub fn read(self) -> Result<Vec<HistoryItem>, String> {
        let Some(stored) = self.stored else {
            return Ok(self.live);
        };
        let db = open_store(stored.store.as_ref(), &stored.session_id)?;
        let mut items = match stored.part {
            StoredHistory::Range(range) => {
                let items = db
                    .read_history_items_range(range.clone())
                    .map_err(|err| format!("read session history range: {err}"))?;
                if items.len() != range.len() {
                    return Err(format!(
                        "read session history range {}..{}: expected {} items, found {}",
                        range.start,
                        range.end,
                        range.len(),
                        items.len()
                    ));
                }
                items
            }
            StoredHistory::Tail {
                end,
                max_items,
                max_bytes,
            } => db
                .read_history_items_tail(end, max_items, max_bytes)
                .map_err(|err| format!("read session history tail: {err}"))?,
        };
        items.extend(self.live);
        Ok(items)
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

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::HistoryAppendPlan;

    const SESSION_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn navigation_counts_follow_message_projection_and_suffix_replacement() {
        let tool_step = HistoryItem::Assistant(protocol::AssistantStep {
            content: None,
            reasoning: None,
            reasoning_blocks: Vec::new(),
            invocations: (0..3)
                .map(|index| protocol::ToolInvocation {
                    call_id: index.to_string(),
                    name: "test".into(),
                    arguments: "{}".into(),
                    result: protocol::ToolOutcome::new("result".into(), false, None),
                    elapsed_ms: None,
                    called_at_ms: None,
                    provider: None,
                })
                .collect(),
        });
        let history = [
            HistoryItem::System {
                content: protocol::Content::text("system"),
            },
            HistoryItem::user(protocol::Content::text("user")),
            tool_step.clone(),
            HistoryItem::Note(protocol::HistoryNote::context("context")),
            HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text("answer")),
                None,
                Vec::new(),
            )),
            HistoryItem::user(protocol::compaction_summary_content("summary")),
            HistoryItem::user(protocol::Content::text("follow-up")),
        ];
        let mut navigation = HistoryNavigation::default();
        for (index, item) in history.iter().enumerate() {
            navigation.append(index, item);
            assert_eq!(
                navigation.materialized_message_count(index + 1),
                protocol::history_to_messages(&history[..=index]).len()
            );
            assert_eq!(
                navigation.user_turn_count(),
                history[..=index].iter().filter_map(rewind_label).count()
            );
        }
        for end in 0..=history.len() {
            let mut retained = navigation.clone();
            retained.truncate(end);
            assert_eq!(
                retained.materialized_message_count(end),
                protocol::history_to_messages(&history[..end]).len()
            );
            assert_eq!(
                retained.user_turn_count(),
                history[..end].iter().filter_map(rewind_label).count()
            );
        }
        navigation.truncate(2);
        navigation.append(2, &tool_step);
        assert_eq!(navigation.materialized_message_count(3), 6);
        navigation.truncate(2);
        navigation.append(2, &history[4]);
        assert_eq!(navigation.materialized_message_count(3), 3);
    }

    fn seed_store(
        root: &std::path::Path,
        id: &str,
        mode: Option<&str>,
        history: Vec<HistoryItem>,
    ) -> SessionStoreAddress {
        let store = smelt_store::Store::new(root);
        store
            .writer(id)
            .and_then(|mut writer| {
                writer.commit(
                    &smelt_store::SessionIdentity {
                        id: id.into(),
                        created_at: 1,
                        parent_id: None,
                    },
                    &smelt_store::SessionCommit {
                        metadata: smelt_store::SessionMetadata {
                            mode: mode.map(str::to_owned),
                            updated_at: 2,
                            ..Default::default()
                        },
                        history: Some(smelt_store::HistorySuffix {
                            start: 0,
                            items: history,
                        }),
                        ..Default::default()
                    },
                )
            })
            .expect("seed session store");
        SessionStoreAddress::new(store, id.into())
    }

    fn stored_live(address: SessionStoreAddress, history_len: usize, users: usize) -> LiveSession {
        LiveSession::from_parts(
            SESSION_ID.into(),
            address.artifact_dir(),
            Some(address),
            history_len,
            users,
        )
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
        let persisted = vec![
            HistoryItem::user(protocol::Content::text("one")),
            HistoryItem::user(protocol::Content::text("two")),
        ];
        let address = seed_store(dir.path(), SESSION_ID, None, persisted);

        let mut live = stored_live(address, 2, 2);
        assert_eq!(live.navigation().user_turn_count(), 2);
        assert_eq!(live.local_user_turn_count_before(1), None);
        assert_eq!(live.user_turn_count_before(1).unwrap(), 1);
        live.append_history(HistoryItem::user(protocol::Content::text("three")));
        assert_eq!(live.navigation().user_turn_count(), 3);

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
        let checkpoint = ContextCheckpoint {
            summary: "summary".into(),
            first_live_index: 2,
            ..Default::default()
        };
        let source = live.model_history_source(Some(&checkpoint)).unwrap();
        assert_eq!(source.coordinates.canonical_start().get(), 2);
        assert_eq!(source.coordinates.model_prefix_len(), 1);
        assert_eq!(source.items.len(), 2);
        assert_eq!(live.live_start, 2);

        // Context before the memory window is read from the store, and stays
        // there until loaded.
        let source = live.model_history_source(None).unwrap();
        assert_eq!(source.items, live.history_range(0..3).unwrap());
        assert_eq!(live.live_start, 2);
        live.load_from(live.model_context_start(None)).unwrap();
        assert_eq!(live.live_start, 0);
        assert_eq!(live.model_history_source(None).unwrap().items, source.items);
        assert_eq!(live.local_user_turn_count_before(1), Some(1));
        assert_eq!(live.navigation().user_turn_count(), 3);

        live.release_before(2);
        assert_eq!(live.live_suffix_len(), 1);
        assert_eq!(live.navigation().user_turn_count(), 3);
        live.append_history(HistoryItem::user(protocol::compaction_summary_content(
            "summary",
        )));
        live.append_history(HistoryItem::user(protocol::Content::text("four")));
        assert_eq!(live.navigation().user_turn_count(), 4);
        assert_eq!(live.local_user_turn_count_before(4), Some(3));
        live.release_before(4);
        assert_eq!(live.live_suffix_len(), 1);
        assert_eq!(live.navigation().user_turn_count(), 4);
        live.truncate_from(4, None);
        assert_eq!(live.navigation().user_turn_count(), 3);
        live.append_history(HistoryItem::user(protocol::Content::text("replacement")));
        assert_eq!(live.navigation().user_turn_count(), 4);
        live.truncate_from(1, Some(1));
        assert_eq!(live.navigation().user_turn_count(), 1);
        live.append_history(HistoryItem::user(protocol::Content::text("new branch")));
        assert_eq!(live.navigation().user_turn_count(), 2);
        live.release_before(2);
        live.release_before(2);
        assert_eq!(live.navigation().user_turn_count(), 2);
        live.truncate_from(0, None);
        assert_eq!(live.navigation().user_turn_count(), 0);
    }

    #[test]
    fn store_backed_live_session_uses_indexed_mode_and_visibility() {
        let dir = tempfile::tempdir().expect("tempdir");
        let history = vec![
            HistoryItem::user(protocol::Content::text("hello")),
            HistoryItem::Note(protocol::HistoryNote::mode_change_for_transition(
                "normal", "plan", "switch",
            )),
            HistoryItem::user(protocol::Content::text("after mode")),
        ];
        let address = seed_store(dir.path(), SESSION_ID, Some("normal"), history);
        let live = stored_live(address, 3, 2);

        assert!(live.any_transcript_visible_before(2).unwrap());
        assert!(live.any_transcript_visible_before(3).unwrap());
        assert!(live.any_transcript_visible_before(usize::MAX).unwrap());
        assert!(!live.any_transcript_visible_before(0).unwrap());
        assert_eq!(live.effective_mode_at(3, "normal").unwrap(), "plan");
        assert_eq!(live.effective_mode_at(0, "normal").unwrap(), "normal");
    }

    #[test]
    fn store_backed_history_append_plans_exact_semantic_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut history = vec![HistoryItem::note(protocol::HistoryNote::context(
            "old context",
        ))];
        history.extend(
            (0..130).map(|index| HistoryItem::user(protocol::Content::text(index.to_string()))),
        );
        let address = seed_store(dir.path(), SESSION_ID, Some("normal"), history.clone());
        let mut live = stored_live(address, history.len(), 130);

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
