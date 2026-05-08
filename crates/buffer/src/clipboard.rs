//! Clipboard subsystem: kill ring + platform sink.

use super::kill_ring::KillRing;
use base64::Engine;

/// Abstraction over the platform clipboard.
pub trait Sink {
    /// Read the current clipboard text. `None` on failure or non-text data.
    fn read(&mut self) -> Option<String>;

    /// Write `text` to the clipboard.
    fn write(&mut self, text: &str) -> Result<(), String>;
}

/// No-op sink for headless / test use and as a temporary placeholder in `swap_sink`.
pub struct NullSink;

impl Sink for NullSink {
    fn read(&mut self) -> Option<String> {
        None
    }
    fn write(&mut self, _text: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Unified clipboard: kill ring + platform sink.
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

    /// Constructor backed by `NullSink`.
    pub fn null() -> Self {
        Self::new(Box::new(NullSink))
    }

    pub fn read(&mut self) -> Option<String> {
        self.sink.read()
    }

    pub fn write(&mut self, text: &str) -> Result<(), String> {
        self.sink.write(text)
    }

    /// Replace the platform sink and return the previous one.
    pub fn swap_sink(&mut self, sink: Box<dyn Sink + Send>) -> Box<dyn Sink + Send> {
        std::mem::replace(&mut self.sink, sink)
    }
}

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

/// `Sink` backed by platform subprocess helpers (`pbcopy`/`pbpaste`, etc.).
pub struct SystemSink;

impl Sink for SystemSink {
    fn read(&mut self) -> Option<String> {
        paste_from_clipboard()
    }
    fn write(&mut self, text: &str) -> Result<(), String> {
        copy_to_clipboard(text)
    }
}

/// OSC 52 clipboard sink. Writes `\x1b]52;c;<base64>\x07` to stdout;
/// works over SSH/tmux with terminals that support OSC 52. Read falls
/// back to subprocess helpers.
pub struct Osc52Sink;

impl Sink for Osc52Sink {
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
