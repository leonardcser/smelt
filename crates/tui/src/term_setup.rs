//! Terminal mode setup, teardown, and shell-out suspension.
//!
//! The TUI lives inside an envelope of terminal modes (alt screen, raw mode,
//! mouse capture, bracketed paste, focus reporting, line wrap off, hidden
//! cursor). [`TuiTerminal`] is an RAII handle that owns that envelope -
//! claim it once at startup and the matching teardown runs on drop, including
//! on panic. [`TuiTerminal::suspended`] is the temporary handoff used for
//! shell-outs like `$EDITOR`.

use std::io::{self, Write};

use crossterm::{
    cursor,
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{self, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};

/// RAII guard for the TUI terminal envelope. Restores cooked mode + the
/// normal screen on drop, even if the run loop panics.
pub struct TuiTerminal {
    // Terminal state is a process-wide resource; the guard must not migrate
    // between threads while it's alive.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl TuiTerminal {
    pub fn claim() -> io::Result<Self> {
        enter_envelope()?;
        Ok(Self {
            _not_send: std::marker::PhantomData,
        })
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
    pub fn suspended<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _ = release_input_modes();
        let result = f();
        // The child's exit may have dropped us to the normal screen; the
        // full envelope re-enters the alt buffer to be safe.
        let _ = enter_envelope();
        result
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        let _ = leave_envelope();
    }
}

// ── private ─────────────────────────────────────────────────────────────────

fn enter_envelope() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    out.queue(EnterAlternateScreen)?
        .queue(DisableLineWrap)?
        .queue(cursor::Hide)?
        .queue(EnableBracketedPaste)?
        .queue(EnableFocusChange)?
        .queue(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ))?
        .queue(EnableMouseCapture)?;
    out.flush()
}

fn leave_envelope() -> io::Result<()> {
    release_input_modes()?;
    let mut out = io::stdout();
    out.queue(LeaveAlternateScreen)?;
    out.flush()?;
    terminal::disable_raw_mode()
}

/// Drain the TUI's input-side modes (raw, mouse, bracketed, focus) plus the
/// display tweaks that aren't tied to the alt-screen swap (cursor visible,
/// line wrap on). Shared by full teardown and temporary suspend; the
/// alt-screen swap is the only difference and lives at the call site.
fn release_input_modes() -> io::Result<()> {
    let mut out = io::stdout();
    out.queue(DisableMouseCapture)?
        .queue(PopKeyboardEnhancementFlags)?
        .queue(DisableFocusChange)?
        .queue(DisableBracketedPaste)?
        .queue(cursor::Show)?
        .queue(EnableLineWrap)?;
    out.flush()
}
