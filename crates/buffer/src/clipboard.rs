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

/// Encode `text` as the OSC 52 byte sequence terminals consume to update
/// the system clipboard: `\x1b]52;c;<base64>\x07`.
pub fn osc52_payload(text: &str) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{encoded}\x07").into_bytes()
}

impl Sink for Osc52Sink {
    fn read(&mut self) -> Option<String> {
        paste_from_clipboard()
    }
    fn write(&mut self, text: &str) -> Result<(), String> {
        use std::io::Write;
        let payload = osc52_payload(text);
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&payload).map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    // ── osc52_payload (pure encoder) ──────────────────────────────────────

    #[test]
    fn osc52_payload_wraps_text_in_escape_and_terminator() {
        let p = osc52_payload("hi");
        assert!(p.starts_with(b"\x1b]52;c;"), "got {p:?}");
        assert!(p.ends_with(b"\x07"), "got {p:?}");
    }

    #[test]
    fn osc52_payload_encodes_text_as_base64() {
        // "hello" → base64 "aGVsbG8="
        let p = osc52_payload("hello");
        let s = std::str::from_utf8(&p).unwrap();
        assert!(s.contains("aGVsbG8="), "got {s:?}");
    }

    #[test]
    fn osc52_payload_round_trips_unicode_text() {
        // Decode the base64 middle and confirm we get our input back.
        let text = "日本語 + emoji 🦀";
        let p = osc52_payload(text);
        let s = std::str::from_utf8(&p).unwrap();
        let inner = s.trim_start_matches("\x1b]52;c;").trim_end_matches('\x07');
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(inner)
            .unwrap();
        assert_eq!(std::str::from_utf8(&decoded).unwrap(), text);
    }

    #[test]
    fn osc52_payload_for_empty_text_is_just_the_envelope() {
        // Empty input → empty base64 → "\x1b]52;c;\x07".
        let p = osc52_payload("");
        assert_eq!(p, b"\x1b]52;c;\x07");
    }

    // ── Clipboard + Sink seam ─────────────────────────────────────────────

    struct CountingSink {
        writes: Arc<AtomicUsize>,
    }
    impl Sink for CountingSink {
        fn read(&mut self) -> Option<String> {
            None
        }
        fn write(&mut self, _text: &str) -> Result<(), String> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn clipboard_null_constructor_succeeds_and_reads_none() {
        let mut c = Clipboard::null();
        assert_eq!(c.read(), None);
        assert!(c.write("anything").is_ok());
    }

    #[test]
    fn clipboard_write_dispatches_to_installed_sink() {
        let writes = Arc::new(AtomicUsize::new(0));
        let mut c = Clipboard::new(Box::new(CountingSink {
            writes: writes.clone(),
        }));
        c.write("a").unwrap();
        c.write("b").unwrap();
        c.write("c").unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn swap_sink_redirects_subsequent_writes_to_the_new_sink() {
        let a_writes = Arc::new(AtomicUsize::new(0));
        let b_writes = Arc::new(AtomicUsize::new(0));
        let mut c = Clipboard::new(Box::new(CountingSink {
            writes: a_writes.clone(),
        }));
        c.write("to-a").unwrap();
        let _prev = c.swap_sink(Box::new(CountingSink {
            writes: b_writes.clone(),
        }));
        c.write("to-b").unwrap();
        c.write("to-b").unwrap();
        assert_eq!(a_writes.load(Ordering::SeqCst), 1);
        assert_eq!(b_writes.load(Ordering::SeqCst), 2);
    }
}
