//! Renderer-facing style + color types.
//!
//! `SpanStyle` is a thin wrapper over `Style` plus an optional
//! `group: HlGroup` for theme-tracked highlight groups. When a span
//! sets only a group with no other axis modifications, the
//! `LineBuilder` emits the group id directly so theme switches flip
//! the rendered span without re-running the parser. Compound spans
//! (`group` plus `bold`/`dim`/etc., or two distinct groups for
//! fg+bg) anonymous-intern at the resolved Style.

pub use crate::buffer::SpanMeta;
use crate::style::{Color, Style};
use crate::theme::HlGroup;

/// Renderer-facing style. Concrete colors plus an optional theme
/// group id. The group's resolved fg/bg layer underneath the explicit
/// `fg`/`bg` (explicit wins).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpanStyle {
    /// Theme group whose fg/bg fills any unset `fg`/`bg` slot. When
    /// the rest of the style is default, the collector emits this id
    /// directly so theme switches track live.
    pub group: Option<HlGroup>,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub crossedout: bool,
}

impl SpanStyle {
    pub fn from_group(group: HlGroup) -> Self {
        Self {
            group: Some(group),
            ..Default::default()
        }
    }

    pub fn from_fg(fg: Color) -> Self {
        Self {
            fg: Some(fg),
            ..Default::default()
        }
    }

    /// Whether this style carries any axis modification beyond a
    /// possible `group` reference.
    pub fn has_axis_mods(&self) -> bool {
        self.fg.is_some()
            || self.bg.is_some()
            || self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.crossedout
    }
}

/// Convert a `Style` into a `SpanStyle` (concrete-color path; no
/// group). Used by replay paths that read pre-resolved styles back
/// from a Buffer.
impl From<Style> for SpanStyle {
    fn from(s: Style) -> Self {
        Self {
            group: None,
            fg: s.fg,
            bg: s.bg,
            bold: s.bold,
            dim: s.dim,
            italic: s.italic,
            underline: s.underline,
            crossedout: s.crossedout,
        }
    }
}
