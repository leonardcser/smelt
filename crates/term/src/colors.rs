//! Ask the terminal for its colors: the default foreground (OSC 10), the
//! default background (OSC 11) and the 16 ANSI colors (OSC 4).
//!
//! Run [`query`] before [`crate::TerminalSession`] takes the terminal. Keys
//! typed while it waits come back in [`ColorQuery::input`]. Programs that
//! parse input bytes themselves can replay them; programs that read crossterm
//! events cannot, so those keys are lost.

use smelt_style::palette::{Palette, Rgb};

/// The colors a terminal reported. Each is `None` when it did not answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalColors {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub ansi: [Option<Rgb>; 16],
    /// Whether the background is light. `None` when it is not known.
    pub light: Option<bool>,
}

/// The result of [`query`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ColorQuery {
    pub colors: TerminalColors,
    /// Bytes typed while the query waited, without the terminal's replies.
    pub input: Vec<u8>,
}

impl TerminalColors {
    /// The reported colors, with the xterm palette for a light or dark
    /// background (dark when unknown) in the gaps.
    pub fn palette(&self) -> Palette {
        let mut palette = Palette::xterm(self.light.unwrap_or(false));
        palette.fg = self.fg.unwrap_or(palette.fg);
        palette.bg = self.bg.unwrap_or(palette.bg);
        for (slot, reply) in palette.ansi.iter_mut().zip(self.ansi) {
            *slot = reply.unwrap_or(*slot);
        }
        palette
    }

    /// Read the colors from OSC reply payloads such as `4;1;rgb:cd/00/00`.
    /// `light` follows the reported background.
    fn from_replies<'a>(replies: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut colors = Self::default();
        for reply in replies {
            let Ok(reply) = std::str::from_utf8(reply) else {
                continue;
            };
            let mut fields = reply.split(';');
            match (fields.next(), fields.next(), fields.next()) {
                (Some("10"), Some(color), None) => colors.fg = parse_x11_rgb(color),
                (Some("11"), Some(color), None) => colors.bg = parse_x11_rgb(color),
                (Some("4"), Some(index), Some(color)) => {
                    if let Some(slot) = index
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| colors.ansi.get_mut(index))
                    {
                        *slot = parse_x11_rgb(color);
                    }
                }
                _ => {}
            }
        }
        colors.light = colors.bg.map(|bg| {
            Palette {
                bg,
                ..Palette::xterm(false)
            }
            .is_light()
        });
        colors
    }
}

/// Send the color queries, fenced by a status report so terminals that ignore
/// some of them still end the wait, and read the replies from the
/// controlling terminal. Waits at most 100 ms for each read and 200 ms in
/// total. Turns raw mode on for the wait when it is off. Returns no colors on
/// a `dumb` terminal, without a controlling terminal, or off Unix. When the
/// terminal does not report its background, `light` comes from `$COLORFGBG`.
pub fn query() -> ColorQuery {
    let response = exchange();
    let scan = Scan::new(&response);
    let mut colors = TerminalColors::from_replies(scan.replies);
    if colors.light.is_none() {
        colors.light = std::env::var("COLORFGBG")
            .ok()
            .and_then(|value| colorfgbg_is_light(&value));
    }
    ColorQuery {
        colors,
        input: scan.input,
    }
}

/// Every byte the terminal sent back for the queries.
#[cfg(unix)]
fn exchange() -> Vec<u8> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
    use std::fs::OpenOptions;
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    /// Longest wait for the next reply byte.
    const IDLE_TIMEOUT: Duration = Duration::from_millis(100);
    /// Longest total wait, so continuous typing cannot extend it.
    const TOTAL_TIMEOUT: Duration = Duration::from_millis(200);

    if std::env::var("TERM").is_ok_and(|term| term == "dumb") {
        return Vec::new();
    }
    let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
        return Vec::new();
    };
    let switch_raw = !is_raw_mode_enabled().unwrap_or(false);
    if switch_raw && enable_raw_mode().is_err() {
        return Vec::new();
    }

    let mut response = Vec::new();
    if tty.write_all(&request()).and_then(|()| tty.flush()).is_ok() {
        let deadline = Instant::now() + TOTAL_TIMEOUT;
        let mut chunk = [0u8; 1024];
        while !Scan::new(&response).fenced {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(IDLE_TIMEOUT);
            if wait.is_zero() || !wait_for_input(tty.as_raw_fd(), wait) {
                break;
            }
            match tty.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&chunk[..n]),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }

    if switch_raw {
        let _ = disable_raw_mode();
    }
    response
}

#[cfg(not(unix))]
fn exchange() -> Vec<u8> {
    Vec::new()
}

/// OSC 10, 11 and 4 queries for every ANSI color, then a device status
/// report request. Terminals answer in order, so the status report comes last.
#[cfg(unix)]
fn request() -> Vec<u8> {
    let mut request = b"\x1b]10;?\x07\x1b]11;?\x07".to_vec();
    for index in 0..16 {
        request.extend_from_slice(format!("\x1b]4;{index};?\x07").as_bytes());
    }
    request.extend_from_slice(b"\x1b[5n");
    request
}

/// Terminal output split into OSC reply payloads, the status report fence,
/// and everything else, which is typed input.
#[derive(Debug, Default, PartialEq)]
struct Scan<'a> {
    replies: Vec<&'a [u8]>,
    fenced: bool,
    input: Vec<u8>,
}

impl<'a> Scan<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        let mut scan = Self::default();
        let mut rest = bytes;
        while let Some(&byte) = rest.first() {
            if let Some(body) = rest.strip_prefix(b"\x1b]") {
                // An unterminated reply was cut off by the timeout: drop it.
                let Some((payload, len)) = osc_payload(body) else {
                    break;
                };
                scan.replies.push(payload);
                rest = &body[len..];
            } else if let Some(len) = status_report_len(rest) {
                scan.fenced = true;
                rest = &rest[len..];
            } else {
                scan.input.push(byte);
                rest = &rest[1..];
            }
        }
        scan
    }
}

/// The payload of an OSC body terminated by BEL or ST, and the body length
/// including the terminator.
fn osc_payload(body: &[u8]) -> Option<(&[u8], usize)> {
    body.iter().enumerate().find_map(|(end, &byte)| match byte {
        b'\x07' => Some((&body[..end], end + 1)),
        b'\x1b' if body.get(end + 1) == Some(&b'\\') => Some((&body[..end], end + 2)),
        _ => None,
    })
}

/// The length of a device status report (`ESC [ <digits> n`) at the start of
/// `bytes`. No key sends this sequence.
fn status_report_len(bytes: &[u8]) -> Option<usize> {
    let rest = bytes.strip_prefix(b"\x1b[")?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    (digits > 0 && rest.get(digits) == Some(&b'n')).then_some(2 + digits + 1)
}

/// Parse an X11 `rgb:R/G/B` color with 1 to 4 hex digits per channel.
fn parse_x11_rgb(text: &str) -> Option<Rgb> {
    let mut channels = text.strip_prefix("rgb:")?.split('/').map(|channel| {
        let digits = channel.len();
        if !(1..=4).contains(&digits) {
            return None;
        }
        let value = u32::from_str_radix(channel, 16).ok()?;
        let max = (1u32 << (4 * digits)) - 1;
        Some(((value * 255 + max / 2) / max) as u8)
    });
    let rgb = (channels.next()??, channels.next()??, channels.next()??);
    channels.next().is_none().then_some(rgb)
}

/// Parse `$COLORFGBG` (`"fg;bg"` or `"fg;default;bg"`).
fn colorfgbg_is_light(value: &str) -> Option<bool> {
    let parts: Vec<&str> = value.split(';').collect();
    let bg = match parts.as_slice() {
        [_, bg] | [_, _, bg] => bg,
        _ => return None,
    };
    let code: u8 = bg.parse().ok()?;
    Some(matches!(code, 7 | 9..=15))
}

/// Whether `fd` is readable within `timeout`.
#[cfg(target_os = "macos")]
fn wait_for_input(fd: std::os::fd::RawFd, timeout: std::time::Duration) -> bool {
    // poll() does not work on tty devices on macOS.
    unsafe {
        let mut read_fds: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut read_fds);
        let mut tv = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        libc::select(
            fd + 1,
            &mut read_fds,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        ) > 0
    }
}

/// Whether `fd` is readable within `timeout`.
#[cfg(all(unix, not(target_os = "macos")))]
fn wait_for_input(fd: std::os::fd::RawFd, timeout: std::time::Duration) -> bool {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pollfd, 1, timeout.as_millis() as libc::c_int) > 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_style::palette::XTERM_ANSI;

    fn colors(bytes: &[u8]) -> TerminalColors {
        TerminalColors::from_replies(Scan::new(bytes).replies)
    }

    #[test]
    fn reads_every_answer() {
        let colors = colors(
            b"\x1b]10;rgb:e5e5/e5e5/e5e5\x1b\\\x1b]11;rgb:1c/1c/1c\x07\x1b]4;1;rgb:cdcd/0000/0000\x1b\\\x1b]4;15;rgb:f/f/f\x07\x1b[0n",
        );
        assert_eq!(colors.fg, Some((229, 229, 229)));
        assert_eq!(colors.bg, Some((28, 28, 28)));
        assert_eq!(colors.ansi[1], Some((205, 0, 0)));
        assert_eq!(colors.ansi[15], Some((255, 255, 255)));
        assert_eq!(colors.ansi[2], None);
        assert_eq!(colors.light, Some(false));

        let palette = colors.palette();
        assert_eq!(palette.bg, (28, 28, 28));
        assert_eq!(palette.ansi[2], XTERM_ANSI[2]);
    }

    #[test]
    fn light_background_fills_gaps_from_light_xterm_palette() {
        let colors = colors(b"\x1b]11;rgb:ffff/ffff/f0f0\x07");
        assert_eq!(colors.light, Some(true));
        assert_eq!(colors.palette().fg, Palette::xterm(true).fg);
    }

    #[test]
    fn ignores_malformed_replies() {
        assert_eq!(colors(b"garbage"), TerminalColors::default());
        assert_eq!(
            colors(b"\x1b]11;rgb:12345/0/0\x07\x1b]4;99;rgb:0/0/0\x07\x1b]10;rgb:0/0\x07"),
            TerminalColors::default()
        );
    }

    #[test]
    fn typed_keys_around_replies_are_input() {
        let scan = Scan::new(b"a\x1b[A\x1b]11;rgb:0/0/0\x07b\x1b[0nc\x1b[3~");
        assert_eq!(scan.replies, [&b"11;rgb:0/0/0"[..]]);
        assert!(scan.fenced);
        assert_eq!(scan.input, b"a\x1b[Abc\x1b[3~");
    }

    #[test]
    fn status_report_needs_digits() {
        assert!(Scan::new(b"\x1b[3n").fenced);
        assert!(!Scan::new(b"\x1b[n").fenced);
        assert!(!Scan::new(b"[0n").fenced);
        assert!(!Scan::new(b"\x1b]11;rgb:0/0/0\x07").fenced);
    }

    #[test]
    fn unterminated_reply_is_dropped() {
        let scan = Scan::new(b"a\x1b]4;3;rgb:cd/cd");
        assert!(scan.replies.is_empty());
        assert_eq!(scan.input, b"a");
    }

    #[test]
    fn colorfgbg_names_the_background_slot() {
        assert_eq!(colorfgbg_is_light("0;15"), Some(true));
        assert_eq!(colorfgbg_is_light("15;default;0"), Some(false));
        assert_eq!(colorfgbg_is_light("15"), None);
        assert_eq!(colorfgbg_is_light("0;x"), None);
    }
}
