//! Theme pipeline: terminal background detection, declarative `ThemeSpec`
//! (consumed from Lua), and the `compile` step that turns a spec into the
//! flat `Theme` the renderer reads.
//!
//! Architecture
//! ------------
//! - A colorscheme lives in Lua as a `ThemeSpec` table: a flat map
//!   keyed by highlight-group name. Diff backgrounds, scrollbar colors,
//!   mode indicators — they're all just groups.
//! - `ThemeSpec`, `StyleDecl`, and `ColorDecl` round-trip through mlua
//!   with full IDE completion. `ColorDecl` is recursive (its `dark` /
//!   `light` branches are themselves `ColorDecl`s) so the `FromLua` and
//!   `LuaType` impls are hand-written; the leaf types use the derive.
//! - `compile(spec, is_light) -> Theme` resolves string-valued group
//!   entries (e.g. `Comment = "SmeltMuted"`) at compile time, applies
//!   the `is_light` branch to any `{ dark, light }` `ColorDecl`, and
//!   produces a flat `HlGroup → Style` map. There is no runtime alias
//!   table — references are resolved once.
//! - `default_baked()` returns a process-wide fallback theme built from
//!   an embedded spec, used by offline render paths (`format.rs`,
//!   `prompt_buf.rs`) that don't have access to the live app theme.
//!   A `default_lua_matches_baked_spec` test loads
//!   `runtime/lua/smelt/colorschemes/default.lua` in a bare Lua VM and
//!   compares it group-by-group against `baked_default_spec`, so the
//!   two sources of truth can't drift silently.

use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use smelt_core::lua::doc::record_class;
use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField, LuaOpts, LuaType, LuaTypeTuple};
use smelt_core::style::{Color, Style};
use smelt_core::theme::Theme;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

// ---------------------------------------------------------------------------
// Spec types (consumed by Lua via #[derive(LuaOpts)])
// ---------------------------------------------------------------------------

/// Single group entry in a `ThemeSpec`. Either a concrete `StyleDecl`
/// (`{ fg = ..., bg = ..., bold = true }`) or a string referencing
/// another group in the same spec (`"SmeltMuted"`). String references
/// are resolved at compile time; the runtime `Theme` only contains
/// concrete styles.
#[derive(Debug, Clone)]
pub enum GroupDecl {
    Style(StyleDecl),
    Ref(String),
}

impl LuaType for GroupDecl {
    fn lua_type() -> String {
        let _ = StyleDecl::lua_type();
        "string | smelt.theme.StyleDecl".to_string()
    }
}

impl LuaTypeTuple for GroupDecl {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!("{}: {}", name, Self::lua_type())
    }
}

impl FromLua for GroupDecl {
    fn from_lua(value: LuaValue, lua: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::String(ref s) => Ok(GroupDecl::Ref(s.to_str()?.to_string())),
            LuaValue::Table(_) => {
                let decl = StyleDecl::from_lua(value, lua)?;
                Ok(GroupDecl::Style(decl))
            }
            other => Err(LuaError::FromLuaConversionError {
                from: other.type_name(),
                to: "smelt.theme.GroupDecl".into(),
                message: Some(
                    "expected a style table or a string referencing another group".into(),
                ),
            }),
        }
    }
}

/// Style table for a single highlight group. Every field is optional —
/// unset fields stay at `Style::default()`. Pass a string in place of
/// this struct (at the group-map level) to alias another group.
#[derive(Debug, Default, Clone, LuaOpts)]
#[lua(name = "smelt.theme.StyleDecl")]
pub struct StyleDecl {
    /// Foreground color.
    pub fg: Option<ColorDecl>,
    /// Background color.
    pub bg: Option<ColorDecl>,
    /// Bold text.
    pub bold: Option<bool>,
    /// Italic text.
    pub italic: Option<bool>,
    /// Dim / faint text.
    pub dim: Option<bool>,
    /// Underline.
    pub underline: Option<bool>,
    /// Strikethrough.
    pub crossedout: Option<bool>,
}

impl StyleDecl {
    /// Resolve to a concrete `Style`, applying `is_light` to any
    /// `{ dark, light }` color branch.
    fn to_style(&self, is_light: bool) -> Style {
        Style {
            fg: self.fg.as_ref().and_then(|c| c.to_color(is_light)),
            bg: self.bg.as_ref().and_then(|c| c.to_color(is_light)),
            bold: self.bold.unwrap_or(false),
            italic: self.italic.unwrap_or(false),
            dim: self.dim.unwrap_or(false),
            underline: self.underline.unwrap_or(false),
            crossedout: self.crossedout.unwrap_or(false),
        }
    }
}

/// Color value. A direct color (`{ ansi = N }` / `{ rgb = { R, G, B } }`)
/// or a `{ dark, light }` branch resolved at compile time against the
/// terminal background. The branch entries are themselves `ColorDecl`s,
/// so a light/dark side can carry any other shape; nested branches are
/// allowed but pointless (the resolver re-reads `is_light` at each
/// level, so an inner branch resolves the same way as an outer one).
/// If both a direct color and a matching-side branch are set, the
/// branch wins.
#[derive(Debug, Default, Clone)]
pub struct ColorDecl {
    /// ANSI 256-color palette index for the default (non-branched) case.
    pub ansi: Option<u8>,
    /// `[r, g, b]` sRGB triple for the default (non-branched) case.
    pub rgb: Option<[u8; 3]>,
    /// Color this branch resolves to when `is_light == true`.
    pub light: Option<Box<ColorDecl>>,
    /// Color this branch resolves to when `is_light == false`.
    pub dark: Option<Box<ColorDecl>>,
}

impl ColorDecl {
    const DOC: &'static str = "Color value. Set `ansi` (256-color palette \
index) or `rgb` (`{R, G, B}` triple) for a direct color, or `dark` / `light` \
(themselves `ColorDecl`s) for a branch that resolves against the terminal \
background. A matching-side branch wins over the direct fields.";

    pub fn to_color(&self, is_light: bool) -> Option<Color> {
        if is_light {
            if let Some(c) = self.light.as_ref().and_then(|c| c.to_color(is_light)) {
                return Some(c);
            }
        } else if let Some(c) = self.dark.as_ref().and_then(|c| c.to_color(is_light)) {
            return Some(c);
        }
        if let Some(v) = self.ansi {
            return Some(Color::AnsiValue(v));
        }
        if let Some([r, g, b]) = self.rgb {
            return Some(Color::Rgb { r, g, b });
        }
        None
    }
}

// Hand-written LuaOpts/FromLua impls because `Box<ColorDecl>` doesn't
// satisfy the `LuaType` / `FromLua` trait bounds the derive emits.
// (mlua's `FromLua` isn't implemented for `Box<T>`, and adding a
// blanket impl would touch the shared `lua_type` module just for this
// recursive case.) Behavior matches what `#[derive(LuaOpts)]` would
// produce for the equivalent non-recursive struct.

impl LuaType for ColorDecl {
    fn lua_type() -> String {
        record_class(LuaClassDecl {
            name: "smelt.theme.ColorDecl",
            doc: Self::DOC,
            fields: vec![
                LuaClassField {
                    name: "ansi",
                    ty: "integer".into(),
                    optional: true,
                    doc: "ANSI 256-color palette index for the default (non-branched) case.",
                },
                LuaClassField {
                    name: "rgb",
                    ty: "integer[]".into(),
                    optional: true,
                    doc: "`[r, g, b]` sRGB triple for the default (non-branched) case.",
                },
                LuaClassField {
                    name: "light",
                    ty: "smelt.theme.ColorDecl".into(),
                    optional: true,
                    doc: "Color this branch resolves to when `is_light == true`.",
                },
                LuaClassField {
                    name: "dark",
                    ty: "smelt.theme.ColorDecl".into(),
                    optional: true,
                    doc: "Color this branch resolves to when `is_light == false`.",
                },
            ],
        });
        "smelt.theme.ColorDecl".to_string()
    }
}

impl LuaTypeTuple for ColorDecl {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!("{}: {}", name, Self::lua_type())
    }
}

impl LuaOpts for ColorDecl {
    fn lua_class_decl() -> LuaClassDecl {
        // Returned for the `LuaOpts` trait contract; the field list is
        // emitted by the `LuaType` impl above when the registry first
        // sees the class.
        LuaClassDecl {
            name: "smelt.theme.ColorDecl",
            doc: Self::DOC,
            fields: vec![],
        }
    }
}

impl FromLua for ColorDecl {
    // `lua` is threaded through recursive calls for the trait signature;
    // clippy can't see past the recursion to know it's actually used.
    #[allow(clippy::only_used_in_recursion)]
    fn from_lua(value: LuaValue, lua: &Lua) -> LuaResult<Self> {
        let t = match value {
            LuaValue::Table(t) => t,
            other => {
                return Err(LuaError::FromLuaConversionError {
                    from: other.type_name(),
                    to: "smelt.theme.ColorDecl".into(),
                    message: Some("expected a table".into()),
                });
            }
        };
        let ansi = t.get::<Option<u8>>("ansi")?;
        let rgb = t.get::<Option<[u8; 3]>>("rgb")?;
        // Recurse via `LuaValue` round-trip so `Box<ColorDecl>` doesn't
        // need a `FromLua` impl of its own.
        let light = match t.get::<Option<LuaValue>>("light")? {
            Some(v) => Some(Box::new(ColorDecl::from_lua(v, lua)?)),
            None => None,
        };
        let dark = match t.get::<Option<LuaValue>>("dark")? {
            Some(v) => Some(Box::new(ColorDecl::from_lua(v, lua)?)),
            None => None,
        };
        Ok(Self {
            ansi,
            rgb,
            light,
            dark,
        })
    }
}

/// Full colorscheme description: a flat map keyed by highlight-group
/// name (nvim conventions: `Comment`, `Visual`, `SmeltAccent`, …) with
/// either a `StyleDecl` table or a string referencing another group
/// in the same spec as the value. Compile via [`compile`] to produce
/// a runtime `Theme`. Every themable color lives here — there are no
/// special-case fields. The diff renderer's row fills are just
/// `SmeltDiffAddBg` / `SmeltDiffDelBg` groups, scrollbar colors are
/// `SmeltScrollbarTrack` / `…Thumb`, and so on.
#[derive(Debug, Default, Clone)]
pub struct ThemeSpec {
    pub groups: HashMap<String, GroupDecl>,
}

impl ThemeSpec {
    const DOC: &'static str = "Flat map keyed by highlight-group name \
(`Comment`, `Visual`, `SmeltAccent`, …). Each value is either a \
`StyleDecl` table or a string referencing another group in the same \
spec. Every themable color (foreground, background, diff row fills, \
scrollbar colors, mode indicators) is just a group.";
}

impl LuaType for ThemeSpec {
    fn lua_type() -> String {
        let _ = StyleDecl::lua_type();
        let _ = ColorDecl::lua_type();
        record_class(LuaClassDecl {
            name: "smelt.theme.ThemeSpec",
            doc: Self::DOC,
            fields: vec![LuaClassField {
                // `[string]` is the LuaCATS index-signature key — emitted as
                // `---@field [string] V` so any group name typechecks.
                name: "[string]",
                ty: "string | smelt.theme.StyleDecl".into(),
                optional: true,
                doc: "Style table or alias string for the group named by the key.",
            }],
        });
        "smelt.theme.ThemeSpec".to_string()
    }
}

impl LuaTypeTuple for ThemeSpec {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!("{}: {}", name, Self::lua_type())
    }
}

impl LuaOpts for ThemeSpec {
    fn lua_class_decl() -> LuaClassDecl {
        LuaClassDecl {
            name: "smelt.theme.ThemeSpec",
            doc: Self::DOC,
            fields: vec![],
        }
    }
}

impl FromLua for ThemeSpec {
    fn from_lua(value: LuaValue, lua: &Lua) -> LuaResult<Self> {
        let t = match value {
            LuaValue::Table(t) => t,
            other => {
                return Err(LuaError::FromLuaConversionError {
                    from: other.type_name(),
                    to: "smelt.theme.ThemeSpec".into(),
                    message: Some("expected a table".into()),
                });
            }
        };
        let mut groups = HashMap::new();
        for pair in t.pairs::<String, LuaValue>() {
            let (name, v) = pair?;
            groups.insert(name, GroupDecl::from_lua(v, lua)?);
        }
        Ok(Self { groups })
    }
}

// ---------------------------------------------------------------------------
// Compile
// ---------------------------------------------------------------------------

/// Compile a `ThemeSpec` into a runtime `Theme`, resolving string-valued
/// group entries to the referenced group's style. Cycles and dangling
/// references are reported as `Err`.
pub fn compile(spec: &ThemeSpec, is_light: bool) -> Result<Theme, String> {
    let mut theme = Theme::new();
    theme.set_light(is_light);

    let mut resolving: Vec<String> = Vec::new();
    let mut resolved: HashMap<String, Style> = HashMap::new();
    for name in spec.groups.keys() {
        resolve_group(name, spec, is_light, &mut resolving, &mut resolved)?;
    }
    for (name, style) in resolved {
        theme.set(name, style);
    }

    Ok(theme)
}

fn resolve_group(
    name: &str,
    spec: &ThemeSpec,
    is_light: bool,
    resolving: &mut Vec<String>,
    resolved: &mut HashMap<String, Style>,
) -> Result<Style, String> {
    if let Some(s) = resolved.get(name) {
        return Ok(*s);
    }
    if resolving.iter().any(|n| n == name) {
        resolving.push(name.to_string());
        return Err(format!(
            "theme: cyclic reference: {} (chain: {})",
            name,
            resolving.join(" → ")
        ));
    }
    let decl = spec.groups.get(name).ok_or_else(|| {
        let chain = resolving.join(" → ");
        if chain.is_empty() {
            format!("theme: unknown group: {name}")
        } else {
            format!("theme: unknown group: {name} (referenced from {chain})")
        }
    })?;
    resolving.push(name.to_string());
    let style = match decl {
        GroupDecl::Style(sd) => sd.to_style(is_light),
        GroupDecl::Ref(target) => resolve_group(target, spec, is_light, resolving, resolved)?,
    };
    resolving.pop();
    resolved.insert(name.to_string(), style);
    Ok(style)
}

// ---------------------------------------------------------------------------
// Light / dark terminal detection
// ---------------------------------------------------------------------------

/// Probe terminal background and set the light flag on `theme`.
/// Must be called before entering raw mode / alternate screen.
pub(crate) fn detect_background(theme: &mut Theme) {
    if let Some(light) = detect_light_background() {
        theme.set_light(light);
    }
}

fn detect_light_background() -> Option<bool> {
    if let Some(luma) = osc_background_luma() {
        return Some(luma > 0.6);
    }
    colorfgbg_is_light()
}

/// Parse `$COLORFGBG` (`"fg;bg"` or `"fg;default;bg"`).
fn colorfgbg_is_light() -> Option<bool> {
    let val = std::env::var("COLORFGBG").ok()?;
    let parts: Vec<&str> = val.split(';').collect();
    let bg = match parts.len() {
        2 => parts[1],
        3 => parts[2],
        _ => return None,
    };
    let code: u8 = bg.parse().ok()?;
    Some(matches!(code, 7 | 9..=15)) // ANSI: 0-6,8 dark; 7,9-15 light
}

/// OSC 11 query: returns luma of the terminal background (0.0 = black, 1.0 = white).
#[cfg(unix)]
fn osc_background_luma() -> Option<f32> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    // Don't query if TERM=dumb.
    if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        return None;
    }

    let switch_raw = !is_raw_mode_enabled().unwrap_or(false);
    if switch_raw {
        enable_raw_mode().ok()?;
    }

    let result = (|| -> Option<f32> {
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "\x1b]11;?\x07\x1b[5n").ok()?; // OSC 11 + DSR fence
        stdout.flush().ok()?;

        let mut tty = File::open("/dev/tty").ok()?;
        let mut buf = [0u8; 100];
        let mut written = 0;

        while written < buf.len() {
            if !wait_for_input(tty.as_raw_fd(), 100) {
                break;
            }
            let n = tty.read(&mut buf[written..]).ok()?;
            if n == 0 {
                break;
            }
            written += n;
            if buf[..written].contains(&b'n') {
                break;
            }
        }

        let response = std::str::from_utf8(&buf[..written]).ok()?;
        parse_osc11_response(response)
    })();

    if switch_raw {
        let _ = disable_raw_mode();
    }

    result
}

#[cfg(not(unix))]
fn osc_background_luma() -> Option<f32> {
    None
}

/// Parse `\x1b]11;rgb:RRRR/GGGG/BBBB\x1b\\` and return luma.
fn parse_osc11_response(response: &str) -> Option<f32> {
    let rgb_start = response.find("rgb:")?;
    let raw = &response[rgb_start + 4..];
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    let r = u8::from_str_radix(parts[0].get(..2)?, 16).ok()?;
    let g = u8::from_str_radix(parts[1].get(..2)?, 16).ok()?;
    let blue_str: String = parts[2]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    let b = u8::from_str_radix(blue_str.get(..2)?, 16).ok()?;

    Some((0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0) // sRGB luma
}

/// Returns `true` if the fd is readable within `timeout_ms`.
#[cfg(target_os = "macos")]
fn wait_for_input(fd: std::os::fd::RawFd, timeout_ms: u64) -> bool {
    unsafe {
        let mut read_fds: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut read_fds);
        let mut tv = libc::timeval {
            tv_sec: (timeout_ms / 1000) as libc::time_t,
            tv_usec: ((timeout_ms % 1000) * 1000) as libc::suseconds_t,
        };
        libc::select(
            fd + 1,
            &mut read_fds,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        ) > 0
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn wait_for_input(fd: std::os::fd::RawFd, timeout_ms: u64) -> bool {
    unsafe {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        libc::poll(&mut pollfd, 1, timeout_ms as libc::c_int) > 0
    }
}

// ---------------------------------------------------------------------------
// Default baked theme
// ---------------------------------------------------------------------------

/// Process-wide fallback theme, built once from the baked default spec
/// and reused by offline render paths that don't have access to the
/// live app theme (e.g. `format.rs`, transcript parser unit tests).
/// First call also publishes the theme as the process-wide active
/// theme, so the diff renderer has working colors before the TUI app
/// starts. The `is_light` flag defaults to dark — light-mode callers
/// should clone and flip.
pub fn default_baked() -> &'static Arc<Theme> {
    static T: OnceLock<Arc<Theme>> = OnceLock::new();
    T.get_or_init(|| {
        let spec = baked_default_spec();
        let theme = Arc::new(compile(&spec, false).expect("baked default spec must compile"));
        smelt_core::theme::set_active(theme.clone());
        theme
    })
}

/// The same spec that `runtime/lua/smelt/colorschemes/default.lua`
/// describes, hard-coded here so the binary has a working theme before
/// the Lua runtime initializes. Keep the two in sync — the Lua spec is
/// the source of truth at runtime; this is the paint-before-bootstrap
/// fallback.
fn baked_default_spec() -> ThemeSpec {
    fn ansi(v: u8) -> ColorDecl {
        ColorDecl {
            ansi: Some(v),
            ..Default::default()
        }
    }
    fn dl(dark: u8, light: u8) -> ColorDecl {
        ColorDecl {
            dark: Some(Box::new(ansi(dark))),
            light: Some(Box::new(ansi(light))),
            ..Default::default()
        }
    }
    fn rgb(r: u8, g: u8, b: u8) -> ColorDecl {
        ColorDecl {
            rgb: Some([r, g, b]),
            ..Default::default()
        }
    }
    fn fg(c: ColorDecl) -> GroupDecl {
        GroupDecl::Style(StyleDecl {
            fg: Some(c),
            ..Default::default()
        })
    }
    fn bg(c: ColorDecl) -> GroupDecl {
        GroupDecl::Style(StyleDecl {
            bg: Some(c),
            ..Default::default()
        })
    }
    fn dim() -> GroupDecl {
        GroupDecl::Style(StyleDecl {
            dim: Some(true),
            ..Default::default()
        })
    }
    fn aliased(target: &str) -> GroupDecl {
        GroupDecl::Ref(target.to_string())
    }

    let mut groups: HashMap<String, GroupDecl> = HashMap::new();

    // ── Base colors: the only groups that hold literal values. ──────────
    groups.insert("SmeltAccent".into(), fg(ansi(208))); // ember
    groups.insert("SmeltSlug".into(), aliased("SmeltAccent"));
    groups.insert("SmeltMuted".into(), fg(ansi(244)));
    groups.insert("SmeltSuccess".into(), fg(ansi(77)));
    groups.insert("SmeltHeading".into(), fg(ansi(117)));

    // Background fills. `dl(dark, light)` carries the per-mode branch.
    groups.insert("SmeltStatusBg".into(), bg(ansi(233)));
    groups.insert("SmeltUserBg".into(), bg(dl(236, 254)));
    groups.insert("SmeltScrollPillBg".into(), bg(dl(236, 254)));
    groups.insert("SmeltCodeBlockBg".into(), bg(dl(233, 255)));
    groups.insert("SmeltBar".into(), bg(dl(237, 252)));
    groups.insert("SmeltSelection".into(), bg(dl(238, 189)));
    groups.insert("SmeltCursorLineBg".into(), bg(dl(237, 253)));
    groups.insert("SmeltScrollbarTrack".into(), bg(dl(235, 254)));
    groups.insert("SmeltScrollbarThumb".into(), bg(dl(243, 247)));

    // Tool / reasoning state colors. `8` = `DarkGrey` in 256-color.
    groups.insert("SmeltToolPending".into(), fg(dl(8, 250)));
    groups.insert("SmeltReasonOff".into(), fg(dl(8, 250)));
    groups.insert("SmeltReasonLow".into(), fg(ansi(75)));
    groups.insert("SmeltReasonMed".into(), fg(ansi(214)));
    groups.insert("SmeltReasonHigh".into(), fg(ansi(203)));
    groups.insert("SmeltReasonMax".into(), fg(ansi(196)));

    // Mode indicator colors.
    groups.insert("SmeltModePlan".into(), fg(ansi(79)));
    groups.insert("SmeltModeApply".into(), fg(ansi(141)));
    groups.insert("SmeltModeYolo".into(), fg(ansi(204)));
    groups.insert(
        "SmeltModeExec".into(),
        GroupDecl::Style(StyleDecl {
            fg: Some(ansi(197)),
            bold: Some(true),
            ..Default::default()
        }),
    );

    // ── Semantic / nvim-standard names: links into the base set. ────────
    groups.insert("Comment".into(), aliased("SmeltMuted"));
    groups.insert("Visual".into(), aliased("SmeltSelection"));
    groups.insert("CursorLine".into(), aliased("SmeltCursorLineBg"));
    groups.insert(
        "ErrorMsg".into(),
        GroupDecl::Style(StyleDecl {
            fg: Some(ansi(9)), // bright red
            ..Default::default()
        }),
    );
    groups.insert("GhostText".into(), dim());

    // Diff renderer row fills. Themes can override these like any
    // other group.
    groups.insert("SmeltDiffAddBg".into(), bg(rgb(20, 50, 20)));
    groups.insert("SmeltDiffDelBg".into(), bg(rgb(60, 20, 20)));

    ThemeSpec { groups }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_osc11_dark_background() {
        let resp = "\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma < 0.2, "luma {luma} should indicate dark");
    }

    #[test]
    fn parse_osc11_light_background() {
        let resp = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma > 0.9, "luma {luma} should indicate light");
    }

    #[test]
    fn parse_osc11_mid_tone() {
        let resp = "\x1b]11;rgb:8080/8080/8080\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(
            (0.4..0.6).contains(&luma),
            "luma {luma} should be mid-range"
        );
    }

    #[test]
    fn parse_osc11_short_hex() {
        let resp = "\x1b]11;rgb:ff/ff/ff\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma > 0.9);
    }

    #[test]
    fn parse_osc11_garbage() {
        assert!(parse_osc11_response("garbage").is_none());
        assert!(parse_osc11_response("").is_none());
    }

    #[test]
    fn baked_default_compiles_and_resolves_aliases() {
        let theme = default_baked();
        // Comment is aliased to SmeltMuted → fg = AnsiValue(244).
        assert_eq!(theme.get("Comment").fg, Some(Color::AnsiValue(244)));
        assert_eq!(theme.get("SmeltMuted").fg, Some(Color::AnsiValue(244)));
        // SmeltSlug is aliased to SmeltAccent → fg = AnsiValue(208).
        assert_eq!(theme.get("SmeltSlug").fg, Some(Color::AnsiValue(208)));
        assert_eq!(theme.get("SmeltAccent").fg, Some(Color::AnsiValue(208)));
    }

    #[test]
    fn baked_default_light_branch() {
        let spec = baked_default_spec();
        let dark = compile(&spec, false).unwrap();
        let light = compile(&spec, true).unwrap();
        assert_eq!(dark.get("SmeltUserBg").bg, Some(Color::AnsiValue(236)));
        assert_eq!(light.get("SmeltUserBg").bg, Some(Color::AnsiValue(254)));
    }

    #[test]
    fn compile_reports_dangling_reference() {
        let mut groups = HashMap::new();
        groups.insert("Anchor".into(), GroupDecl::Ref("Missing".into()));
        let spec = ThemeSpec { groups };
        let err = compile(&spec, false).unwrap_err();
        assert!(err.contains("Missing"), "got: {err}");
    }

    #[test]
    fn compile_reports_cycle() {
        let mut groups = HashMap::new();
        groups.insert("A".into(), GroupDecl::Ref("B".into()));
        groups.insert("B".into(), GroupDecl::Ref("A".into()));
        let spec = ThemeSpec { groups };
        let err = compile(&spec, false).unwrap_err();
        assert!(err.contains("cyclic"), "got: {err}");
    }

    #[test]
    fn compile_resolves_chain() {
        let mut groups = HashMap::new();
        groups.insert(
            "Base".into(),
            GroupDecl::Style(StyleDecl {
                fg: Some(ColorDecl {
                    ansi: Some(42),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
        groups.insert("Mid".into(), GroupDecl::Ref("Base".into()));
        groups.insert("Top".into(), GroupDecl::Ref("Mid".into()));
        let spec = ThemeSpec { groups };
        let theme = compile(&spec, false).unwrap();
        assert_eq!(theme.get("Top").fg, Some(Color::AnsiValue(42)));
        assert_eq!(theme.get("Mid").fg, Some(Color::AnsiValue(42)));
    }

    #[test]
    fn diff_colors_compile_into_smelt_diff_groups() {
        let spec = baked_default_spec();
        let theme = compile(&spec, false).unwrap();
        assert_eq!(
            theme.get("SmeltDiffAddBg").bg,
            Some(Color::Rgb {
                r: 20,
                g: 50,
                b: 20
            })
        );
        assert_eq!(
            theme.get("SmeltDiffDelBg").bg,
            Some(Color::Rgb {
                r: 60,
                g: 20,
                b: 20
            })
        );
    }

    /// Drift guard: load `runtime/lua/smelt/colorschemes/default.lua` in
    /// a bare Lua VM, decode the returned table as a `ThemeSpec`, and
    /// compare its compiled output group-by-group against the in-Rust
    /// `baked_default_spec`. The Lua spec is the source of truth at
    /// runtime; the baked spec is the paint-before-bootstrap fallback.
    /// They must stay in lockstep — this test catches the moment they
    /// don't.
    #[test]
    fn default_lua_matches_baked_spec() {
        const DEFAULT_LUA: &str =
            include_str!("../../../runtime/lua/smelt/colorschemes/default.lua");
        let lua = mlua::Lua::new();
        let lua_spec_value: LuaValue = lua
            .load(DEFAULT_LUA)
            .set_name("colorschemes/default.lua")
            .eval()
            .expect("default.lua must load");
        let lua_spec: ThemeSpec = ThemeSpec::from_lua(lua_spec_value, &lua)
            .expect("default.lua must decode as ThemeSpec");
        let baked_spec = baked_default_spec();

        for is_light in [false, true] {
            let lua_theme = compile(&lua_spec, is_light).expect("lua spec compiles");
            let baked_theme = compile(&baked_spec, is_light).expect("baked spec compiles");

            // Collect every group name appearing on either side. Both
            // resolved styles must match exactly — same fg/bg/flags.
            let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (id, _) in lua_theme.iter() {
                if let Some(n) = smelt_core::theme::name_of(id) {
                    if !n.starts_with("__anon__/") {
                        names.insert(n);
                    }
                }
            }
            for (id, _) in baked_theme.iter() {
                if let Some(n) = smelt_core::theme::name_of(id) {
                    if !n.starts_with("__anon__/") {
                        names.insert(n);
                    }
                }
            }
            for name in &names {
                let lua_style = lua_theme.get(name);
                let baked_style = baked_theme.get(name);
                assert_eq!(
                    lua_style, baked_style,
                    "group `{name}` drifted between default.lua and baked_default_spec \
                     (is_light={is_light}): lua={lua_style:?} baked={baked_style:?}"
                );
            }
        }
    }
}
