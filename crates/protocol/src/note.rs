use smelt_buffer::text;

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
    format!("{MODE_NOTE_PREFIX}{}", text::trim_whitespace(note))
}

/// Marker prefix on synthetic user messages that announce durable runtime
/// context (cwd, managed worktree facts, or other app-visible state) without
/// changing the cacheable system prompt.
pub const CONTEXT_NOTE_PREFIX: &str = "[smelt:context] ";

/// Build a synthetic user-note text appended to history when app context that
/// the model needs to know changes. Each note supersedes earlier session context
/// without rewriting the canonical history prefix.
pub fn context_note(note: &str) -> String {
    format!(
        "{CONTEXT_NOTE_PREFIX}Session context replaces earlier session context:\n{}",
        text::trim_whitespace(note)
    )
}

/// Build a model-visible tombstone for session context that is no longer active.
pub fn cleared_session_context_note() -> String {
    format!(
        "{CONTEXT_NOTE_PREFIX}Session context is no longer active. Ignore earlier session context."
    )
}

/// Build a model-visible update for independently managed named context.
pub fn named_context_note(name: &str, note: &str) -> String {
    let name = serde_json::to_string(name).expect("serialize context note name");
    format!(
        "{CONTEXT_NOTE_PREFIX}Named context {name} replaces earlier context with the same name:\n{}",
        text::trim_whitespace(note)
    )
}

/// Build a model-visible tombstone for named context that is no longer active.
pub fn cleared_context_note(name: &str) -> String {
    let name = serde_json::to_string(name).expect("serialize context note name");
    format!(
        "{CONTEXT_NOTE_PREFIX}Named context {name} is no longer active. Ignore earlier context with this name."
    )
}

/// Marker prefix on synthetic user messages that announce a background process
/// completion. The transcript renderer keys on this so persisted/resumed notes
/// remain process-status rows instead of being rebuilt as chat blocks.
pub const PROCESS_STATUS_NOTE_PREFIX: &str = "[smelt:process] ";

/// Build a synthetic user-note text appended to history when a background
/// process exits. The visible body is kept separate from its stable marker so
/// session restore can recover the original transcript block kind.
pub fn process_status_note(note: &str) -> String {
    format!(
        "{PROCESS_STATUS_NOTE_PREFIX}{}",
        text::trim_whitespace(note)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_trimming_keeps_graphemes_atomic() {
        let body = " \u{301}status\u{600} ";
        assert_eq!(mode_change_note(body), format!("{MODE_NOTE_PREFIX}{body}"));
        assert_eq!(
            process_status_note(body),
            format!("{PROCESS_STATUS_NOTE_PREFIX}{body}")
        );
    }
}
