//! Clipboard subsystem: kill ring + platform sink.
//!
//! The whole runtime shares one `Clipboard`. Vim and emacs yank/paste
//! sites borrow it directly so the same kill ring backs the prompt,
//! the transcript, dialog inputs, and any future Lua callers. A thin
//! `Sink` trait abstracts the platform side — `tui` plugs in the
//! subprocess-backed system sink, headless / tests plug in
//! `NullSink` (or a memory-backed test sink).

use super::kill_ring::KillRing;
use base64::Engine;

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

/// Copy text to the system clipboard using platform commands.
///
/// Reached only through `SystemSink::write` — every clipboard write
/// in the runtime flows through `app.core.clipboard.write()` so vim,
/// emacs, transcript yank, and Lua `smelt.clipboard` share one path.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let (cmd, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if std::env::var("WAYLAND_DISPLAY").is_ok() {
        ("wl-copy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{cmd}: {e}"))?;

    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;

    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with {status}"))
    }
}

/// Read text from the system clipboard using platform commands.
/// Returns `None` when the platform helper fails or the clipboard is
/// empty / holds non-text data — callers should fall back to the kill
/// ring in that case.
///
/// Reached only through `SystemSink::read` — every clipboard read in
/// the runtime flows through `app.core.clipboard.read()`.
fn paste_from_clipboard() -> Option<String> {
    use std::process::{Command, Stdio};

    let (cmd, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbpaste", &[])
    } else if std::env::var("WAYLAND_DISPLAY").is_ok() {
        ("wl-paste", &["--no-newline"])
    } else {
        ("xclip", &["-selection", "clipboard", "-o"])
    };

    let output = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `crate::core::Sink` impl backed by the platform subprocess helpers. Owned
/// by the TuiApp-level `crate::core::Clipboard` so vim yank / paste sites push
/// through the same path the prompt and transcript already use.
pub(crate) struct SystemSink;

impl crate::core::Sink for SystemSink {
    fn read(&mut self) -> Option<String> {
        paste_from_clipboard()
    }
    fn write(&mut self, text: &str) -> Result<(), String> {
        copy_to_clipboard(text)
    }
}

/// OSC 52 clipboard sink: writes the terminal escape sequence
/// `\x1b]52;c;<base64>\x07` to stdout so the terminal copies the
/// text to the system clipboard. Works over SSH/tmux with modern
/// terminals (iTerm2, kitty, alacritty, foot, wezterm, tmux
/// `set-clipboard on`, etc.). Read falls back to subprocess helpers.
pub(crate) struct Osc52Sink;

impl crate::core::Sink for Osc52Sink {
    fn read(&mut self) -> Option<String> {
        paste_from_clipboard()
    }
    fn write(&mut self, text: &str) -> Result<(), String> {
        use std::io::Write;
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
            .map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())
    }
}
