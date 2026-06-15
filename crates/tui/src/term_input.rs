//! Terminal input reader with explicit escape-sequence disambiguation.
//!
//! Crossterm's `EventStream` parser may emit a bare `Esc` when the OS hands it
//! only the first byte of a longer terminal sequence. Mouse reports are ESC-led
//! (`CSI < ... M`), so that split can leave the remaining bytes to be routed as
//! printable prompt input. This reader owns stdin byte parsing and waits briefly
//! before deciding that a lone ESC is a real key.

use std::io;
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

const ESC_TIMEOUT: Duration = Duration::from_millis(40);

pub(crate) struct TerminalInput {
    rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    shutdown: Option<platform::Shutdown>,
}

impl TerminalInput {
    pub(crate) fn spawn() -> io::Result<Self> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let shutdown = platform::spawn_reader(tx)?;
        Ok(Self {
            rx,
            shutdown: Some(shutdown),
        })
    }

    pub(crate) async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }

    pub(crate) fn try_recv(&mut self) -> Result<Event, tokio::sync::mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        drop(self.shutdown.take());
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::fd::RawFd;
    use std::thread::JoinHandle;

    pub(super) struct Shutdown {
        write_fd: RawFd,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Shutdown {
        fn drop(&mut self) {
            let byte = [0u8; 1];
            unsafe {
                let _ = libc::write(self.write_fd, byte.as_ptr().cast(), byte.len());
                libc::close(self.write_fd);
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub(super) fn spawn_reader(
        tx: tokio::sync::mpsc::UnboundedSender<Event>,
    ) -> io::Result<Shutdown> {
        let fd = open_input_fd()?;
        let (shutdown_read_fd, shutdown_write_fd) = match open_shutdown_pipe() {
            Ok(pipe) => pipe,
            Err(e) => {
                unsafe { libc::close(fd) };
                return Err(e);
            }
        };
        let thread = match std::thread::Builder::new()
            .name("smelt-terminal-input".into())
            .spawn(move || reader_loop(fd, shutdown_read_fd, tx))
        {
            Ok(thread) => thread,
            Err(e) => {
                unsafe {
                    libc::close(fd);
                    libc::close(shutdown_read_fd);
                    libc::close(shutdown_write_fd);
                }
                return Err(io::Error::other(e));
            }
        };
        Ok(Shutdown {
            write_fd: shutdown_write_fd,
            thread: Some(thread),
        })
    }

    fn open_input_fd() -> io::Result<RawFd> {
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) == 1 {
                let fd = libc::dup(libc::STDIN_FILENO);
                if fd < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(fd)
                }
            } else {
                let path = std::ffi::CString::new("/dev/tty").expect("literal has no nul");
                let fd = libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(fd)
                }
            }
        }
    }

    fn open_shutdown_pipe() -> io::Result<(RawFd, RawFd)> {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if let Err(e) = set_cloexec(fds[0]).and_then(|_| set_cloexec(fds[1])) {
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            Err(e)
        } else {
            Ok((fds[0], fds[1]))
        }
    }

    fn set_cloexec(fd: RawFd) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn reader_loop(fd: RawFd, shutdown_fd: RawFd, tx: tokio::sync::mpsc::UnboundedSender<Event>) {
        let mut parser = Parser::new();
        let mut buf = [0u8; 1024];
        loop {
            let timeout_ms = if parser.awaiting_escape_tail() {
                ESC_TIMEOUT.as_millis() as libc::c_int
            } else {
                -1
            };
            let mut pfds = [
                libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: shutdown_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready =
                unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if ready == 0 {
                for ev in parser.flush_escape_timeout() {
                    if tx.send(ev).is_err() {
                        close_reader_fds(fd, shutdown_fd);
                        return;
                    }
                }
                continue;
            }
            if pfds[1].revents & libc::POLLIN != 0 {
                break;
            }
            if pfds[0].revents & libc::POLLIN == 0 {
                continue;
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted
                    || err.kind() == io::ErrorKind::WouldBlock
                {
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }
            for ev in parser.advance(&buf[..n as usize]) {
                if tx.send(ev).is_err() {
                    close_reader_fds(fd, shutdown_fd);
                    return;
                }
            }
        }
        close_reader_fds(fd, shutdown_fd);
    }

    fn close_reader_fds(fd: RawFd, shutdown_fd: RawFd) {
        unsafe {
            libc::close(fd);
            libc::close(shutdown_fd);
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;
    use futures_core::Stream;
    use std::pin::Pin;

    pub(super) struct Shutdown;

    pub(super) fn spawn_reader(
        tx: tokio::sync::mpsc::UnboundedSender<Event>,
    ) -> io::Result<Shutdown> {
        std::thread::Builder::new()
            .name("smelt-terminal-input".into())
            .spawn(move || {
                let mut stream = crossterm::event::EventStream::new();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .expect("terminal input runtime");
                rt.block_on(async move {
                    while let Some(Ok(ev)) = stream_next(&mut stream).await {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                });
            })
            .map_err(io::Error::other)?;
        Ok(Shutdown)
    }

    async fn stream_next<S>(stream: &mut S) -> Option<S::Item>
    where
        S: Stream + Unpin,
    {
        std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
    }
}

#[derive(Debug, Default)]
struct Parser {
    buf: Vec<u8>,
    drop_csi_tail: bool,
    drop_string_tail: bool,
    drop_string_prev_esc: bool,
    drop_bytes: usize,
}

impl Parser {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(128),
            drop_csi_tail: false,
            drop_string_tail: false,
            drop_string_prev_esc: false,
            drop_bytes: 0,
        }
    }

    fn advance(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for &b in bytes {
            if self.drop_csi_tail {
                if csi_final_byte(b) {
                    self.drop_csi_tail = false;
                }
                continue;
            }
            if self.drop_string_tail {
                if b == 0x07 || (self.drop_string_prev_esc && b == b'\\') {
                    self.drop_string_tail = false;
                    self.drop_string_prev_esc = false;
                } else {
                    self.drop_string_prev_esc = b == 0x1b;
                }
                continue;
            }
            if self.drop_bytes > 0 {
                self.drop_bytes -= 1;
                continue;
            }
            self.buf.push(b);
            self.drain_ready(&mut out);
        }
        out
    }

    fn awaiting_escape_tail(&self) -> bool {
        self.buf.first() == Some(&0x1b) && !self.buf.starts_with(b"\x1b[200~")
    }

    fn flush_escape_timeout(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.buf == [0x1b] {
            self.buf.clear();
            out.push(key(KeyCode::Esc, KeyModifiers::empty()));
        } else if self.buf.starts_with(b"\x1b[") {
            self.buf.clear();
            self.drop_csi_tail = true;
        } else if self.buf.starts_with(b"\x1b]") {
            self.drop_string_prev_esc = self.buf.last() == Some(&0x1b);
            self.buf.clear();
            self.drop_string_tail = true;
        } else if self.buf.starts_with(b"\x1bO") {
            self.buf.clear();
            self.drop_bytes = 1;
        } else if self.buf.first() == Some(&0x1b) {
            self.buf.drain(..1);
            out.push(key(KeyCode::Esc, KeyModifiers::empty()));
            self.drain_ready(&mut out);
        }
        out
    }

    fn drain_ready(&mut self, out: &mut Vec<Event>) {
        loop {
            match parse_one(&self.buf) {
                ParseResult::Event { event, consumed } => {
                    self.buf.drain(..consumed);
                    out.push(event);
                }
                ParseResult::NeedMore => break,
                ParseResult::Invalid { consumed } => {
                    self.buf.drain(..consumed.clamp(1, self.buf.len()));
                }
            }
        }
    }
}

#[derive(Debug)]
enum ParseResult {
    Event { event: Event, consumed: usize },
    NeedMore,
    Invalid { consumed: usize },
}

fn parse_one(buf: &[u8]) -> ParseResult {
    if buf.is_empty() {
        return ParseResult::NeedMore;
    }
    match buf[0] {
        0x1b => parse_escape(buf),
        b'\r' => event(key(KeyCode::Enter, KeyModifiers::empty()), 1),
        b'\t' => event(key(KeyCode::Tab, KeyModifiers::empty()), 1),
        0x7f => event(key(KeyCode::Backspace, KeyModifiers::empty()), 1),
        c @ 0x01..=0x1a => event(
            key(
                KeyCode::Char((c - 0x01 + b'a') as char),
                KeyModifiers::CONTROL,
            ),
            1,
        ),
        c @ 0x1c..=0x1f => event(
            key(
                KeyCode::Char((c - 0x1c + b'4') as char),
                KeyModifiers::CONTROL,
            ),
            1,
        ),
        0x00 => event(key(KeyCode::Char(' '), KeyModifiers::CONTROL), 1),
        _ => parse_utf8_key(buf),
    }
}

fn parse_escape(buf: &[u8]) -> ParseResult {
    if buf.len() == 1 {
        return ParseResult::NeedMore;
    }
    match buf[1] {
        b'[' => parse_csi(buf),
        b']' => parse_string_control(buf),
        b'O' => parse_ss3(buf),
        0x1b => event(key(KeyCode::Esc, KeyModifiers::empty()), 1),
        _ => match parse_one(&buf[1..]) {
            ParseResult::Event {
                mut event,
                consumed,
            } => {
                if let Event::Key(k) = &mut event {
                    k.modifiers |= KeyModifiers::ALT;
                }
                ParseResult::Event {
                    event,
                    consumed: consumed + 1,
                }
            }
            ParseResult::NeedMore => ParseResult::NeedMore,
            ParseResult::Invalid { consumed } => invalid(consumed + 1),
        },
    }
}

fn parse_ss3(buf: &[u8]) -> ParseResult {
    if buf.len() < 3 {
        return ParseResult::NeedMore;
    }
    let code = match buf[2] {
        b'D' => KeyCode::Left,
        b'C' => KeyCode::Right,
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P'..=b'S' => KeyCode::F(1 + buf[2] - b'P'),
        _ => return invalid(3),
    };
    event(key(code, KeyModifiers::empty()), 3)
}

fn parse_csi(buf: &[u8]) -> ParseResult {
    if buf.len() < 3 {
        return ParseResult::NeedMore;
    }
    if buf.starts_with(b"\x1b[200~") {
        if let Some(end) = find_subslice(&buf[6..], b"\x1b[201~") {
            let start = 6;
            let end_abs = start + end;
            let paste = String::from_utf8_lossy(&buf[start..end_abs]).to_string();
            return event(Event::Paste(paste), end_abs + 6);
        }
        return ParseResult::NeedMore;
    }
    if buf[2] == b'<' {
        return parse_sgr_mouse(buf);
    }
    if buf[2] == b'M' {
        return parse_normal_mouse(buf);
    }
    if buf[2] == b'[' {
        return parse_linux_console_fkey(buf);
    }

    let Some(final_idx) = csi_final_index(buf) else {
        return ParseResult::NeedMore;
    };
    let final_byte = buf[final_idx];
    let params = &buf[2..final_idx];
    let consumed = final_idx + 1;

    match final_byte {
        b'A' | b'B' | b'C' | b'D' | b'H' | b'F' => {
            let code = match final_byte {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                _ => unreachable!(),
            };
            let mods = csi_trailing_modifier(params);
            event(key(code, mods), consumed)
        }
        b'Z' => event(key(KeyCode::BackTab, KeyModifiers::SHIFT), consumed),
        b'I' => event(Event::FocusGained, consumed),
        b'O' => event(Event::FocusLost, consumed),
        b'~' => parse_special_key(params, consumed),
        b'P' => event(key(KeyCode::F(1), csi_trailing_modifier(params)), consumed),
        b'Q' => event(key(KeyCode::F(2), csi_trailing_modifier(params)), consumed),
        b'R' => event(key(KeyCode::F(3), csi_trailing_modifier(params)), consumed),
        b'S' => event(key(KeyCode::F(4), csi_trailing_modifier(params)), consumed),
        b'M' => parse_rxvt_mouse(params, consumed),
        b'u' => parse_csi_u(params, consumed),
        _ => invalid(consumed),
    }
}

fn parse_linux_console_fkey(buf: &[u8]) -> ParseResult {
    if buf.len() < 4 {
        return ParseResult::NeedMore;
    }
    match buf[3] {
        b'A'..=b'E' => event(key(KeyCode::F(1 + buf[3] - b'A'), KeyModifiers::empty()), 4),
        _ => invalid(4),
    }
}

fn parse_string_control(buf: &[u8]) -> ParseResult {
    let mut prev_esc = false;
    for (i, &b) in buf.iter().enumerate().skip(2) {
        if b == 0x07 || (prev_esc && b == b'\\') {
            return invalid(i + 1);
        }
        prev_esc = b == 0x1b;
    }
    ParseResult::NeedMore
}

fn parse_sgr_mouse(buf: &[u8]) -> ParseResult {
    let Some(final_idx) = buf.iter().position(|&b| b == b'M' || b == b'm') else {
        return ParseResult::NeedMore;
    };
    let consumed = final_idx + 1;
    let s = match std::str::from_utf8(&buf[3..final_idx]) {
        Ok(s) => s,
        Err(_) => return invalid(consumed),
    };
    let mut parts = s.split(';');
    let Some(cb) = parts.next().and_then(|s| s.parse::<u8>().ok()) else {
        return invalid(consumed);
    };
    let Some(cx) = parts.next().and_then(|s| s.parse::<u16>().ok()) else {
        return invalid(consumed);
    };
    let Some(cy) = parts.next().and_then(|s| s.parse::<u16>().ok()) else {
        return invalid(consumed);
    };
    let Some((mut kind, modifiers)) = parse_mouse_cb(cb) else {
        return invalid(consumed);
    };
    if buf[final_idx] == b'm' {
        if let MouseEventKind::Down(button) = kind {
            kind = MouseEventKind::Up(button);
        }
    }
    event(
        Event::Mouse(MouseEvent {
            kind,
            column: cx.saturating_sub(1),
            row: cy.saturating_sub(1),
            modifiers,
        }),
        consumed,
    )
}

fn parse_normal_mouse(buf: &[u8]) -> ParseResult {
    if buf.len() < 6 {
        return ParseResult::NeedMore;
    }
    let Some(cb) = buf[3].checked_sub(32) else {
        return invalid(6);
    };
    let Some((kind, modifiers)) = parse_mouse_cb(cb) else {
        return invalid(6);
    };
    event(
        Event::Mouse(MouseEvent {
            kind,
            column: u16::from(buf[4].saturating_sub(32)).saturating_sub(1),
            row: u16::from(buf[5].saturating_sub(32)).saturating_sub(1),
            modifiers,
        }),
        6,
    )
}

fn parse_rxvt_mouse(params: &[u8], consumed: usize) -> ParseResult {
    let s = match std::str::from_utf8(params) {
        Ok(s) => s,
        Err(_) => return invalid(consumed),
    };
    let mut parts = s.split(';');
    let Some(raw_cb) = parts.next().and_then(|s| s.parse::<u8>().ok()) else {
        return invalid(consumed);
    };
    let Some(cb) = raw_cb.checked_sub(32) else {
        return invalid(consumed);
    };
    let Some((kind, modifiers)) = parse_mouse_cb(cb) else {
        return invalid(consumed);
    };
    let Some(cx) = parts.next().and_then(|s| s.parse::<u16>().ok()) else {
        return invalid(consumed);
    };
    let Some(cy) = parts.next().and_then(|s| s.parse::<u16>().ok()) else {
        return invalid(consumed);
    };
    event(
        Event::Mouse(MouseEvent {
            kind,
            column: cx.saturating_sub(1),
            row: cy.saturating_sub(1),
            modifiers,
        }),
        consumed,
    )
}

fn parse_special_key(params: &[u8], consumed: usize) -> ParseResult {
    let s = match std::str::from_utf8(params) {
        Ok(s) => s,
        Err(_) => return invalid(consumed),
    };
    let mut parts = s.split(';');
    let Some(first) = parts.next().and_then(|s| s.parse::<u8>().ok()) else {
        return invalid(consumed);
    };
    let mods = parts
        .next()
        .and_then(parse_modifier)
        .unwrap_or_else(KeyModifiers::empty);
    let code = match first {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        v @ 11..=15 => KeyCode::F(v - 10),
        v @ 17..=21 => KeyCode::F(v - 11),
        v @ 23..=26 => KeyCode::F(v - 12),
        v @ 28..=29 => KeyCode::F(v - 15),
        v @ 31..=34 => KeyCode::F(v - 17),
        _ => return invalid(consumed),
    };
    event(key(code, mods), consumed)
}

fn parse_csi_u(params: &[u8], consumed: usize) -> ParseResult {
    let s = match std::str::from_utf8(params) {
        Ok(s) => s,
        Err(_) => return invalid(consumed),
    };
    let mut parts = s.split(';');
    let Some(codepoint) = parts
        .next()
        .and_then(|s| s.split(':').next())
        .and_then(|s| s.parse::<u32>().ok())
        .and_then(char::from_u32)
    else {
        return invalid(consumed);
    };
    let mods = parts
        .next()
        .and_then(|s| s.split(':').next())
        .and_then(parse_modifier)
        .unwrap_or_else(KeyModifiers::empty);
    let code = match codepoint as u32 {
        9 => KeyCode::Tab,
        10 | 13 => KeyCode::Enter,
        27 => KeyCode::Esc,
        127 => KeyCode::Backspace,
        _ => KeyCode::Char(codepoint),
    };
    event(key(code, mods), consumed)
}

fn parse_mouse_cb(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 == 0b0010_0000;
    let kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3, true) | (4, true) | (5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let mut modifiers = KeyModifiers::empty();
    if cb & 0b0000_0100 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if cb & 0b0000_1000 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if cb & 0b0001_0000 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    Some((kind, modifiers))
}

fn parse_utf8_key(buf: &[u8]) -> ParseResult {
    let width = utf8_width(buf[0]);
    if width == 0 {
        return invalid(1);
    }
    if buf.len() < width {
        return ParseResult::NeedMore;
    }
    let s = match std::str::from_utf8(&buf[..width]) {
        Ok(s) => s,
        Err(_) => return invalid(1),
    };
    let Some(ch) = s.chars().next() else {
        return invalid(1);
    };
    let mods = if ch.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::empty()
    };
    event(key(KeyCode::Char(ch), mods), width)
}

fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

fn csi_final_index(buf: &[u8]) -> Option<usize> {
    buf.iter()
        .enumerate()
        .skip(2)
        .find_map(|(i, &b)| csi_final_byte(b).then_some(i))
}

fn csi_final_byte(b: u8) -> bool {
    (0x40..=0x7e).contains(&b)
}

fn csi_trailing_modifier(params: &[u8]) -> KeyModifiers {
    std::str::from_utf8(params)
        .ok()
        .and_then(|s| s.rsplit(';').next())
        .and_then(parse_modifier)
        .unwrap_or_else(KeyModifiers::empty)
}

fn parse_modifier(s: &str) -> Option<KeyModifiers> {
    let n = s.parse::<u8>().ok()?;
    let mut out = KeyModifiers::empty();
    let bits = n.checked_sub(1)?;
    if bits & 1 != 0 {
        out |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        out |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        out |= KeyModifiers::CONTROL;
    }
    Some(out)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn event(event: Event, consumed: usize) -> ParseResult {
    ParseResult::Event { event, consumed }
}

fn invalid(consumed: usize) -> ParseResult {
    ParseResult::Invalid { consumed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_text(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|ev| match ev {
                Event::Key(k) => match k.code {
                    KeyCode::Char(c) => Some(c),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn split_sgr_mouse_waits_for_tail() {
        let mut p = Parser::new();
        assert!(p.advance(b"\x1b").is_empty());
        let events = p.advance(b"[<32;3;40M");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], Event::Mouse(_)));
        assert_eq!(keys_text(&events), "");
    }

    #[test]
    fn incomplete_csi_timeout_does_not_emit_tail_text() {
        let mut p = Parser::new();
        assert!(p.advance(b"\x1b[").is_empty());
        let events = p.flush_escape_timeout();
        assert!(events.is_empty());
        assert!(p.advance(b"<32;3;40M").is_empty());
    }

    #[test]
    fn unknown_csi_is_swallowed() {
        let mut p = Parser::new();
        let events = p.advance(b"\x1b[?25h");
        assert!(events.is_empty());
        assert_eq!(keys_text(&events), "");
    }

    #[test]
    fn osc_sequence_is_swallowed() {
        let mut p = Parser::new();
        assert!(p.advance(b"\x1b]0;title\x07").is_empty());
    }

    #[test]
    fn split_osc_timeout_does_not_emit_tail_text() {
        let mut p = Parser::new();
        assert!(p.advance(b"\x1b]").is_empty());
        assert!(p.flush_escape_timeout().is_empty());
        assert!(p.advance(b"0;title\x07").is_empty());
    }

    #[test]
    fn linux_console_fkey_sequence_is_parsed() {
        let mut p = Parser::new();
        assert!(matches!(
            p.advance(b"\x1b[[A").as_slice(),
            [Event::Key(KeyEvent {
                code: KeyCode::F(1),
                ..
            })]
        ));
    }

    #[test]
    fn csi_u_shift_enter_is_parsed_as_enter() {
        let mut p = Parser::new();
        assert!(matches!(
            p.advance(b"\x1b[13;2u").as_slice(),
            [Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            })] if modifiers.contains(KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn csi_u_ctrl_enter_is_parsed_as_enter() {
        let mut p = Parser::new();
        assert!(matches!(
            p.advance(b"\x1b[13;5u").as_slice(),
            [Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            })] if modifiers.contains(KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn escape_timeout_emits_real_escape() {
        let mut p = Parser::new();
        assert!(p.advance(b"\x1b").is_empty());
        let events = p.flush_escape_timeout();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ));
    }

    #[test]
    fn split_bracketed_paste_waits_for_end() {
        let mut p = Parser::new();
        assert!(p.advance(b"\x1b[200~hello").is_empty());
        let events = p.advance(b" world\x1b[201~");
        assert_eq!(events, vec![Event::Paste("hello world".into())]);
    }

    #[test]
    fn alt_char_uses_escape_prefix() {
        let mut p = Parser::new();
        assert!(p.advance(b"\x1bd").into_iter().any(|ev| matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('d'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::ALT)
        )));
    }
}
