//! Renderer-facing style + color types.
//!
//! These are the "template" forms used by block renderers: a renderer
//! builds up a [`SpanStyle`] (with theme-relative [`ColorValue::Role`]
//! refs), pushes it onto the [`crate::content::layout_out::SpanCollector`],
//! and the collector resolves to a concrete [`crate::style::Style`]
//! (with [`crate::style::Color`]) when emitting the span. Buffer +
//! Window only ever see resolved styles.

use serde::{Deserialize, Serialize};

pub use crate::buffer::SpanMeta;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanStyle {
    pub fg: Option<ColorValue>,
    pub bg: Option<ColorValue>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub crossedout: bool,
}

/// Color value that may be theme-dependent (resolved at write time)
/// or fixed (raw RGB / named ANSI / 256-color palette index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorValue {
    Rgb(u8, u8, u8),
    Ansi(u8),
    Named(NamedColor),
    Role(ColorRole),
}

/// Theme-dependent semantic colors. Resolved by `SpanCollector` against
/// the active [`crate::theme::Theme`] at write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorRole {
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
pub enum NamedColor {
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
