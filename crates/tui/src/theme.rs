//! Theme initialisation: `populate_ui_theme`, light/dark detection, and preset list.

use smelt_core::style::Color;

/// Re-export so callers can refer to one canonical name.
pub use crate::smelt_term::DEFAULT_ACCENT;

/// Look up a preset by name. Returns the ansi value if found.
pub(crate) fn preset_by_name(name: &str) -> Option<u8> {
    PRESETS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, v)| *v)
}

// ---------------------------------------------------------------------------
// Light / dark terminal detection
// ---------------------------------------------------------------------------

/// Probe terminal background and set the light flag on `theme`.
/// Must be called before entering raw mode / alternate screen.
pub(crate) fn detect_background(theme: &mut crate::smelt_term::Theme) {
    if let Some(light) = detect_light_background() {
        theme.set_light(light);
    }
}

fn detect_light_background() -> Option<bool> {
    if let Some(luma) = osc_background_luma() {
        return Some(luma > 0.6);
    }
    colorfgbg_is_light()
}

/// Parse `$COLORFGBG` (`"fg;bg"` or `"fg;default;bg"`).
fn colorfgbg_is_light() -> Option<bool> {
    let val = std::env::var("COLORFGBG").ok()?;
    let parts: Vec<&str> = val.split(';').collect();
    let bg = match parts.len() {
        2 => parts[1],
        3 => parts[2],
        _ => return None,
    };
    let code: u8 = bg.parse().ok()?;
    Some(matches!(code, 7 | 9..=15)) // ANSI: 0-6,8 dark; 7,9-15 light
}

/// OSC 11 query: returns luma of the terminal background (0.0 = black, 1.0 = white).
#[cfg(unix)]
fn osc_background_luma() -> Option<f32> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    // Don't query if TERM=dumb.
    if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        return None;
    }

    let switch_raw = !is_raw_mode_enabled().unwrap_or(false);
    if switch_raw {
        enable_raw_mode().ok()?;
    }

    let result = (|| -> Option<f32> {
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "\x1b]11;?\x07\x1b[5n").ok()?; // OSC 11 + DSR fence
        stdout.flush().ok()?;

        let mut tty = File::open("/dev/tty").ok()?;
        let mut buf = [0u8; 100];
        let mut written = 0;

        while written < buf.len() {
            if !wait_for_input(tty.as_raw_fd(), 100) {
                break;
            }
            let n = tty.read(&mut buf[written..]).ok()?;
            if n == 0 {
                break;
            }
            written += n;
            if buf[..written].contains(&b'n') {
                break;
            }
        }

        let response = std::str::from_utf8(&buf[..written]).ok()?;
        parse_osc11_response(response)
    })();

    if switch_raw {
        let _ = disable_raw_mode();
    }

    result
}

#[cfg(not(unix))]
fn osc_background_luma() -> Option<f32> {
    None
}

/// Parse `\x1b]11;rgb:RRRR/GGGG/BBBB\x1b\\` and return luma.
fn parse_osc11_response(response: &str) -> Option<f32> {
    let rgb_start = response.find("rgb:")?;
    let raw = &response[rgb_start + 4..];
    // RRRR/GGGG/BBBB or RR/GG/BB — take the first 2 hex digits of each.
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    let r = u8::from_str_radix(parts[0].get(..2)?, 16).ok()?;
    let g = u8::from_str_radix(parts[1].get(..2)?, 16).ok()?;
    // Blue may have trailing ESC/BEL; take only the leading hex digits.
    let blue_str: String = parts[2]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    let b = u8::from_str_radix(blue_str.get(..2)?, 16).ok()?;

    Some((0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0) // sRGB luma
}

/// Returns `true` if the fd is readable within `timeout_ms`.
#[cfg(target_os = "macos")]
fn wait_for_input(fd: std::os::fd::RawFd, timeout_ms: u64) -> bool {
    unsafe {
        let mut read_fds: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut read_fds);
        let mut tv = libc::timeval {
            tv_sec: (timeout_ms / 1000) as libc::time_t,
            tv_usec: ((timeout_ms % 1000) * 1000) as libc::suseconds_t,
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

#[cfg(all(unix, not(target_os = "macos")))]
fn wait_for_input(fd: std::os::fd::RawFd, timeout_ms: u64) -> bool {
    unsafe {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        libc::poll(&mut pollfd, 1, timeout_ms as libc::c_int) > 0
    }
}

// ---------------------------------------------------------------------------
// Highlight group population
// ---------------------------------------------------------------------------

/// Write smelt's highlight groups into `theme` from its accent/slug/light state.
/// Idempotent — safe to call every frame so `set_accent` mutations propagate.
/// Names follow nvim conventions (`Visual`, `Comment`) with a `Smelt*` prefix
/// for app-specific roles.
pub(crate) fn populate_ui_theme(theme: &mut crate::smelt_term::Theme) {
    use crate::smelt_term::grid::Style;

    let is_light = theme.is_light();
    crate::content::highlight::set_syntax_theme_light(is_light);

    let muted = Color::AnsiValue(244);
    let user_bg = if is_light {
        Color::AnsiValue(254)
    } else {
        Color::AnsiValue(236)
    };
    let code_block_bg = if is_light {
        Color::AnsiValue(255)
    } else {
        Color::AnsiValue(233)
    };
    let bar = if is_light {
        Color::AnsiValue(252)
    } else {
        Color::AnsiValue(237)
    };
    let selection_bg = if is_light {
        Color::AnsiValue(189)
    } else {
        Color::AnsiValue(238)
    };
    let scrollbar_track = if is_light {
        Color::AnsiValue(254)
    } else {
        Color::AnsiValue(235)
    };
    let scrollbar_thumb = if is_light {
        Color::AnsiValue(247)
    } else {
        Color::AnsiValue(243)
    };
    let cursor_line_bg = if is_light {
        Color::AnsiValue(253)
    } else {
        Color::AnsiValue(237)
    };
    let tool_pending = if is_light {
        Color::AnsiValue(250)
    } else {
        Color::DarkGrey
    };
    let reason_off = if is_light {
        Color::AnsiValue(250)
    } else {
        Color::DarkGrey
    };

    theme.set("Visual", Style::new().bg(selection_bg));
    theme.set("CursorLine", Style::new().bg(cursor_line_bg));
    theme.set("Comment", Style::new().fg(muted));
    theme.set("ErrorMsg", Style::new().fg(Color::Red));
    theme.set(
        "GhostText",
        Style {
            dim: true,
            ..Style::default()
        },
    );

    theme.set("SmeltAccent", Style::new().fg(theme.accent_color()));
    theme.set("SmeltSlug", Style::new().bg(theme.slug_color()));
    theme.set("SmeltUserBg", Style::new().bg(user_bg));
    theme.set("SmeltCodeBlockBg", Style::new().bg(code_block_bg));
    theme.set("SmeltBar", Style::new().bg(bar));
    theme.set("SmeltScrollbarTrack", Style::new().bg(scrollbar_track));
    theme.set("SmeltScrollbarThumb", Style::new().bg(scrollbar_thumb));
    theme.set("SmeltToolPending", Style::new().fg(tool_pending));
    theme.set("SmeltReasonOff", Style::new().fg(reason_off));
    theme.set("SmeltSuccess", Style::new().fg(Color::AnsiValue(77)));
    theme.set("SmeltHeading", Style::new().fg(Color::AnsiValue(117)));

    theme.set("SmeltModePlan", Style::new().fg(Color::AnsiValue(79)));
    theme.set("SmeltModeApply", Style::new().fg(Color::AnsiValue(141)));
    theme.set("SmeltModeYolo", Style::new().fg(Color::AnsiValue(204)));
    theme.set("SmeltModeExec", Style::new().fg(Color::AnsiValue(197)));

    theme.set("SmeltReasonLow", Style::new().fg(Color::AnsiValue(75)));
    theme.set("SmeltReasonMed", Style::new().fg(Color::AnsiValue(214)));
    theme.set("SmeltReasonHigh", Style::new().fg(Color::AnsiValue(203)));
    theme.set("SmeltReasonMax", Style::new().fg(Color::AnsiValue(196)));
}

/// Available accent presets: `(name, description, ansi_value)`.
pub const PRESETS: &[(&str, &str, u8)] = &[
    ("ember", "default", crate::smelt_term::DEFAULT_ACCENT),
    ("coral", "salmon pink", 210),
    ("rose", "soft pink", 211),
    ("gold", "warm yellow", 220),
    ("ice", "cool white-blue", 159),
    ("sky", "light blue", 117),
    ("blue", "classic blue", 69),
    ("lavender", "cool purple", 147),
    ("lilac", "warm purple", 183),
    ("mint", "soft green", 115),
    ("sage", "muted green", 108),
    ("silver", "grey", 244),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_osc11_dark_background() {
        // Typical dark terminal (near-black)
        let resp = "\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma < 0.2, "luma {luma} should indicate dark");
    }

    #[test]
    fn parse_osc11_light_background() {
        // Typical light terminal (near-white)
        let resp = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma > 0.9, "luma {luma} should indicate light");
    }

    #[test]
    fn parse_osc11_mid_tone() {
        let resp = "\x1b]11;rgb:8080/8080/8080\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(
            (0.4..0.6).contains(&luma),
            "luma {luma} should be mid-range"
        );
    }

    #[test]
    fn parse_osc11_short_hex() {
        // Some terminals send 2-digit hex
        let resp = "\x1b]11;rgb:ff/ff/ff\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma > 0.9);
    }

    #[test]
    fn parse_osc11_garbage() {
        assert!(parse_osc11_response("garbage").is_none());
        assert!(parse_osc11_response("").is_none());
    }

    #[test]
    fn populate_writes_groups_for_default_theme() {
        let mut t = crate::smelt_term::Theme::new();
        populate_ui_theme(&mut t);
        assert!(t.get("SmeltAccent").fg.is_some());
        assert!(t.get("SmeltSlug").bg.is_some());
        assert!(t.get("Comment").fg.is_some());
    }

    #[test]
    fn populate_reflects_set_accent() {
        let mut t = crate::smelt_term::Theme::new();
        t.set_accent(108); // sage
        populate_ui_theme(&mut t);
        assert_eq!(t.get("SmeltAccent").fg, Some(Color::AnsiValue(108)));
        // slug == 0 falls back to accent
        assert_eq!(t.get("SmeltSlug").bg, Some(Color::AnsiValue(108)));
    }

    #[test]
    fn populate_light_palette_differs_from_dark() {
        let mut dark = crate::smelt_term::Theme::new();
        populate_ui_theme(&mut dark);
        let mut light = crate::smelt_term::Theme::new();
        light.set_light(true);
        populate_ui_theme(&mut light);
        assert_ne!(dark.get("Visual").bg, light.get("Visual").bg);
    }
}
