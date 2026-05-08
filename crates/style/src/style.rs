//! Cell style — fg/bg + text attributes. Pure data; shared between
//! document models that carry styled spans and any frontend's render
//! layer.
//!
//! `Color` is a frontend-neutral mirror of crossterm's `style::Color`
//! variant set: `Reset`, the 16 named ANSI slots, an indexed
//! `AnsiValue(u8)`, and `Rgb { r, g, b }`. Frontends interpret the
//! variants for their target — the terminal frontend converts to
//! `crossterm::style::Color` at SGR-emit time; a future GUI frontend
//! defines its own mapping (named slots → theme palette, `Reset` →
//! "use the theme's default text color", etc.). Keeping the shape
//! neutral means consumers of this crate carry no terminal dep.

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            crossedout: false,
        }
    }

    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub fn crossedout(mut self) -> Self {
        self.crossedout = true;
        self
    }
}
