//! Persistent message log — the nvim `:messages` model.
//!
//! Lua errors, deferred warnings, and any other diagnostic that
//! shouldn't disappear with the next toast land here. Toasts get a
//! one-line summary; the full body (multi-line tracebacks, source
//! context) stays addressable via `/messages` and the
//! `smelt.messages.*` Lua bindings.

use std::time::SystemTime;

/// Capacity guard. Append-only ring; on overflow we drop the oldest
/// entries so the log stays bounded.
const MAX_ENTRIES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Warning,
    Error,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Info => "info",
            MessageKind::Warning => "warn",
            MessageKind::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessageEntry {
    pub kind: MessageKind,
    /// Coarse origin label (`"lua"`, `"engine"`, …). Free-form;
    /// shown verbatim in the `/messages` overlay.
    pub source: String,
    /// First line — what shows up in the toast.
    pub summary: String,
    /// Full body. May be the same string as `summary` for short
    /// messages; for Lua tracebacks it is the multi-line form.
    pub full: String,
    /// Wall-clock timestamp at append.
    pub ts: SystemTime,
}

/// Persistent message buffer. Locked behind a `Mutex` so
/// `record_error` (which holds only `&self` on `LuaRuntime`) can
/// append without re-entering `&mut Core`.
pub struct Messages {
    entries: Vec<MessageEntry>,
    /// Errors appended since the last `mark_read`. Drives the
    /// statusline indicator; `/messages` clears it on open.
    unread_errors: usize,
}

impl Default for Messages {
    fn default() -> Self {
        Self::new()
    }
}

impl Messages {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            unread_errors: 0,
        }
    }

    pub fn append(&mut self, kind: MessageKind, source: String, full: String) {
        let summary = full.lines().next().unwrap_or("").to_string();
        self.entries.push(MessageEntry {
            kind,
            source,
            summary,
            full,
            ts: SystemTime::now(),
        });
        if matches!(kind, MessageKind::Error) {
            self.unread_errors = self.unread_errors.saturating_add(1);
        }
        if self.entries.len() > MAX_ENTRIES {
            let drop = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(0..drop);
        }
    }

    pub fn entries(&self) -> &[MessageEntry] {
        &self.entries
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }

    pub fn unread_errors(&self) -> usize {
        self.unread_errors
    }

    pub fn mark_read(&mut self) {
        self.unread_errors = 0;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.unread_errors = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_splits_summary_from_full() {
        let mut m = Messages::new();
        m.append(
            MessageKind::Error,
            "lua".into(),
            "first line\nsecond line\nthird".into(),
        );
        assert_eq!(m.entries()[0].summary, "first line");
        assert_eq!(m.entries()[0].full, "first line\nsecond line\nthird");
    }

    #[test]
    fn unread_only_counts_errors() {
        let mut m = Messages::new();
        m.append(MessageKind::Info, "x".into(), "hi".into());
        m.append(MessageKind::Error, "x".into(), "boom".into());
        m.append(MessageKind::Warning, "x".into(), "warn".into());
        assert_eq!(m.unread_errors(), 1);
        m.mark_read();
        assert_eq!(m.unread_errors(), 0);
    }

    #[test]
    fn ring_caps_at_max() {
        let mut m = Messages::new();
        for i in 0..MAX_ENTRIES + 50 {
            m.append(MessageKind::Info, "x".into(), format!("msg {i}"));
        }
        assert_eq!(m.entries().len(), MAX_ENTRIES);
        assert_eq!(m.entries()[0].full, format!("msg {}", 50));
    }
}
