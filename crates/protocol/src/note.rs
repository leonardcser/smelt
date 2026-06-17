/// Marker prefix on synthetic user messages that announce a mode change.
/// The TUI's set_mode handler emits these; the transcript renderer keys
/// on the prefix to display the note as a small inline pill instead of
/// a chat block. Source-of-truth for both writers and readers; bytes
/// must stay stable so the prefix doesn't bust the prompt cache.
pub const MODE_NOTE_PREFIX: &str = "[smelt:mode] ";

/// Build a synthetic user-note text appended to history when the agent's
/// mode switches. The human-facing body comes from the Lua mode registry;
/// this wrapper keeps the model-visible marker stable.
pub fn mode_change_note(note: &str) -> String {
    format!("{MODE_NOTE_PREFIX}{}", note.trim())
}

/// Marker prefix on synthetic user messages that announce durable runtime
/// context (cwd, managed worktree facts, or other app-visible state) without
/// changing the cacheable system prompt.
pub const CONTEXT_NOTE_PREFIX: &str = "[smelt:context] ";

/// Build a synthetic user-note text appended to history when app context that
/// the model needs to know changes. The wrapper keeps the model-visible marker
/// stable while the body carries the dynamic facts.
pub fn context_note(note: &str) -> String {
    format!("{CONTEXT_NOTE_PREFIX}{}", note.trim())
}

/// Marker prefix on synthetic user messages that announce a background process
/// completion. The transcript renderer keys on this so persisted/resumed notes
/// remain process-status rows instead of being rebuilt as chat blocks.
pub const PROCESS_STATUS_NOTE_PREFIX: &str = "[smelt:process] ";

/// Build a synthetic user-note text appended to history when a background
/// process exits. The visible body is kept separate from its stable marker so
/// session restore can recover the original transcript block kind.
pub fn process_status_note(note: &str) -> String {
    format!("{PROCESS_STATUS_NOTE_PREFIX}{}", note.trim())
}
