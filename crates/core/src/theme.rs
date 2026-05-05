//! Theme registry — nvim-style highlight groups.
//!
//! Code references *names* (`"Visual"`, `"SmeltAccent"`, …); the host
//! populates the registry at startup; users override names via Lua.
//! Names that aren't set fall back to `Style::default()` — nvim's
//! policy of no panic on typo. `link()` aliases one name to another.
//!
//! Replaces the host-side `super::theme::*` flat module of color
//! constants and the `Ui::set_selection_bg` / `selection_style()` shim
//! that fanned out one slot to every widget.
//!
//! P9.e: highlight groups are interned to a small u32 id (`HlGroup`)
//! so extmarks can store the id instead of a resolved `Style`. The
//! resolution happens at paint time via [`Theme::resolve`]; theme
//! switches mutate the same Theme and update existing ids' styles —
//! buffers don't need rebuilding. The interner is a singleton
//! `HlGroupRegistry` shared by every Theme so ids stay stable across
//! tests and across switches.

use crate::style::{Color, Style};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// Interned highlight-group id. Minted by [`HlGroupRegistry::intern`];
/// stable for the process lifetime. Theme stores `HlGroup → Style`;
/// extmark Highlight payloads carry the id (P9.e).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HlGroup(pub u32);

/// Process-global name → id interner. Decoupled from `Theme` so ids
/// stay stable across theme switches and across multiple Theme
/// instances (tests, headless harness). The actual `Style` for each id
/// lives on whichever `Theme` is queried at paint time — different
/// themes can give the same id different styles.
struct HlGroupRegistry {
    name_to_id: HashMap<String, HlGroup>,
    id_to_name: Vec<String>,
}

impl HlGroupRegistry {
    fn new() -> Self {
        Self {
            name_to_id: HashMap::new(),
            id_to_name: Vec::new(),
        }
    }

    fn intern(&mut self, name: &str) -> HlGroup {
        if let Some(id) = self.name_to_id.get(name) {
            return *id;
        }
        let id = HlGroup(self.id_to_name.len() as u32);
        self.name_to_id.insert(name.to_string(), id);
        self.id_to_name.push(name.to_string());
        id
    }
}

fn registry() -> &'static RwLock<HlGroupRegistry> {
    static REG: OnceLock<RwLock<HlGroupRegistry>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HlGroupRegistry::new()))
}

/// Get-or-mint the [`HlGroup`] id for `name`. Stable across the whole
/// process; the same name always interns to the same id.
pub fn intern(name: &str) -> HlGroup {
    if let Some(id) = registry().read().unwrap().name_to_id.get(name).copied() {
        return id;
    }
    registry().write().unwrap().intern(name)
}

/// Reverse the interner: id → name. `None` for an id from a different
/// process or never minted (shouldn't happen in practice).
pub fn name_of(g: HlGroup) -> Option<String> {
    registry()
        .read()
        .unwrap()
        .id_to_name
        .get(g.0 as usize)
        .cloned()
}

/// Intern a Style as an anonymous group keyed by its content hash.
/// Used during the P9.e migration to store legacy inline-Style call
/// sites in the new HlGroup-keyed extmark payload without forcing a
/// name on every site. Future commits convert call sites to named
/// groups (`intern("My.Name")`); anonymous groups are bypassed by
/// theme switches because there's no name to override.
pub fn intern_anonymous_style(style: Style) -> HlGroup {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    style.hash(&mut h);
    let key = format!("__anon__/{:016x}", h.finish());
    let id = intern(&key);
    // Make sure the ANON Theme entry has this style so resolve() works
    // even if no Theme called set() with this id. We stash the style
    // in a parallel global map keyed by HlGroup.
    anon_styles().write().unwrap().insert(id, style);
    id
}

fn anon_styles() -> &'static RwLock<HashMap<HlGroup, Style>> {
    static MAP: OnceLock<RwLock<HashMap<HlGroup, Style>>> = OnceLock::new();
    MAP.get_or_init(|| RwLock::new(HashMap::new()))
}

fn anon_resolve(id: HlGroup) -> Option<Style> {
    anon_styles().read().unwrap().get(&id).copied()
}

/// Default accent palette index — `Color::AnsiValue(208)`,
/// the warm orange "ember" preset.
pub const DEFAULT_ACCENT: u8 = 208;

#[derive(Debug, Clone)]
pub struct Theme {
    /// Resolved styles, keyed by interned HlGroup id (P9.e). Sparse
    /// — `get`/`resolve` fall back to `Style::default()` for ids that
    /// were never set through this Theme.
    styles: HashMap<HlGroup, Style>,
    /// Group links: source HlGroup → target HlGroup. Resolved at
    /// `resolve()` time, max chain depth 16 (cycle defense).
    links: HashMap<HlGroup, HlGroup>,
    /// Whether the host terminal has a light background. Read by the
    /// host's default-theme builder to choose the correct palette.
    /// Detected once at startup via OSC 11 query.
    is_light: bool,
    /// Accent palette index (ANSI 256-color). Tracked separately from
    /// the `SmeltAccent` group entry so a host palette rebuild
    /// (light/dark flip, preset swap) is a single setter call.
    accent: u8,
    /// Slug pill background palette index. `0` means "use accent."
    /// Stored separately from `SmeltSlug` for the same reason as
    /// `accent`.
    slug: u8,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            styles: HashMap::new(),
            links: HashMap::new(),
            is_light: false,
            accent: DEFAULT_ACCENT,
            slug: 0,
        }
    }
}

impl Theme {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, style: Style) {
        let id = intern(&name.into());
        self.links.remove(&id);
        self.styles.insert(id, style);
    }

    pub fn link(&mut self, from: impl Into<String>, to: impl Into<String>) {
        let from_id = intern(&from.into());
        let to_id = intern(&to.into());
        self.styles.remove(&from_id);
        self.links.insert(from_id, to_id);
    }

    /// Resolve a name to its current Style, following links. Always
    /// returns a value — unknown names get `Style::default()` (nvim
    /// policy: typos don't panic).
    pub fn get(&self, name: &str) -> Style {
        self.resolve(intern(name))
    }

    /// Resolve a HlGroup id to its current Style. Follows up to 16
    /// link hops; cycles fall back to default. Anonymous ids
    /// (`intern_anonymous_style`) bypass `Theme.styles` and read the
    /// global anon-style map.
    pub fn resolve(&self, hl: HlGroup) -> Style {
        let mut cur = hl;
        let mut visited: usize = 0;
        while let Some(target) = self.links.get(&cur) {
            visited += 1;
            if visited > 16 {
                return Style::default();
            }
            cur = *target;
        }
        if let Some(style) = self.styles.get(&cur).copied() {
            return style;
        }
        anon_resolve(cur).unwrap_or_default()
    }

    /// Get-or-mint the HlGroup id for `name`. Convenience wrapper
    /// around the module-level `intern`.
    pub fn id_for(&self, name: &str) -> HlGroup {
        intern(name)
    }

    pub fn is_light(&self) -> bool {
        self.is_light
    }

    pub fn set_light(&mut self, light: bool) {
        self.is_light = light;
    }

    pub fn accent(&self) -> u8 {
        self.accent
    }

    pub fn set_accent(&mut self, ansi: u8) {
        self.accent = ansi;
    }

    pub fn accent_color(&self) -> Color {
        Color::AnsiValue(self.accent)
    }

    pub fn slug(&self) -> u8 {
        self.slug
    }

    pub fn set_slug(&mut self, ansi: u8) {
        self.slug = ansi;
    }

    /// Resolved slug pill background. `slug == 0` falls back to accent.
    pub fn slug_color(&self) -> Color {
        if self.slug == 0 {
            self.accent_color()
        } else {
            Color::AnsiValue(self.slug)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;

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

    #[test]
    fn accent_defaults_to_ember_and_round_trips() {
        let mut t = Theme::new();
        assert_eq!(t.accent(), DEFAULT_ACCENT);
        assert_eq!(t.accent_color(), Color::AnsiValue(DEFAULT_ACCENT));
        t.set_accent(75);
        assert_eq!(t.accent(), 75);
        assert_eq!(t.accent_color(), Color::AnsiValue(75));
    }

    #[test]
    fn slug_zero_falls_back_to_accent() {
        let mut t = Theme::new();
        t.set_accent(75);
        assert_eq!(t.slug(), 0);
        assert_eq!(t.slug_color(), Color::AnsiValue(75));
        t.set_slug(108);
        assert_eq!(t.slug_color(), Color::AnsiValue(108));
    }
}
