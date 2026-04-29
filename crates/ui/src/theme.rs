//! Theme registry — nvim-style highlight groups.
//!
//! Code references *names* (`"Visual"`, `"SmeltAccent"`, …); the host
//! populates the registry at startup; users override names via Lua.
//! Names that aren't set fall back to `Style::default()` — nvim's
//! policy of no panic on typo. `link()` aliases one name to another.
//!
//! Replaces the host-side `crate::theme::*` flat module of color
//! constants and the `Ui::set_selection_bg` / `selection_style()` shim
//! that fanned out one slot to every widget.

use crate::grid::Style;
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct Theme {
    groups: HashMap<String, Style>,
    links: HashMap<String, String>,
    /// Whether the host terminal has a light background. Read by the
    /// host's default-theme builder to choose the correct palette.
    /// Detected once at startup via OSC 11 query.
    is_light: bool,
}

impl Theme {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, style: Style) {
        let name = name.into();
        self.links.remove(&name);
        self.groups.insert(name, style);
    }

    pub fn link(&mut self, from: impl Into<String>, to: impl Into<String>) {
        let from = from.into();
        self.groups.remove(&from);
        self.links.insert(from, to.into());
    }

    pub fn get(&self, name: &str) -> Style {
        let mut visited: usize = 0;
        let mut cur = name;
        while let Some(target) = self.links.get(cur) {
            visited += 1;
            if visited > 16 {
                return Style::default();
            }
            cur = target;
        }
        self.groups.get(cur).copied().unwrap_or_default()
    }

    pub fn is_light(&self) -> bool {
        self.is_light
    }

    pub fn set_light(&mut self, light: bool) {
        self.is_light = light;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::style::Color;

    #[test]
    fn unknown_name_returns_default() {
        let t = Theme::new();
        assert_eq!(t.get("Nonexistent"), Style::default());
    }

    #[test]
    fn set_and_get_round_trip() {
        let mut t = Theme::new();
        let s = Style {
            fg: Some(Color::Red),
            bold: true,
            ..Style::default()
        };
        t.set("Error", s);
        assert_eq!(t.get("Error"), s);
    }

    #[test]
    fn link_chases_to_target() {
        let mut t = Theme::new();
        t.set("Visual", Style::bg(Color::AnsiValue(237)));
        t.link("SearchHighlight", "Visual");
        assert_eq!(t.get("SearchHighlight"), t.get("Visual"));
    }

    #[test]
    fn link_chain_resolves() {
        let mut t = Theme::new();
        t.set("Base", Style::bg(Color::AnsiValue(42)));
        t.link("Mid", "Base");
        t.link("Top", "Mid");
        assert_eq!(t.get("Top"), t.get("Base"));
    }

    #[test]
    fn cyclic_link_returns_default_without_panic() {
        let mut t = Theme::new();
        t.link("A", "B");
        t.link("B", "A");
        assert_eq!(t.get("A"), Style::default());
    }

    #[test]
    fn set_overwrites_existing_link() {
        let mut t = Theme::new();
        t.set("Visual", Style::bg(Color::AnsiValue(237)));
        t.link("Search", "Visual");
        let direct = Style::bg(Color::AnsiValue(220));
        t.set("Search", direct);
        assert_eq!(t.get("Search"), direct);
    }

    #[test]
    fn link_overwrites_existing_set() {
        let mut t = Theme::new();
        t.set("X", Style::bg(Color::AnsiValue(1)));
        t.set("Y", Style::bg(Color::AnsiValue(2)));
        t.link("X", "Y");
        assert_eq!(t.get("X"), t.get("Y"));
    }
}
