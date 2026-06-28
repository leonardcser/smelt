//! Terminal mode setup, teardown, and shell-out suspension.
//!
//! The TUI lives inside an envelope of terminal modes (alt screen, raw mode,
//! mouse capture, bracketed paste, focus reporting, line wrap off, hidden
//! cursor). [`TuiTerminal`] is an RAII handle that owns that envelope -
//! claim it once at startup and the matching teardown runs on drop, including
//! on panic. [`TuiTerminal::suspended`] is the temporary handoff used for
//! shell-outs like `$EDITOR`.

use std::io::{self, BufWriter, Stdout};

use crossterm::event::KeyboardEnhancementFlags;
use smelt_term::{TerminalSession, TerminalSessionBuilder};

/// RAII guard for the TUI terminal envelope. Restores cooked mode + the
/// normal screen on drop, even if the run loop panics.
pub struct TuiTerminal {
    session: TerminalSession<BufWriter<Stdout>>,
    title_touched: bool,
    // Terminal state is a process-wide resource; the guard must not migrate
    // between threads while it's alive.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl TuiTerminal {
    pub fn claim() -> io::Result<Self> {
        let session = tui_session_builder().enter_stdout()?;
        Ok(Self {
            session,
            title_touched: false,
            _not_send: std::marker::PhantomData,
        })
    }

    pub fn write_control_sequence(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.session.write_control_sequence(bytes)
    }

    pub fn set_title_sequence(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.title_touched = true;
        self.write_control_sequence(bytes)
    }

    pub fn clear_title_sequence(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.title_touched = false;
        self.write_control_sequence(bytes)
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        self.session.size()
    }

    /// Hand the terminal to a child process for the duration of `f`, then
    /// take it back.
    ///
    /// We keep the alt screen during suspend: leaving it would flash the
    /// user's shell scrollback for the instant between our
    /// `LeaveAlternateScreen` and the child's own `EnterAlternateScreen`.
    /// Children that use the alt screen (vim/nvim/nano/helix) issue
    /// `DECSET 1049` themselves; children that don't will write directly
    /// into our alt buffer, which the caller rebuilds with `force_redraw`.
    ///
    /// This deliberately does **not** touch any active terminal input reader.
    /// Callers must stop or recreate their reader around suspension so it does
    /// not race the child process for stdin bytes.
    pub fn suspended<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.session.suspend(f)
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        if self.title_touched {
            let _ = self.session.write_control_sequence(b"\x1b]0;\x07");
        }
    }
}

fn tui_session_builder() -> TerminalSessionBuilder {
    TerminalSession::builder()
        .alternate_screen(true)
        .line_wrap(false)
        .hide_cursor(true)
        .bracketed_paste(true)
        .focus_events(true)
        .keyboard_enhancements(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        .mouse_capture(true)
}
