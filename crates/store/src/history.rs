use protocol::{HistoryItem, HistoryNote, HistoryNoteKind, HistoryNoteProjection};

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
        _ => return None,
    };
    Some(HistoryNoteProjection { kind, mode })
}

/// Conversation text of a history item: user input and assistant replies.
pub(crate) fn conversation_text(item: &HistoryItem) -> Option<std::borrow::Cow<'_, str>> {
    match item {
        HistoryItem::User { content, .. } => Some(content.text_content()),
        HistoryItem::Assistant(step) => step.content.as_ref().map(|content| content.text_content()),
        HistoryItem::System { .. } | HistoryItem::Note(_) => None,
    }
}
