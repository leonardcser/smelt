use std::io::{self, BufWriter, Stdout, Write};
use std::sync::{Mutex, Once};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{self, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};

use crate::grid::GridSlice;
use crate::layout::{PaintId, ResolvedLayout};
use crate::surface::Surface;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendScreen {
    KeepAlternate,
    LeaveAlternate,
}

pub struct TerminalSession<W: Write> {
    writer: W,
    raw_mode: bool,
    modes: TerminalModes,
    /// Stdout sessions publish their state so a panic can restore the terminal.
    panic_restore: bool,
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Modes and raw mode the active stdout session has turned on. The panic hook
/// takes this to restore the terminal before the panic message prints.
static PANIC_RESTORE: Mutex<Option<(TerminalModes, bool)>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalModes {
    alternate_screen: bool,
    mouse_capture: bool,
    hide_cursor: bool,
    keyboard_enhancements: Option<KeyboardEnhancementFlags>,
    bracketed_paste: bool,
    focus_events: bool,
    line_wrap: bool,
}

impl TerminalModes {
    fn inactive() -> Self {
        Self {
            alternate_screen: false,
            mouse_capture: false,
            hide_cursor: false,
            keyboard_enhancements: None,
            bracketed_paste: false,
            focus_events: false,
            line_wrap: true,
        }
    }
}

impl Default for TerminalModes {
    fn default() -> Self {
        Self {
            alternate_screen: true,
            mouse_capture: true,
            hide_cursor: true,
            keyboard_enhancements: None,
            bracketed_paste: false,
            focus_events: false,
            line_wrap: true,
        }
    }
}

/// Configures the terminal modes a [`TerminalSession`] enters. The hardware
/// caret is always hidden while the session is active: paint any visible
/// cursor into the grid, and use `Grid::set_terminal_cursor_position` to
/// anchor the hidden caret for input methods.
#[derive(Clone, Debug)]
pub struct TerminalSessionBuilder {
    modes: TerminalModes,
    buffer_capacity: usize,
}

impl Default for TerminalSessionBuilder {
    fn default() -> Self {
        Self {
            modes: TerminalModes::default(),
            buffer_capacity: 64 * 1024,
        }
    }
}

impl TerminalSessionBuilder {
    pub fn alternate_screen(mut self, yes: bool) -> Self {
        self.modes.alternate_screen = yes;
        self
    }

    pub fn mouse_capture(mut self, yes: bool) -> Self {
        self.modes.mouse_capture = yes;
        self
    }

    pub fn keyboard_enhancements(mut self, flags: KeyboardEnhancementFlags) -> Self {
        self.modes.keyboard_enhancements = Some(flags);
        self
    }

    pub fn bracketed_paste(mut self, yes: bool) -> Self {
        self.modes.bracketed_paste = yes;
        self
    }

    pub fn focus_events(mut self, yes: bool) -> Self {
        self.modes.focus_events = yes;
        self
    }

    pub fn line_wrap(mut self, yes: bool) -> Self {
        self.modes.line_wrap = yes;
        self
    }

    pub fn buffer_capacity(mut self, bytes: usize) -> Self {
        self.buffer_capacity = bytes;
        self
    }

    /// Enter on stdout. A panic while the session is active restores the
    /// terminal first, so the panic message is readable.
    pub fn enter_stdout(self) -> io::Result<TerminalSession<BufWriter<Stdout>>> {
        let capacity = self.buffer_capacity;
        let mut session = self.enter(BufWriter::with_capacity(capacity, io::stdout()))?;
        install_panic_hook();
        session.panic_restore = true;
        session.publish_state();
        Ok(session)
    }

    /// Enter on a custom writer. No panic hook is installed, since the hook
    /// cannot reach `writer`.
    pub fn enter<W: Write>(self, writer: W) -> io::Result<TerminalSession<W>> {
        TerminalSession::enter_with(writer, self)
    }
}

impl TerminalSession<BufWriter<Stdout>> {
    pub fn enter_stdout() -> io::Result<Self> {
        TerminalSession::builder().enter_stdout()
    }

    pub fn builder() -> TerminalSessionBuilder {
        TerminalSessionBuilder::default()
    }
}

impl<W: Write> TerminalSession<W> {
    fn enter_with(writer: W, options: TerminalSessionBuilder) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut session = Self {
            writer,
            raw_mode: true,
            modes: TerminalModes::inactive(),
            panic_restore: false,
            _not_send: std::marker::PhantomData,
        };

        if let Err(err) = session.enter_modes(options.modes) {
            let _ = session.restore();
            return Err(err);
        }

        Ok(session)
    }

    pub fn writer(&mut self) -> &mut W {
        &mut self.writer
    }

    pub fn write_control_sequence(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        terminal::size()
    }

    /// Match `surface` to the terminal's current size, then render it.
    /// Returns the painted geometry for hit-testing this frame.
    pub fn draw<F>(&mut self, surface: &mut Surface, paint: F) -> io::Result<ResolvedLayout>
    where
        F: FnMut(PaintId, &mut GridSlice<'_>),
    {
        let (width, height) = self.size()?;
        if surface.terminal_size() != (width, height) {
            surface.set_terminal_size(width, height);
        }
        surface.render(&mut self.writer, paint)
    }

    /// Wait for input, then return it together with everything already queued.
    /// `None` waits forever; an expired `timeout` returns no events. Only the
    /// last `Resize` is kept: a resize drag queues one per step, and
    /// [`Self::draw`] reads the final size anyway.
    pub fn events(&self, timeout: Option<Duration>) -> io::Result<Vec<Event>> {
        if let Some(timeout) = timeout {
            if !event::poll(timeout)? {
                return Ok(Vec::new());
            }
        }
        let mut events = vec![event::read()?];
        while event::poll(Duration::ZERO)? {
            events.push(event::read()?);
        }
        Ok(keep_last_resize(events))
    }

    pub fn suspend<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.suspend_with(SuspendScreen::KeepAlternate, f)
    }

    pub fn suspend_with<F, R>(&mut self, screen: SuspendScreen, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let modes = self.modes;
        let raw_mode = self.raw_mode;
        let _ = self.release_input_modes();
        if screen == SuspendScreen::LeaveAlternate && modes.alternate_screen {
            let _ = self.writer.queue(LeaveAlternateScreen);
            let _ = self.writer.flush();
            self.modes.alternate_screen = false;
        }
        self.publish_state();
        let result = f();
        if modes.alternate_screen {
            let _ = self.writer.queue(EnterAlternateScreen);
        }
        self.modes = modes;
        let _ = self.restore_input_modes(raw_mode);
        self.publish_state();
        result
    }

    fn enter_modes(&mut self, modes: TerminalModes) -> io::Result<()> {
        if modes.alternate_screen {
            self.writer.queue(EnterAlternateScreen)?;
            self.modes.alternate_screen = true;
        }
        if !modes.line_wrap {
            self.writer.queue(DisableLineWrap)?;
            self.modes.line_wrap = false;
        }
        if modes.hide_cursor {
            self.writer.queue(cursor::Hide)?;
            self.modes.hide_cursor = true;
        }
        if modes.bracketed_paste {
            self.writer.queue(EnableBracketedPaste)?;
            self.modes.bracketed_paste = true;
        }
        if modes.focus_events {
            self.writer.queue(EnableFocusChange)?;
            self.modes.focus_events = true;
        }
        if let Some(flags) = modes.keyboard_enhancements {
            self.writer.queue(PushKeyboardEnhancementFlags(flags))?;
            self.modes.keyboard_enhancements = Some(flags);
        }
        if modes.mouse_capture {
            self.writer.queue(EnableMouseCapture)?;
            self.modes.mouse_capture = true;
        }
        self.writer.flush()
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.panic_restore && take_panic_state().is_none() {
            // The panic hook already restored the terminal.
            self.modes = TerminalModes::inactive();
            self.raw_mode = false;
            return Ok(());
        }
        restore_terminal(&mut self.writer, &mut self.modes, &mut self.raw_mode)
    }

    fn release_input_modes(&mut self) -> io::Result<()> {
        queue_input_release(&mut self.writer, &mut self.modes)?;
        self.writer.flush()?;
        disable_raw_mode(&mut self.raw_mode)
    }

    /// Record the current state for the panic hook. Stdout sessions only.
    fn publish_state(&self) {
        if self.panic_restore {
            *panic_state() = Some((self.modes, self.raw_mode));
        }
    }

    fn restore_input_modes(&mut self, raw_mode: bool) -> io::Result<()> {
        if raw_mode {
            terminal::enable_raw_mode()?;
            self.raw_mode = true;
        }
        if !self.modes.line_wrap {
            self.writer.queue(DisableLineWrap)?;
        }
        if self.modes.hide_cursor {
            self.writer.queue(cursor::Hide)?;
        }
        if self.modes.bracketed_paste {
            self.writer.queue(EnableBracketedPaste)?;
        }
        if self.modes.focus_events {
            self.writer.queue(EnableFocusChange)?;
        }
        if let Some(flags) = self.modes.keyboard_enhancements {
            self.writer.queue(PushKeyboardEnhancementFlags(flags))?;
        }
        if self.modes.mouse_capture {
            self.writer.queue(EnableMouseCapture)?;
        }
        self.writer.flush()
    }
}

/// Undo every active mode, then leave the alternate screen and raw mode.
/// Each flag clears once its sequence is queued, so a failed write can retry.
fn restore_terminal(
    w: &mut impl Write,
    modes: &mut TerminalModes,
    raw_mode: &mut bool,
) -> io::Result<()> {
    queue_input_release(w, modes)?;
    if modes.alternate_screen {
        w.queue(LeaveAlternateScreen)?;
        modes.alternate_screen = false;
    }
    w.flush()?;
    disable_raw_mode(raw_mode)
}

/// Queue the sequences that undo every active mode except the alternate screen.
fn queue_input_release(w: &mut impl Write, modes: &mut TerminalModes) -> io::Result<()> {
    if modes.mouse_capture {
        w.queue(DisableMouseCapture)?;
        modes.mouse_capture = false;
    }
    if modes.keyboard_enhancements.is_some() {
        w.queue(PopKeyboardEnhancementFlags)?;
        modes.keyboard_enhancements = None;
    }
    if modes.focus_events {
        w.queue(DisableFocusChange)?;
        modes.focus_events = false;
    }
    if modes.bracketed_paste {
        w.queue(DisableBracketedPaste)?;
        modes.bracketed_paste = false;
    }
    if modes.hide_cursor {
        w.queue(cursor::Show)?;
        modes.hide_cursor = false;
    }
    if !modes.line_wrap {
        w.queue(EnableLineWrap)?;
        modes.line_wrap = true;
    }
    Ok(())
}

fn disable_raw_mode(raw_mode: &mut bool) -> io::Result<()> {
    if *raw_mode {
        terminal::disable_raw_mode()?;
        *raw_mode = false;
    }
    Ok(())
}

fn panic_state() -> std::sync::MutexGuard<'static, Option<(TerminalModes, bool)>> {
    PANIC_RESTORE.lock().unwrap_or_else(|e| e.into_inner())
}

fn take_panic_state() -> Option<(TerminalModes, bool)> {
    panic_state().take()
}

/// Chain a hook that restores the terminal before the previous hook prints.
fn install_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some((mut modes, mut raw_mode)) = take_panic_state() {
                let _ = restore_terminal(&mut io::stdout(), &mut modes, &mut raw_mode);
            }
            previous(info);
        }));
    });
}

/// Drop every `Resize` but the last, keeping all other events in order.
fn keep_last_resize(mut events: Vec<Event>) -> Vec<Event> {
    let is_resize = |event: &Event| matches!(event, Event::Resize(..));
    if let Some(last) = events.iter().rposition(is_resize) {
        let mut index = 0;
        events.retain(|event| {
            let keep = index == last || !is_resize(event);
            index += 1;
            keep
        });
    }
    events
}

impl<W: Write> Drop for TerminalSession<W> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct TestWriter {
        bytes: Vec<u8>,
        fail_flush: bool,
    }

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn keep_last_resize_drops_intermediate_sizes_only() {
        let key = Event::FocusGained;
        let events = vec![
            Event::Resize(80, 24),
            key.clone(),
            Event::Resize(90, 24),
            Event::Resize(100, 30),
            Event::FocusLost,
        ];
        assert_eq!(
            keep_last_resize(events),
            vec![key, Event::Resize(100, 30), Event::FocusLost]
        );
        assert_eq!(keep_last_resize(Vec::new()), Vec::new());
    }

    fn test_session(writer: TestWriter) -> TerminalSession<TestWriter> {
        TerminalSession {
            writer,
            raw_mode: false,
            modes: TerminalModes::inactive(),
            panic_restore: false,
            _not_send: std::marker::PhantomData,
        }
    }

    fn all_modes() -> TerminalModes {
        TerminalModes {
            alternate_screen: true,
            mouse_capture: true,
            hide_cursor: true,
            keyboard_enhancements: Some(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            bracketed_paste: true,
            focus_events: true,
            line_wrap: false,
        }
    }

    #[test]
    fn terminal_session_tracks_and_restores_modes() {
        let mut session = test_session(TestWriter::default());
        session.enter_modes(all_modes()).unwrap();
        assert_eq!(session.modes, all_modes());
        assert!(!session.writer.bytes.is_empty());

        session.restore().unwrap();
        assert_eq!(session.modes, TerminalModes::inactive());
    }

    #[test]
    fn terminal_session_can_restore_after_enter_flush_failure() {
        let mut session = test_session(TestWriter {
            fail_flush: true,
            ..TestWriter::default()
        });

        let err = session.enter_modes(all_modes()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(session.modes, all_modes());

        session.writer.fail_flush = false;
        session.restore().unwrap();
        assert_eq!(session.modes, TerminalModes::inactive());
    }

    #[test]
    fn restore_is_skipped_after_the_panic_hook_restored_the_terminal() {
        let mut session = test_session(TestWriter::default());
        session.modes = all_modes();
        session.panic_restore = true;
        // The hook took the published state, so nothing is left to undo.
        take_panic_state();

        session.restore().unwrap();

        assert_eq!(session.modes, TerminalModes::inactive());
        assert!(session.writer.bytes.is_empty());
    }

    #[test]
    fn terminal_session_suspend_can_leave_alternate_screen() {
        let mut session = test_session(TestWriter::default());
        session.modes = all_modes();

        let result = session.suspend_with(SuspendScreen::LeaveAlternate, || 42);

        assert_eq!(result, 42);
        assert_eq!(session.modes, all_modes());
        let out = String::from_utf8_lossy(&session.writer.bytes);
        assert!(out.contains("1049l"), "missing leave alt screen: {out:?}");
        assert!(out.contains("1049h"), "missing enter alt screen: {out:?}");
    }

    #[test]
    fn terminal_session_suspend_keeps_alternate_screen_by_default() {
        let mut session = test_session(TestWriter::default());
        session.modes = all_modes();

        session.suspend(|| ());

        let out = String::from_utf8_lossy(&session.writer.bytes);
        assert!(
            !out.contains("1049l"),
            "unexpected leave alt screen: {out:?}"
        );
        assert!(out.contains("1049h"), "missing enter alt screen: {out:?}");
    }
}
