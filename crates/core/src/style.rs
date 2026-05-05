//! Cell style — fg/bg + text attributes. Pure data; shared between
//! `Buffer` extmark payloads and any frontend's render layer.
//!
//! `Color` is a frontend-neutral mirror of crossterm's `style::Color`
//! variant set: `Reset`, the 16 named ANSI slots, an indexed
//! `AnsiValue(u8)`, and `Rgb { r, g, b }`. Frontends interpret the
//! variants for their target — the terminal frontend converts to
//! `crossterm::style::Color` at SGR-emit time; a future GUI frontend
//! defines its own mapping (named slots → theme palette, `Reset` →
//! "use the theme's default text color", etc.). Keeping the shape
//! neutral means `core` carries no terminal dep.
//!
//! The longer-term plan (P9.e — see `refactor/P9.md`) replaces the
//! raw-color model with HlGroup ids: extmark Highlight payloads
//! reference a semantic group, the theme resolves to a `Style` at
//! paint time, and Buffer never carries colors at all. `Color` /
//! `Style` survive at the paint-layer boundary; today they live
//! inside extmark payloads as a transitional state.

#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum Color {
    Reset,
    Black,
    DarkGrey,
    Red,
    DarkRed,
    Green,
    DarkGreen,
    Yellow,
    DarkYellow,
    Blue,
    DarkBlue,
    Magenta,
    DarkMagenta,
    Cyan,
    DarkCyan,
    White,
    Grey,
    Rgb { r: u8, g: u8, b: u8 },
    AnsiValue(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub crossedout: bool,
}

impl Style {
    pub fn fg(color: Color) -> Self {
        Self {
            fg: Some(color),
            ..Default::default()
        }
    }

    pub fn bg(color: Color) -> Self {
        Self {
            bg: Some(color),
            ..Default::default()
        }
    }

    pub fn bold() -> Self {
        Self {
            bold: true,
            ..Default::default()
        }
    }

    pub fn dim() -> Self {
        Self {
            dim: true,
            ..Default::default()
        }
    }
}
