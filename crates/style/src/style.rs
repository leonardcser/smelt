//! Cell style: fg/bg color and text attributes. No terminal dependencies.

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

impl Color {
    pub fn mix_toward(self, target: Color, amount: f32) -> Option<Color> {
        if matches!(self, Color::Reset) || matches!(target, Color::Reset) {
            return None;
        }
        let (r, g, b) = self.rgb();
        let (tr, tg, tb) = target.rgb();
        let amount = amount.clamp(0.0, 1.0);
        Some(Color::Rgb {
            r: mix_channel(r, tr, amount),
            g: mix_channel(g, tg, amount),
            b: mix_channel(b, tb, amount),
        })
    }

    fn rgb(self) -> (u8, u8, u8) {
        match self {
            Color::Reset => (0, 0, 0),
            Color::Black => (0, 0, 0),
            Color::DarkGrey => (128, 128, 128),
            Color::Red => (255, 0, 0),
            Color::DarkRed => (128, 0, 0),
            Color::Green => (0, 255, 0),
            Color::DarkGreen => (0, 128, 0),
            Color::Yellow => (255, 255, 0),
            Color::DarkYellow => (128, 128, 0),
            Color::Blue => (0, 0, 255),
            Color::DarkBlue => (0, 0, 128),
            Color::Magenta => (255, 0, 255),
            Color::DarkMagenta => (128, 0, 128),
            Color::Cyan => (0, 255, 255),
            Color::DarkCyan => (0, 128, 128),
            Color::White => (255, 255, 255),
            Color::Grey => (192, 192, 192),
            Color::Rgb { r, g, b } => (r, g, b),
            Color::AnsiValue(value) => ansi_value_rgb(value),
        }
    }
}

/// Nearest ANSI 256-color index for an sRGB triple: the closer, by squared
/// distance, of the xterm 6×6×6 cube entry and the 24-step gray ramp.
pub fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    fn band(c: u8) -> usize {
        (0..LEVELS.len())
            .min_by_key(|&i| (c as i32 - LEVELS[i]).abs())
            .unwrap_or(0)
    }
    fn distance(a: (i32, i32, i32), b: (i32, i32, i32)) -> i32 {
        (a.0 - b.0).pow(2) + (a.1 - b.1).pow(2) + (a.2 - b.2).pow(2)
    }
    let target = (r as i32, g as i32, b as i32);
    let (ri, gi, bi) = (band(r), band(g), band(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let step = ((target.0 + target.1 + target.2) / 3 - 3)
        .div_euclid(10)
        .clamp(0, 23);
    let level = 8 + 10 * step;
    if distance(target, (level, level, level)) < distance(target, cube) {
        232 + step as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

impl Color {
    /// The color as a terminal limited to the 256-color palette would show
    /// it: RGB snaps to the nearest palette entry, everything else is kept.
    pub fn to_palette(self) -> Color {
        match self {
            Color::Rgb { r, g, b } => Color::AnsiValue(rgb_to_ansi256(r, g, b)),
            other => other,
        }
    }
}

fn mix_channel(value: u8, target: u8, amount: f32) -> u8 {
    (value as f32 + (target as f32 - value as f32) * amount).round() as u8
}

fn ansi_value_rgb(value: u8) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
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
    if value < 16 {
        return BASIC[value as usize];
    }
    if value < 232 {
        let idx = value - 16;
        let step = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
        return (step(idx / 36), step((idx / 6) % 6), step(idx % 6));
    }
    let v = 8 + (value - 232) * 10;
    (v, v, v)
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
    pub reverse: bool,
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
            reverse: false,
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

    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_builder_methods_set_their_fields() {
        let s = Style::new()
            .fg(Color::Red)
            .bg(Color::Blue)
            .bold()
            .dim()
            .italic()
            .underline()
            .crossedout()
            .reverse();
        assert_eq!(
            s,
            Style {
                fg: Some(Color::Red),
                bg: Some(Color::Blue),
                bold: true,
                dim: true,
                italic: true,
                underline: true,
                crossedout: true,
                reverse: true,
            }
        );
    }

    #[test]
    fn color_mix_toward_handles_named_and_ansi_colors() {
        assert_eq!(
            Color::Black.mix_toward(Color::White, 0.5),
            Some(Color::Rgb {
                r: 128,
                g: 128,
                b: 128,
            })
        );
        assert_eq!(
            Color::AnsiValue(196).mix_toward(Color::Black, 0.5),
            Some(Color::Rgb { r: 128, g: 0, b: 0 })
        );
        assert_eq!(Color::Reset.mix_toward(Color::White, 0.5), None);
    }

    #[test]
    fn rgb_to_ansi256_hits_cube_corners_and_edges() {
        // 255,0,0 is the (5,0,0) cube corner: 16 + 36 * 5.
        assert_eq!(rgb_to_ansi256(255, 0, 0), 196);
        assert_eq!(rgb_to_ansi256(0, 0, 0), 16);
        assert_eq!(rgb_to_ansi256(255, 255, 255), 231);
    }

    #[test]
    fn rgb_to_ansi256_prefers_the_gray_ramp_for_grays() {
        // 128 is exactly a ramp level (8 + 10 * 12); the nearest cube gray is 135.
        assert_eq!(rgb_to_ansi256(128, 128, 128), 244);
        // Not an exact gray, but far closer to the ramp (98) than to cube 95,95,95.
        assert_eq!(rgb_to_ansi256(100, 102, 101), 241);
    }

    #[test]
    fn rgb_to_ansi256_keeps_saturated_colors_in_the_cube() {
        assert_eq!(rgb_to_ansi256(230, 90, 20), 166);
    }

    #[test]
    fn to_palette_only_converts_rgb() {
        assert_eq!(
            Color::Rgb { r: 255, g: 0, b: 0 }.to_palette(),
            Color::AnsiValue(196)
        );
        assert_eq!(Color::Red.to_palette(), Color::Red);
        assert_eq!(Color::AnsiValue(208).to_palette(), Color::AnsiValue(208));
        assert_eq!(Color::Reset.to_palette(), Color::Reset);
    }
}
