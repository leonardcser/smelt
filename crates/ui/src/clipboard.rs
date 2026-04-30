//! Clipboard subsystem: kill ring + platform sink.
//!
//! The whole runtime shares one `Clipboard`. Vim and emacs yank/paste
//! sites borrow it directly so the same kill ring backs the prompt,
//! the transcript, dialog inputs, and any future Lua callers. A thin
//! `Sink` trait abstracts the platform side — `tui` plugs in the
//! subprocess-backed system sink, headless / tests plug in
//! `NullSink` (or a memory-backed test sink).

use crate::kill_ring::KillRing;

/// Read/write handle on the underlying system clipboard. Implementors
/// shell out to `pbcopy` / `wl-copy` / `xclip` (in the binary), or
/// store text in memory (in tests).
pub trait Sink {
    /// Read the current clipboard text, if any. `None` on failure or
    /// if the clipboard holds non-text data.
    fn read(&mut self) -> Option<String>;

    /// Write `text` to the clipboard. Errors are surfaced as strings
    /// so the UI crate stays free of platform-specific error types.
    fn write(&mut self, text: &str) -> Result<(), String>;
}

/// No-op sink. `read` always returns `None`, `write` always succeeds
/// without doing anything. Used as the default sink in headless / test
/// constructors and as the temporary sink for `Clipboard::swap_sink`.
pub struct NullSink;

impl Sink for NullSink {
    fn read(&mut self) -> Option<String> {
        None
    }
    fn write(&mut self, _text: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Unified clipboard subsystem: kill ring + platform sink.
///
/// `kill_ring` is the emacs/vim register store with linewise tracking
/// and yank-pop history. `sink` is the platform write-through that
/// makes yanks visible to other apps (and the source we read from on
/// paste to detect external updates). Yank ops mutate `kill_ring`
/// then push the same text through `sink`; paste ops reconcile the
/// two before reading.
pub struct Clipboard {
    pub kill_ring: KillRing,
    sink: Box<dyn Sink + Send>,
}

impl Clipboard {
    pub fn new(sink: Box<dyn Sink + Send>) -> Self {
        Self {
            kill_ring: KillRing::new(),
            sink,
        }
    }

    /// Headless / test-friendly constructor backed by `NullSink`.
    pub fn null() -> Self {
        Self::new(Box::new(NullSink))
    }

    pub fn read(&mut self) -> Option<String> {
        self.sink.read()
    }

    pub fn write(&mut self, text: &str) -> Result<(), String> {
        self.sink.write(text)
    }

    /// Replace the platform sink and return the previous one. Used by
    /// the transcript yank path to mute system-clipboard writes during
    /// vim dispatch — vim's `yank_range` populates the kill ring with
    /// the *raw* source range, the caller then pushes the *rendered*
    /// version after vim returns.
    pub fn swap_sink(&mut self, sink: Box<dyn Sink + Send>) -> Box<dyn Sink + Send> {
        std::mem::replace(&mut self.sink, sink)
    }
}
