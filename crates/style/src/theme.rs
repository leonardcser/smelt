//! Theme registry: nvim-style highlight groups interned to stable [`HlGroup`] ids.
//! Unknown names resolve to `Style::default()` without panicking.

use crate::style::{Color, Style};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// Interned highlight-group id. Stable for the process lifetime.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HlGroup(pub u32);

/// Process-global name → id interner. Decoupled from `Theme` so ids stay stable across
/// theme switches and multiple `Theme` instances.
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

/// Get-or-mint the [`HlGroup`] id for `name`.
pub fn intern(name: &str) -> HlGroup {
    if let Some(id) = registry().read().unwrap().name_to_id.get(name).copied() {
        return id;
    }
    registry().write().unwrap().intern(name)
}

/// Reverse the interner: id → name.
pub fn name_of(g: HlGroup) -> Option<String> {
    registry()
        .read()
        .unwrap()
        .id_to_name
        .get(g.0 as usize)
        .cloned()
}

/// Intern a `Style` as an anonymous group keyed by content hash.
/// Anonymous groups bypass theme switches; use [`intern`] with a stable name for theme-reactive styling.
pub fn intern_anonymous_style(style: Style) -> HlGroup {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    style.hash(&mut h);
    let key = format!("__anon__/{:016x}", h.finish());
    let id = intern(&key);
    // Stash the style so resolve() works without a Theme::set() call.
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

/// Default accent palette index (`Color::AnsiValue(208)`, the "ember" preset).
pub const DEFAULT_ACCENT: u8 = 208;

#[derive(Debug, Clone)]
pub struct Theme {
    styles: HashMap<HlGroup, Style>,
    /// Source → target links, resolved at `resolve()` time. Max chain depth 16 (cycle guard).
    links: HashMap<HlGroup, HlGroup>,
    is_light: bool,
    /// ANSI 256-color accent index. Tracked separately from `SmeltAccent` so palette rebuilds
    /// are a single setter call.
    accent: u8,
    /// Slug pill background index. `0` means use accent.
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

    /// Resolve a name to its current Style, following links. Unknown names return `Style::default()`.
    pub fn get(&self, name: &str) -> Style {
        self.resolve(intern(name))
    }

    /// Resolve a [`HlGroup`] to its current Style. Follows up to 16 link hops; cycles fall back to default.
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

    /// Get-or-mint the HlGroup id for `name`.
    pub fn id_for(&self, name: &str) -> HlGroup {
        intern(name)
    }

    /// Returns true if this Theme has a Style or link registered for `hl`.
    /// False means `resolve` will fall back to `Style::default()`.
    pub fn contains(&self, hl: HlGroup) -> bool {
        self.styles.contains_key(&hl) || self.links.contains_key(&hl)
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
        t.set("Visual", Style::new().bg(Color::AnsiValue(237)));
        t.link("SearchHighlight", "Visual");
        assert_eq!(t.get("SearchHighlight"), t.get("Visual"));
    }

    #[test]
    fn link_chain_resolves() {
        let mut t = Theme::new();
        t.set("Base", Style::new().bg(Color::AnsiValue(42)));
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
        t.set("Visual", Style::new().bg(Color::AnsiValue(237)));
        t.link("Search", "Visual");
        let direct = Style::new().bg(Color::AnsiValue(220));
        t.set("Search", direct);
        assert_eq!(t.get("Search"), direct);
    }

    #[test]
    fn link_overwrites_existing_set() {
        let mut t = Theme::new();
        t.set("X", Style::new().bg(Color::AnsiValue(1)));
        t.set("Y", Style::new().bg(Color::AnsiValue(2)));
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
