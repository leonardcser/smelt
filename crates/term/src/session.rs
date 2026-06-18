use std::io::{self, BufWriter, Stdout, Write};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendScreen {
    KeepAlternate,
    LeaveAlternate,
}

pub struct TerminalSession<W: Write> {
    writer: W,
    raw_mode: bool,
    modes: TerminalModes,
    _not_send: std::marker::PhantomData<*const ()>,
}

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

    pub fn hide_cursor(mut self, yes: bool) -> Self {
        self.modes.hide_cursor = yes;
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

    pub fn enter_stdout(self) -> io::Result<TerminalSession<BufWriter<Stdout>>> {
        let capacity = self.buffer_capacity;
        let stdout = io::stdout();
        self.enter(BufWriter::with_capacity(capacity, stdout))
    }

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

    pub fn size(&self) -> io::Result<(u16, u16)> {
        terminal::size()
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
        }
        let result = f();
        if modes.alternate_screen {
            let _ = self.writer.queue(EnterAlternateScreen);
        }
        self.modes = modes;
        let _ = self.restore_input_modes(raw_mode);
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
        self.release_input_modes()?;
        if self.modes.alternate_screen {
            self.writer.queue(LeaveAlternateScreen)?;
            self.modes.alternate_screen = false;
        }
        self.writer.flush()?;
        if self.raw_mode {
            terminal::disable_raw_mode()?;
            self.raw_mode = false;
        }
        Ok(())
    }

    fn release_input_modes(&mut self) -> io::Result<()> {
        if self.modes.mouse_capture {
            self.writer.queue(DisableMouseCapture)?;
            self.modes.mouse_capture = false;
        }
        if self.modes.keyboard_enhancements.is_some() {
            self.writer.queue(PopKeyboardEnhancementFlags)?;
            self.modes.keyboard_enhancements = None;
        }
        if self.modes.focus_events {
            self.writer.queue(DisableFocusChange)?;
            self.modes.focus_events = false;
        }
        if self.modes.bracketed_paste {
            self.writer.queue(DisableBracketedPaste)?;
            self.modes.bracketed_paste = false;
        }
        if self.modes.hide_cursor {
            self.writer.queue(cursor::Show)?;
            self.modes.hide_cursor = false;
        }
        if !self.modes.line_wrap {
            self.writer.queue(EnableLineWrap)?;
            self.modes.line_wrap = true;
        }
        self.writer.flush()?;
        if self.raw_mode {
            terminal::disable_raw_mode()?;
            self.raw_mode = false;
        }
        Ok(())
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

    fn test_session(writer: TestWriter) -> TerminalSession<TestWriter> {
        TerminalSession {
            writer,
            raw_mode: false,
            modes: TerminalModes::inactive(),
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
