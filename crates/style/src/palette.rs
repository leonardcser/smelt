//! The terminal's own colors: its default foreground and background and the
//! 16 ANSI colors. Named and low indexed colors, and "no color", only get a
//! real value through the palette of the terminal that shows them.

use crate::style::Color;
use std::sync::OnceLock;

/// An sRGB triple.
pub type Rgb = (u8, u8, u8);

/// Which side of a cell a color paints. A missing color means the terminal
/// default for that side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layer {
    Fg,
    Bg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    pub ansi: [Rgb; 16],
}

/// The xterm defaults for the 16 ANSI colors.
pub const XTERM_ANSI: [Rgb; 16] = [
    (0, 0, 0),
    (128, 0, 0),
    (0, 128, 0),
    (128, 128, 0),
    (0, 0, 128),
    (128, 0, 128),
    (0, 128, 128),
    (192, 192, 192),
    (128, 128, 128),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (0, 0, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

impl Palette {
    /// The xterm palette with a dark or light default background.
    pub const fn xterm(light: bool) -> Self {
        let (fg, bg) = if light {
            ((0, 0, 0), (255, 255, 255))
        } else {
            ((229, 229, 229), (0, 0, 0))
        };
        Self {
            fg,
            bg,
            ansi: XTERM_ANSI,
        }
    }

    /// The color a terminal with this palette shows for `color` on `layer`.
    pub fn rgb(&self, color: Option<Color>, layer: Layer) -> Rgb {
        let default = match layer {
            Layer::Fg => self.fg,
            Layer::Bg => self.bg,
        };
        match color {
            None | Some(Color::Reset) => default,
            Some(color) => match ansi_index(color) {
                Some(index) => self.ansi[usize::from(index)],
                None => color.rgb().unwrap_or(default),
            },
        }
    }

    /// Whether the default background reads as light.
    pub fn is_light(&self) -> bool {
        luma(self.bg) > 0.6
    }
}

/// Relative sRGB luma in `0.0..=1.0`.
fn luma((r, g, b): Rgb) -> f32 {
    (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)) / 255.0
}

/// The ANSI palette slot a named or low indexed color reads from.
fn ansi_index(color: Color) -> Option<u8> {
    Some(match color {
        Color::Black => 0,
        Color::DarkRed => 1,
        Color::DarkGreen => 2,
        Color::DarkYellow => 3,
        Color::DarkBlue => 4,
        Color::DarkMagenta => 5,
        Color::DarkCyan => 6,
        Color::Grey => 7,
        Color::DarkGrey => 8,
        Color::Red => 9,
        Color::Green => 10,
        Color::Yellow => 11,
        Color::Blue => 12,
        Color::Magenta => 13,
        Color::Cyan => 14,
        Color::White => 15,
        Color::AnsiValue(index) if index < 16 => index,
        _ => return None,
    })
}

static TERMINAL: OnceLock<Palette> = OnceLock::new();
const FALLBACK: Palette = Palette::xterm(false);

/// The palette of the terminal smelt draws to: what the startup probe read,
/// or the dark xterm palette when it has not run or got no answer.
pub fn terminal() -> &'static Palette {
    TERMINAL.get().unwrap_or(&FALLBACK)
}

/// Record the terminal's palette. Only the first call takes effect.
pub fn set_terminal(palette: Palette) {
    let _ = TERMINAL.set(palette);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_defaults_named_and_indexed_colors() {
        let mut palette = Palette::xterm(false);
        palette.fg = (1, 2, 3);
        palette.bg = (4, 5, 6);
        palette.ansi[1] = (200, 10, 10);

        assert_eq!(palette.rgb(None, Layer::Fg), (1, 2, 3));
        assert_eq!(palette.rgb(Some(Color::Reset), Layer::Bg), (4, 5, 6));
        assert_eq!(palette.rgb(Some(Color::DarkRed), Layer::Fg), (200, 10, 10));
        assert_eq!(
            palette.rgb(Some(Color::AnsiValue(1)), Layer::Fg),
            (200, 10, 10)
        );
        assert_eq!(
            palette.rgb(Some(Color::AnsiValue(196)), Layer::Fg),
            (255, 0, 0)
        );
        let rgb = Color::Rgb { r: 7, g: 8, b: 9 };
        assert_eq!(palette.rgb(Some(rgb), Layer::Bg), (7, 8, 9));
    }

    #[test]
    fn light_follows_the_background() {
        assert!(Palette::xterm(true).is_light());
        assert!(!Palette::xterm(false).is_light());
    }
}
