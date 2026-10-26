//! How many colors the terminal can show, decided once from the environment.

use std::sync::OnceLock;

/// Color capability the output encoder targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit RGB escapes are understood.
    TrueColor,
    /// Only the 256-color palette is safe; RGB snaps to the nearest entry.
    Palette256,
}

/// Environment variable that overrides detection: `truecolor` (or `24bit`)
/// or `256`. Anything else falls back to detection.
pub const COLOR_ENV: &str = "SMELT_COLOR";

const TRUECOLOR_PROGRAMS: &[&str] = &["iTerm.app", "vscode", "WezTerm", "ghostty", "Hyper"];
const TRUECOLOR_TERMS: &[&str] = &["kitty", "alacritty", "ghostty", "wezterm"];

/// Decide the color depth from an environment lookup. Without a truecolor
/// signal the answer is the 256-color palette, which every terminal in
/// common use (Terminal.app, mosh, screen, the Linux console) handles.
pub fn detect_color_depth(env: impl Fn(&str) -> Option<String>) -> ColorDepth {
    let get = |name: &str| env(name).map(|value| value.trim().to_ascii_lowercase());
    match get(COLOR_ENV).as_deref() {
        Some("truecolor" | "24bit") => return ColorDepth::TrueColor,
        Some("256") => return ColorDepth::Palette256,
        _ => {}
    }
    let colorterm = get("COLORTERM");
    if matches!(colorterm.as_deref(), Some("truecolor" | "24bit")) {
        return ColorDepth::TrueColor;
    }
    if env("TERM_PROGRAM").is_some_and(|program| TRUECOLOR_PROGRAMS.contains(&program.trim())) {
        return ColorDepth::TrueColor;
    }
    if env("WT_SESSION").is_some_and(|session| !session.is_empty()) {
        return ColorDepth::TrueColor;
    }
    if get("TERM").is_some_and(|term| {
        term.ends_with("-direct") || TRUECOLOR_TERMS.iter().any(|known| term.contains(known))
    }) {
        return ColorDepth::TrueColor;
    }
    ColorDepth::Palette256
}

/// The depth for this process, detected from the real environment on first use.
pub fn color_depth() -> ColorDepth {
    static DEPTH: OnceLock<ColorDepth> = OnceLock::new();
    *DEPTH.get_or_init(|| detect_color_depth(|name| std::env::var(name).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(pairs: &[(&str, &str)]) -> ColorDepth {
        detect_color_depth(|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        })
    }

    #[test]
    fn no_signal_means_the_256_palette() {
        assert_eq!(detect(&[]), ColorDepth::Palette256);
        assert_eq!(
            detect(&[("TERM", "xterm-256color")]),
            ColorDepth::Palette256
        );
        assert_eq!(
            detect(&[
                ("TERM", "screen-256color"),
                ("TERM_PROGRAM", "Apple_Terminal")
            ]),
            ColorDepth::Palette256
        );
        assert_eq!(detect(&[("TERM", "linux")]), ColorDepth::Palette256);
    }

    #[test]
    fn colorterm_advertises_truecolor() {
        assert_eq!(detect(&[("COLORTERM", "truecolor")]), ColorDepth::TrueColor);
        assert_eq!(detect(&[("COLORTERM", "24bit")]), ColorDepth::TrueColor);
        assert_eq!(detect(&[("COLORTERM", "TrueColor")]), ColorDepth::TrueColor);
        assert_eq!(detect(&[("COLORTERM", "yes")]), ColorDepth::Palette256);
    }

    #[test]
    fn known_terminal_programs_and_terms_are_truecolor() {
        for program in ["iTerm.app", "vscode", "WezTerm", "ghostty", "Hyper"] {
            assert_eq!(
                detect(&[("TERM_PROGRAM", program)]),
                ColorDepth::TrueColor,
                "{program}"
            );
        }
        for term in [
            "xterm-kitty",
            "alacritty",
            "xterm-ghostty",
            "wezterm",
            "xterm-direct",
        ] {
            assert_eq!(detect(&[("TERM", term)]), ColorDepth::TrueColor, "{term}");
        }
        assert_eq!(detect(&[("WT_SESSION", "1b2f")]), ColorDepth::TrueColor);
        assert_eq!(detect(&[("WT_SESSION", "")]), ColorDepth::Palette256);
    }

    #[test]
    fn explicit_override_wins_over_detection() {
        assert_eq!(
            detect(&[(COLOR_ENV, "256"), ("COLORTERM", "truecolor")]),
            ColorDepth::Palette256
        );
        assert_eq!(
            detect(&[(COLOR_ENV, "truecolor"), ("TERM", "linux")]),
            ColorDepth::TrueColor
        );
        assert_eq!(detect(&[(COLOR_ENV, "24bit")]), ColorDepth::TrueColor);
        assert_eq!(
            detect(&[(COLOR_ENV, "bogus"), ("COLORTERM", "truecolor")]),
            ColorDepth::TrueColor
        );
    }
}
