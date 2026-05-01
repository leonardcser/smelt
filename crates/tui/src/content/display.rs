//! Span-tree representation of one laid-out block.
//!
//! Block renderers produce a `DisplayBlock` (a tree of styled spans grouped
//! into visual lines) during the layout stage. The paint stage walks the
//! tree and emits SGR sequences for the current `Theme` snapshot. Theme
//! colors are stored as semantic `ColorValue::Role(...)` and resolved at
//! paint time, so cached layouts survive theme changes without
//! invalidation.

use crossterm::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DisplayBlock {
    pub(crate) lines: Vec<DisplayLine>,
    /// Terminal width this layout was computed at.
    pub(crate) layout_width: u16,
    /// True iff layout broke at least one logical line into multiple
    /// visual rows. When false, the layout is replayable at any width
    /// >= `max_line_width`. When true it is pinned to `layout_width`.
    pub(crate) was_wrapped: bool,
    /// Longest visible line in the layout (display columns).
    pub(crate) max_line_width: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DisplayLine {
    pub(crate) spans: Vec<DisplaySpan>,
    /// Optional bg color for the left gutter column(s). When set,
    /// `paint_line` fills the gutter with this color instead of blank
    /// spaces. Used by User blocks whose background bleeds into the
    /// gutter while content stays in content-rect coords.
    #[serde(default)]
    pub(crate) gutter_bg: Option<ColorValue>,
    /// Optional bg color that extends from end-of-spans to
    /// `term_width - right_margin` at paint time. Used by diff and code
    /// rows to fill the row with a background color.
    #[serde(default)]
    pub(crate) fill_bg: Option<ColorValue>,
    /// Width (in display columns) reserved on the right side when
    /// `fill_bg` extends the line. The fill stops `right_margin` columns
    /// short of the terminal edge.
    #[serde(default)]
    pub(crate) fill_right_margin: u16,
    /// True when this visual row is a continuation of the previous row's
    /// logical line (soft-wrapped). `copy_range` suppresses `\n` before
    /// soft-wrapped rows so copied text matches the source.
    #[serde(default)]
    pub(crate) soft_wrapped: bool,
    /// Raw source line this display row was rendered from. Set by
    /// `render_markdown_inner` on the first segment of each source line.
    /// Soft-wrap continuations leave this `None`. `copy_range` emits
    /// this instead of display text for fully-selected rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DisplaySpan {
    pub(crate) text: String,
    pub(crate) style: SpanStyle,
    #[serde(default)]
    pub(crate) meta: SpanMeta,
}

/// Per-span selection + copy semantics. Carried alongside `SpanStyle`
/// so the copy path and hit-testing don't have to parse layout
/// structure after the fact.
///
/// - `selectable = false` cells are skipped by selection (diff gutter,
///   quote bar, line-number column, left/right padding).
/// - `copy_as = Some(s)` substitutes `s` for each cell on copy;
///   `Some("")` drops the cell; `None` emits the underlying char.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SpanMeta {
    pub(crate) selectable: bool,
    pub(crate) copy_as: Option<String>,
}

impl Default for SpanMeta {
    fn default() -> Self {
        Self {
            selectable: true,
            copy_as: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SpanStyle {
    pub(crate) fg: Option<ColorValue>,
    pub(crate) bg: Option<ColorValue>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) crossedout: bool,
}

/// Color value that may be theme-dependent (resolved at paint time) or
/// fixed (raw RGB / named ANSI / 256-color palette index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ColorValue {
    Rgb(u8, u8, u8),
    Ansi(u8),
    Named(NamedColor),
    Role(ColorRole),
}

/// Theme-dependent semantic colors. Resolved by `to_buffer::resolve`
/// against the active `ui::Theme` registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ColorRole {
    Accent,
    Slug,
    UserBg,
    CodeBlockBg,
    Bar,
    ToolPending,
    ReasonOff,
    Muted,
    Success,
    ErrorMsg,
    Apply,
    Plan,
    Exec,
    Heading,
    ReasonLow,
    ReasonMed,
    ReasonHigh,
    ReasonMax,
}

/// Mirror of crossterm's named colors. We can't store crossterm::Color
/// directly because it isn't `Eq` and we want a stable serde shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NamedColor {
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
}

impl DisplayBlock {
    pub(crate) fn rows(&self) -> u16 {
        self.lines.len() as u16
    }

    pub(crate) fn is_valid_at(&self, new_width: u16) -> bool {
        if self.was_wrapped {
            new_width == self.layout_width
        } else {
            new_width >= self.max_line_width
        }
    }
}

impl From<Color> for ColorValue {
    fn from(c: Color) -> Self {
        match c {
            Color::Reset => ColorValue::Named(NamedColor::Reset),
            Color::Black => ColorValue::Named(NamedColor::Black),
            Color::DarkGrey => ColorValue::Named(NamedColor::DarkGrey),
            Color::Red => ColorValue::Named(NamedColor::Red),
            Color::DarkRed => ColorValue::Named(NamedColor::DarkRed),
            Color::Green => ColorValue::Named(NamedColor::Green),
            Color::DarkGreen => ColorValue::Named(NamedColor::DarkGreen),
            Color::Yellow => ColorValue::Named(NamedColor::Yellow),
            Color::DarkYellow => ColorValue::Named(NamedColor::DarkYellow),
            Color::Blue => ColorValue::Named(NamedColor::Blue),
            Color::DarkBlue => ColorValue::Named(NamedColor::DarkBlue),
            Color::Magenta => ColorValue::Named(NamedColor::Magenta),
            Color::DarkMagenta => ColorValue::Named(NamedColor::DarkMagenta),
            Color::Cyan => ColorValue::Named(NamedColor::Cyan),
            Color::DarkCyan => ColorValue::Named(NamedColor::DarkCyan),
            Color::White => ColorValue::Named(NamedColor::White),
            Color::Grey => ColorValue::Named(NamedColor::Grey),
            Color::Rgb { r, g, b } => ColorValue::Rgb(r, g, b),
            Color::AnsiValue(v) => ColorValue::Ansi(v),
        }
    }
}

impl From<NamedColor> for Color {
    fn from(n: NamedColor) -> Self {
        match n {
            NamedColor::Reset => Color::Reset,
            NamedColor::Black => Color::Black,
            NamedColor::DarkGrey => Color::DarkGrey,
            NamedColor::Red => Color::Red,
            NamedColor::DarkRed => Color::DarkRed,
            NamedColor::Green => Color::Green,
            NamedColor::DarkGreen => Color::DarkGreen,
            NamedColor::Yellow => Color::Yellow,
            NamedColor::DarkYellow => Color::DarkYellow,
            NamedColor::Blue => Color::Blue,
            NamedColor::DarkBlue => Color::DarkBlue,
            NamedColor::Magenta => Color::Magenta,
            NamedColor::DarkMagenta => Color::DarkMagenta,
            NamedColor::Cyan => Color::Cyan,
            NamedColor::DarkCyan => Color::DarkCyan,
            NamedColor::White => Color::White,
            NamedColor::Grey => Color::Grey,
        }
    }
}
