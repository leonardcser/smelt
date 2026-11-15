use protocol::{HistoryChain, HistoryItem, HistoryNote, HistoryNoteKind, HistoryNoteProjection};
use rusqlite::{params, Connection, OptionalExtension};

use crate::document;
use crate::error::{Result, StoreError};

/// The columns that describe a history item next to its body.
pub(crate) struct HistoryRow<'a> {
    pub user_turn: bool,
    pub visible: bool,
    pub note_kind: Option<&'static str>,
    pub context: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub base_mode: Option<&'a str>,
}

impl<'a> HistoryRow<'a> {
    pub(crate) fn of(item: &'a HistoryItem) -> Self {
        let note = match item {
            HistoryItem::Note(note) => Some(note),
            _ => None,
        };
        Self {
            user_turn: is_user_turn(item),
            visible: !matches!(
                item,
                HistoryItem::System { .. } | HistoryItem::Note(HistoryNote::Context { .. })
            ),
            note_kind: note.map(|note| note_kind_name(note.kind())),
            context: note.and_then(HistoryNote::context_name),
            mode: note.and_then(HistoryNote::mode),
            base_mode: note.and_then(HistoryNote::base_mode),
        }
    }
}

/// A user message typed by the user, as opposed to synthetic user content.
pub fn is_user_turn(item: &HistoryItem) -> bool {
    matches!(item, HistoryItem::User { content, .. } if matches!(
        protocol::classify_user_history_content(content),
        protocol::UserHistoryContent::Plain
    ))
}

fn note_kind_name(kind: HistoryNoteKind) -> &'static str {
    match kind {
        HistoryNoteKind::ModeChange => "mode_change",
        HistoryNoteKind::Context => "context",
        HistoryNoteKind::ProcessStatus => "process_status",
        HistoryNoteKind::Compaction => "compaction",
    }
}

pub(crate) fn note_projection(
    kind: Option<String>,
    mode: Option<String>,
) -> Option<HistoryNoteProjection> {
    let kind = match kind.as_deref()? {
        "mode_change" => HistoryNoteKind::ModeChange,
        "context" => HistoryNoteKind::Context,
        "process_status" => HistoryNoteKind::ProcessStatus,
        "compaction" => HistoryNoteKind::Compaction,
        _ => return None,
    };
    Some(HistoryNoteProjection { kind, mode })
}

/// Conversation text of a history item: user input, assistant replies and
/// compaction summaries. Mode and process status notices are not conversation.
pub(crate) fn conversation_text(item: &HistoryItem) -> Option<std::borrow::Cow<'_, str>> {
    match item {
        HistoryItem::User { content, .. } => match protocol::classify_user_history_content(content)
        {
            protocol::UserHistoryContent::Plain => Some(content.text_content()),
            protocol::UserHistoryContent::CompactionSummary { summary } => Some(summary.into()),
            protocol::UserHistoryContent::ModeChange { .. }
            | protocol::UserHistoryContent::ProcessStatus { .. } => None,
        },
        HistoryItem::Assistant(step) => step.content.as_ref().map(|content| content.text_content()),
        HistoryItem::Note(HistoryNote::Compaction { text }) => Some(text.as_str().into()),
        HistoryItem::System { .. } | HistoryItem::Note(_) => None,
    }
}

/// The chain over the conversation in history `[0, end)` of session `sid`.
pub(crate) fn chain_before(conn: &Connection, sid: i64, end: i64) -> Result<HistoryChain> {
    if end <= 0 {
        return Ok(HistoryChain::default());
    }
    let latest: Option<(i64, Vec<u8>)> = conn
        .prepare_cached(
            "SELECT idx, chain FROM history
             WHERE sid = ?1 AND idx < ?2 AND chain IS NOT NULL ORDER BY idx DESC LIMIT 1",
        )?
        .query_row(params![sid, end], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let (from, chain) = match latest {
        Some((idx, bytes)) => (
            idx + 1,
            HistoryChain::from_bytes(&bytes).ok_or_else(|| {
                StoreError::Integrity(format!("history row {idx} has a bad chain"))
            })?,
        ),
        None => (0, HistoryChain::default()),
    };
    if from >= end {
        return Ok(chain);
    }
    // COMPAT(history-chain-column): rows from before the column, or from
    // earlier 0.8 releases, have no chain.
    let _perf = smelt_perf::perf::begin("store:history:legacy_chain");
    let mut statement = conn.prepare_cached(
        "SELECT body FROM history WHERE sid = ?1 AND idx >= ?2 AND idx < ?3 ORDER BY idx",
    )?;
    let mut rows = statement.query(params![sid, from, end])?;
    let mut chain = chain;
    while let Some(row) = rows.next()? {
        let item: HistoryItem = document::load_json(conn, &row.get::<_, Vec<u8>>(0)?)?;
        chain = chain.extend(&item);
    }
    Ok(chain)
}
