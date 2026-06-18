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
}
