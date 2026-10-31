//! Theme pipeline: terminal background detection, declarative `ThemeSpec`
//! (consumed from Lua), and the `compile` step that turns a spec into the
//! flat `Theme` the renderer reads.
//!
//! Architecture
//! ------------
//! - A colorscheme lives in Lua as a `ThemeSpec` table with optional
//!   `name`, `syntax`, and `light` metadata plus a required `groups` table.
//!   Diff backgrounds, scrollbar colors, mode indicators - they're all just
//!   groups.
//! - `ThemeSpec`, `StyleDecl`, and `ColorDecl` round-trip through mlua
//!   with full IDE completion. `ColorDecl` is recursive (its `dark` /
//!   `light` branches are themselves `ColorDecl`s) so the `FromLua` and
//!   `LuaType` impls are hand-written; the leaf types use the derive.
//! - `compile(spec, is_light) -> Theme` resolves string-valued group
//!   entries (e.g. `SmeltToolPending = "Comment"`) at compile time, applies
//!   the `is_light` branch to any `{ dark, light }` `ColorDecl`, and
//!   produces a flat `HlGroup → Style` map. There is no runtime alias
//!   table - references are resolved once.
//! - `default_baked()` returns a process-wide fallback theme. It evals
//!   `runtime/lua/smelt/colorschemes/default.lua` (embedded via
//!   `include_str!`) in a throwaway `mlua::Lua` and decodes the result
//!   as a `ThemeSpec`. The Lua file is the single source of truth - no
//!   parallel Rust HashMap. Offline render paths (`format.rs`,
//!   `prompt_buf.rs`) that don't have access to the live app theme
//!   reach the default via this entry point.

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
/// another group in the same spec (`"Comment"`). String references
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

/// Style table for a single highlight group. Every field is optional -
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
    /// Reverse video.
    pub reverse: Option<bool>,
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
            reverse: self.reverse.unwrap_or(false),
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
            classification: smelt_core::lua::doc::classification_for_type("smelt.theme.ColorDecl"),
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
            classification: smelt_core::lua::doc::classification_for_type("smelt.theme.ColorDecl"),
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

/// Full colorscheme description. `name`, `syntax`, and `light` are metadata;
/// `groups` is keyed by highlight-group name (nvim conventions: `Comment`,
/// `Visual`, `SmeltAccent`, …) with either a `StyleDecl` table or a string
/// referencing another group in the same spec as the value. Compile via
/// [`compile`] to produce a runtime `Theme`. Every themable color lives in
/// `groups` - there are no special-case group fields. The diff renderer's row
/// and inline fills are `SmeltDiffAddBg` / `SmeltDiffDeleteBg` and
/// `SmeltDiffAddInlineBg` / `SmeltDiffDeleteInlineBg` groups, scrollbar colors are
/// `SmeltScrollbarTrack` / `…Thumb`, and so on.
#[derive(Debug, Default, Clone)]
pub struct ThemeSpec {
    pub name: Option<String>,
    pub groups: HashMap<String, GroupDecl>,
    pub syntax_theme: Option<String>,
    pub is_light: Option<bool>,
}

impl ThemeSpec {
    const DOC: &'static str = "Colorscheme table with optional metadata and a \
required `groups` map. `syntax` selects a bundled two-face syntax theme for \
code highlighting. `light` marks the palette as light or dark; omit it to use \
terminal background detection. Every themable color (foreground, background, \
diff row and inline fills, scrollbar colors, mode indicators) is a group.";
}

impl LuaType for ThemeSpec {
    fn lua_type() -> String {
        let _ = StyleDecl::lua_type();
        let _ = ColorDecl::lua_type();
        record_class(LuaClassDecl {
            name: "smelt.theme.ThemeSpec",
            classification: smelt_core::lua::doc::classification_for_type("smelt.theme.ThemeSpec"),
            doc: Self::DOC,
            fields: vec![
                LuaClassField {
                    name: "name",
                    ty: "string".into(),
                    optional: true,
                                        doc: "Display name for this colorscheme.",
                },
                LuaClassField {
                    name: "syntax",
                    ty: "string".into(),
                    optional: true,
                                        doc: "Bundled syntect/two-face syntax theme name for code highlighting.",
                },
                LuaClassField {
                    name: "light",
                    ty: "boolean".into(),
                    optional: true,
                                        doc: "Whether this colorscheme is light. Omit to use terminal background detection.",
                },
                LuaClassField {
                    name: "groups",
                    ty: "table<string, string | smelt.theme.StyleDecl>".into(),
                    optional: false,
                                        doc: "Highlight groups keyed by group name.",
                },
            ],
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
            classification: smelt_core::lua::doc::classification_for_type("smelt.theme.ThemeSpec"),
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
        let name = t.get::<Option<String>>("name")?;
        let syntax_theme = t.get::<Option<String>>("syntax")?;
        let is_light = t.get::<Option<bool>>("light")?;
        if let Some(name) = syntax_theme.as_deref() {
            if !smelt_core::content::highlight::syntax_theme_name_is_valid(name) {
                return Err(LuaError::FromLuaConversionError {
                    from: "string",
                    to: "smelt.theme.ThemeSpec".into(),
                    message: Some(format!("unknown syntax theme `{name}`")),
                });
            }
        }

        let group_table: LuaTable = t.get("groups")?;
        let mut groups = HashMap::new();
        for pair in group_table.pairs::<String, LuaValue>() {
            let (name, v) = pair?;
            groups.insert(name, GroupDecl::from_lua(v, lua)?);
        }
        Ok(Self {
            name,
            groups,
            syntax_theme,
            is_light,
        })
    }
}

// ---------------------------------------------------------------------------
// Compile
// ---------------------------------------------------------------------------

/// Compile a `ThemeSpec` into a runtime `Theme`, resolving string-valued
/// group entries to the referenced group's style. Cycles and dangling
/// references are reported as `Err`.
pub fn compile(spec: &ThemeSpec, is_light: bool) -> Result<Theme, String> {
    let is_light = spec.is_light.unwrap_or(is_light);
    let mut theme = Theme::new();
    theme.set_light(is_light);
    theme.set_syntax_theme(spec.syntax_theme.clone());

    let mut resolving: Vec<String> = Vec::new();
    let mut resolved: HashMap<String, Style> = HashMap::new();
    for name in spec.groups.keys() {
        resolve_group(name, spec, is_light, &mut resolving, &mut resolved)?;
    }
    for (name, style) in resolved {
        theme.set(name, style);
    }
    install_resize_handle_group(&mut theme);

    Ok(theme)
}

fn install_resize_handle_group(theme: &mut Theme) {
    let group = theme.id_for("SmeltResizeHandle");
    if theme.contains(group) {
        return;
    }
    let target = if theme.is_light() {
        Color::Black
    } else {
        Color::White
    };
    let fg = theme
        .get("SmeltSeparator")
        .fg
        .or_else(|| theme.get("Comment").fg)
        .or_else(|| theme.get("Normal").fg)
        .and_then(|color| color.mix_toward(target, 0.75));
    if let Some(fg) = fg {
        theme.set(
            "SmeltResizeHandle",
            Style {
                fg: Some(fg),
                bold: true,
                ..Style::default()
            },
        );
    }
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

/// Probe the terminal's colors, record them as the process palette, and set
/// the light flag on `theme`. Must be called before entering raw mode /
/// alternate screen.
pub(crate) fn detect_background(theme: &mut Theme) {
    let replies = query_terminal_colors();
    let light = match replies.bg {
        Some(_) => Some(replies.palette(false).is_light()),
        None => colorfgbg_is_light(),
    };
    smelt_core::palette::set_terminal(replies.palette(light.unwrap_or(false)));
    if let Some(light) = light {
        theme.set_light(light);
    }
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

/// Colors the terminal reported. Each is `None` when it did not answer.
#[derive(Debug, Default, PartialEq)]
struct ColorReplies {
    fg: Option<smelt_core::palette::Rgb>,
    bg: Option<smelt_core::palette::Rgb>,
    ansi: [Option<smelt_core::palette::Rgb>; 16],
}

impl ColorReplies {
    /// The reported colors over the xterm palette for a `light` or dark background.
    fn palette(&self, light: bool) -> smelt_core::palette::Palette {
        let mut palette = smelt_core::palette::Palette::xterm(light);
        palette.fg = self.fg.unwrap_or(palette.fg);
        palette.bg = self.bg.unwrap_or(palette.bg);
        for (slot, reply) in palette.ansi.iter_mut().zip(self.ansi) {
            *slot = reply.unwrap_or(*slot);
        }
        palette
    }
}

/// Ask for the default foreground (OSC 10), background (OSC 11) and the 16
/// ANSI colors (OSC 4) in one round trip, fenced by a status report so
/// terminals that ignore some queries still end the wait.
#[cfg(unix)]
fn query_terminal_colors() -> ColorReplies {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        return ColorReplies::default();
    }

    let switch_raw = !is_raw_mode_enabled().unwrap_or(false);
    if switch_raw && enable_raw_mode().is_err() {
        return ColorReplies::default();
    }

    let response = (|| -> Option<Vec<u8>> {
        let mut query = String::from("\x1b]10;?\x07\x1b]11;?\x07");
        for index in 0..16 {
            query.push_str(&format!("\x1b]4;{index};?\x07"));
        }
        query.push_str("\x1b[5n");
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(query.as_bytes()).ok()?;
        stdout.flush().ok()?;

        let mut tty = File::open("/dev/tty").ok()?;
        let mut buf = vec![0u8; 2048];
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
            if ends_with_status_report(&buf[..written]) {
                break;
            }
        }
        buf.truncate(written);
        Some(buf)
    })();

    if switch_raw {
        let _ = disable_raw_mode();
    }

    response
        .map(|bytes| parse_color_replies(&String::from_utf8_lossy(&bytes)))
        .unwrap_or_default()
}

#[cfg(not(unix))]
fn query_terminal_colors() -> ColorReplies {
    ColorReplies::default()
}

/// Whether `bytes` ends with the device status report (`ESC [ <n> n`) that fences the queries.
#[cfg(any(unix, test))]
fn ends_with_status_report(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_suffix(b"n") else {
        return false;
    };
    let digits = rest.iter().rev().take_while(|b| b.is_ascii_digit()).count();
    digits > 0 && rest[..rest.len() - digits].ends_with(b"\x1b[")
}

/// Parse OSC 10, 11 and 4 color replies such as `ESC ] 4;1;rgb:cd/00/00 ST`.
#[cfg(any(unix, test))]
fn parse_color_replies(response: &str) -> ColorReplies {
    let mut replies = ColorReplies::default();
    for reply in response.split("\x1b]").skip(1) {
        let reply = reply.trim_end_matches(['\x07', '\x1b', '\\']);
        let mut rest = reply.split(';');
        let Some(code) = rest.next() else {
            continue;
        };
        match code {
            "10" => replies.fg = rest.next().and_then(parse_xparse_color),
            "11" => replies.bg = rest.next().and_then(parse_xparse_color),
            "4" => {
                let index = rest.next().and_then(|index| index.parse::<usize>().ok());
                if let (Some(slot), Some(color)) = (
                    index.and_then(|index| replies.ansi.get_mut(index)),
                    rest.next().and_then(parse_xparse_color),
                ) {
                    *slot = Some(color);
                }
            }
            _ => {}
        }
    }
    replies
}

/// Parse an X11 `rgb:R/G/B` color with 1 to 4 hex digits per channel.
#[cfg(any(unix, test))]
fn parse_xparse_color(text: &str) -> Option<smelt_core::palette::Rgb> {
    let text = text.split(['\x07', '\x1b']).next()?;
    let mut channels = text.strip_prefix("rgb:")?.split('/').map(|channel| {
        let digits = channel.len();
        if !(1..=4).contains(&digits) {
            return None;
        }
        let value = u32::from_str_radix(channel, 16).ok()?;
        let max = (1u32 << (4 * digits)) - 1;
        Some(((value * 255 + max / 2) / max) as u8)
    });
    let rgb = (channels.next()??, channels.next()??, channels.next()??);
    channels.next().is_none().then_some(rgb)
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
/// The fallback is immutable and dark; light-mode callers use
/// `default_baked_with_background`.
pub fn default_baked() -> &'static Arc<Theme> {
    static T: OnceLock<Arc<Theme>> = OnceLock::new();
    T.get_or_init(|| {
        let spec = baked_default_spec();
        Arc::new(compile(&spec, false).expect("baked default spec must compile"))
    })
}

pub fn default_baked_with_background(is_light: bool) -> Theme {
    let spec = baked_default_spec();
    compile(&spec, is_light).expect("baked default spec must compile")
}

/// Decode `runtime/lua/smelt/colorschemes/default.lua` (embedded at
/// compile time) into a `ThemeSpec`. The Lua file is the single source
/// of truth for the default colorscheme - `default_baked()` calls this
/// once and caches the compiled `Theme`. Panics on decode failure so a
/// malformed default.lua surfaces immediately at first paint.
fn baked_default_spec() -> ThemeSpec {
    const DEFAULT_LUA: &str = include_str!("../../../runtime/lua/smelt/colorschemes/default.lua");
    let lua = mlua::Lua::new();
    let value: mlua::Value = lua
        .load(DEFAULT_LUA)
        .set_name("colorschemes/default.lua")
        .eval()
        .expect("embedded default.lua must load");
    ThemeSpec::from_lua(value, &lua).expect("embedded default.lua must decode as ThemeSpec")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn canonical_group_names(lua: &Lua) -> BTreeSet<String> {
        const GROUPS_LUA: &str =
            include_str!("../../../runtime/lua/smelt/colorschemes/_groups.lua");
        let entries: LuaTable = lua
            .load(GROUPS_LUA)
            .set_name("colorschemes/_groups.lua")
            .eval()
            .expect("highlight group manifest must load");
        let mut names = BTreeSet::new();
        for entry in entries.sequence_values::<LuaTable>() {
            let entry = entry.expect("manifest entries must be tables");
            let name: String = entry.get("name").expect("manifest group name");
            let role: String = entry.get("role").expect("manifest group role");
            let owner: String = entry.get("owner").expect("manifest group owner");
            let description: String = entry
                .get("description")
                .expect("manifest group description");
            assert!(
                matches!(role.as_str(), "foreground" | "background" | "style"),
                "invalid role `{role}` for `{name}`"
            );
            assert!(!owner.is_empty(), "missing owner for `{name}`");
            assert!(!description.is_empty(), "missing description for `{name}`");
            assert!(names.insert(name.clone()), "duplicate group `{name}`");
        }
        names
    }

    fn spec_group_names(spec: &ThemeSpec) -> BTreeSet<String> {
        spec.groups.keys().cloned().collect()
    }

    fn production_group_usage() -> String {
        fn visit(dir: &Path, sources: &mut String) {
            for entry in std::fs::read_dir(dir).expect("read source directory") {
                let path = entry.expect("read source entry").path();
                if path.is_dir() {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("");
                    if matches!(
                        name,
                        "colorschemes" | "examples" | "harness_tests" | "test_harness" | "tests"
                    ) {
                        continue;
                    }
                    visit(&path, sources);
                } else if matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("lua" | "rs")
                ) && path.file_name().and_then(|name| name.to_str()) != Some("theme.rs")
                {
                    sources.push_str(
                        &std::fs::read_to_string(&path)
                            .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
                    );
                    sources.push('\n');
                }
            }
        }

        let root = workspace_root();
        let mut sources = String::new();
        visit(&root.join("runtime/lua/smelt"), &mut sources);
        visit(&root.join("crates"), &mut sources);
        sources
    }

    #[test]
    fn bundled_themes_match_the_canonical_group_manifest() {
        let lua = Lua::new();
        let canonical = canonical_group_names(&lua);
        assert_eq!(spec_group_names(&baked_default_spec()), canonical);

        const TWO_FACE_LUA: &str =
            include_str!("../../../runtime/lua/smelt/colorschemes/_two_face.lua");
        let generator: LuaTable = lua
            .load(TWO_FACE_LUA)
            .set_name("colorschemes/_two_face.lua")
            .eval()
            .expect("bundled theme generator must load");
        let theme: LuaFunction = generator.get("theme").expect("theme generator function");
        let schemes: LuaTable = generator.get("schemes").expect("bundled theme list");
        for scheme in schemes.sequence_values::<LuaTable>() {
            let scheme = scheme.expect("bundled theme metadata");
            let module: String = scheme.get("module").expect("bundled theme module");
            let value: LuaValue = theme.call(module.as_str()).expect("generate bundled theme");
            let spec = ThemeSpec::from_lua(value, &lua).expect("decode bundled theme");
            assert_eq!(
                spec_group_names(&spec),
                canonical,
                "bundled theme `{module}` does not match the canonical manifest"
            );
            compile(&spec, false)
                .unwrap_or_else(|error| panic!("bundled theme `{module}` must compile: {error}"));
        }
    }

    #[test]
    fn canonical_groups_match_production_consumers() {
        let lua = Lua::new();
        let canonical = canonical_group_names(&lua);
        let sources = production_group_usage();
        for name in &canonical {
            assert!(
                sources.contains(&format!("\"{name}\"")),
                "canonical group `{name}` has no production consumer"
            );
        }
        for (start, _) in sources.match_indices("\"Smelt") {
            let identifier = sources[start + 1..]
                .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .next()
                .expect("group identifier");
            if identifier != "Smelt" {
                assert!(
                    canonical.contains(identifier),
                    "production source references unknown group `{identifier}`"
                );
            }
        }
    }

    #[test]
    fn documented_smelt_groups_are_canonical() {
        let lua = Lua::new();
        let canonical = canonical_group_names(&lua);
        let root = workspace_root();
        for relative in [
            "docs/docs/customize/appearance.md",
            "docs/docs/customize/settings.md",
            "runtime/skills/customize/SKILL.md",
        ] {
            let text =
                std::fs::read_to_string(root.join(relative)).expect("read theme documentation");
            for identifier in text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_')) {
                if identifier.starts_with("Smelt") && identifier != "Smelt" {
                    assert!(
                        canonical.contains(identifier),
                        "documentation `{relative}` references unknown group `{identifier}`"
                    );
                }
            }
        }
    }

    #[test]
    fn parse_color_replies_reads_every_answer() {
        let replies = parse_color_replies(
            "\x1b]10;rgb:e5e5/e5e5/e5e5\x1b\\\x1b]11;rgb:1c/1c/1c\x07\x1b]4;1;rgb:cdcd/0000/0000\x1b\\\x1b]4;15;rgb:f/f/f\x07\x1b[0n",
        );
        assert_eq!(replies.fg, Some((229, 229, 229)));
        assert_eq!(replies.bg, Some((28, 28, 28)));
        assert_eq!(replies.ansi[1], Some((205, 0, 0)));
        assert_eq!(replies.ansi[15], Some((255, 255, 255)));
        assert_eq!(replies.ansi[2], None);

        let palette = replies.palette(false);
        assert_eq!(palette.bg, (28, 28, 28));
        assert_eq!(palette.ansi[2], smelt_core::palette::XTERM_ANSI[2]);
        assert!(!palette.is_light());
    }

    #[test]
    fn parse_color_replies_ignores_garbage() {
        assert_eq!(parse_color_replies("garbage"), ColorReplies::default());
        assert_eq!(parse_color_replies(""), ColorReplies::default());
        assert_eq!(
            parse_color_replies("\x1b]11;rgb:12345/0/0\x07\x1b]4;99;rgb:0/0/0\x07"),
            ColorReplies::default()
        );
    }

    #[test]
    fn status_report_fences_the_replies() {
        assert!(ends_with_status_report(b"\x1b]11;rgb:0/0/0\x07\x1b[0n"));
        assert!(ends_with_status_report(b"\x1b[3n"));
        assert!(!ends_with_status_report(b"\x1b]11;rgb:0/0/0\x07"));
        assert!(!ends_with_status_report(b"[n"));
    }

    #[test]
    fn baked_default_uses_canonical_group_styles() {
        let theme = default_baked();
        assert_eq!(theme.get("Comment").fg, Some(Color::AnsiValue(244)));
        // SmeltSlug carries the pill fg only; bg falls back to
        // SmeltAccent in statusline.lua so `/color` and theme swaps
        // both propagate through SmeltAccent without rewriting the slug.
        assert_eq!(theme.get("SmeltSlug").fg, Some(Color::AnsiValue(0)));
        assert_eq!(theme.get("SmeltSlug").bg, None);
        assert_eq!(theme.get("SmeltAccent").fg, Some(Color::AnsiValue(208)));
        assert_eq!(theme.get("Search").bg, theme.get("SmeltAccent").fg);
        assert_eq!(theme.get("Search").fg, Some(Color::AnsiValue(0)));
        assert_eq!(theme.get("SmeltLink").fg, Some(Color::AnsiValue(75)));
    }

    #[test]
    fn baked_default_distinguishes_paused_and_blocked_goals() {
        let theme = default_baked();
        assert_eq!(
            theme.get("SmeltGoalBannerPausedLabel").bg,
            Some(Color::AnsiValue(220))
        );
        assert_eq!(
            theme.get("SmeltGoalBannerBlockedLabel").bg,
            Some(Color::AnsiValue(203))
        );
    }

    #[test]
    fn theme_spec_metadata_controls_syntax_and_light_flag() {
        let lua = mlua::Lua::new();
        let value: mlua::Value = lua
            .load(
                r#"{
                    name = "mocha-test",
                    syntax = "Catppuccin Mocha",
                    light = true,
                    groups = {
                        Normal = { fg = { ansi = 15 } },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        let spec = ThemeSpec::from_lua(value, &lua).unwrap();
        let theme = compile(&spec, false).unwrap();
        assert_eq!(spec.name.as_deref(), Some("mocha-test"));
        assert_eq!(theme.syntax_theme(), Some("Catppuccin Mocha"));
        assert!(theme.is_light());
        assert_eq!(theme.get("Normal").fg, Some(Color::AnsiValue(15)));
    }

    #[test]
    fn theme_spec_rejects_unknown_syntax_theme() {
        let lua = mlua::Lua::new();
        let value: mlua::Value = lua
            .load(r#"{ syntax = "No Such Theme", groups = { Normal = {} } }"#)
            .eval()
            .unwrap();
        let err = ThemeSpec::from_lua(value, &lua).unwrap_err();
        assert!(err.to_string().contains("No Such Theme"), "got: {err}");
    }

    #[test]
    fn baked_default_light_branch() {
        let spec = baked_default_spec();
        let dark = compile(&spec, false).unwrap();
        let light = compile(&spec, true).unwrap();
        assert_eq!(dark.get("SmeltUserBg").bg, Some(Color::AnsiValue(236)));
        assert_eq!(light.get("SmeltUserBg").bg, Some(Color::AnsiValue(252)));
    }

    #[test]
    fn baked_default_leaves_syntax_theme_to_the_terminal_background() {
        let spec = baked_default_spec();
        assert_eq!(spec.syntax_theme, None);
    }

    #[test]
    fn compile_reports_dangling_reference() {
        let mut groups = HashMap::new();
        groups.insert("Anchor".into(), GroupDecl::Ref("Missing".into()));
        let spec = ThemeSpec {
            name: None,
            groups,
            syntax_theme: None,
            is_light: None,
        };
        let err = compile(&spec, false).unwrap_err();
        assert!(err.contains("Missing"), "got: {err}");
    }

    #[test]
    fn compile_reports_cycle() {
        let mut groups = HashMap::new();
        groups.insert("A".into(), GroupDecl::Ref("B".into()));
        groups.insert("B".into(), GroupDecl::Ref("A".into()));
        let spec = ThemeSpec {
            name: None,
            groups,
            syntax_theme: None,
            is_light: None,
        };
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
        let spec = ThemeSpec {
            name: None,
            groups,
            syntax_theme: None,
            is_light: None,
        };
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
            theme.get("SmeltDiffDeleteBg").bg,
            Some(Color::Rgb {
                r: 60,
                g: 20,
                b: 20
            })
        );
        assert_eq!(
            theme.get("SmeltDiffAddInlineBg").bg,
            Some(Color::Rgb {
                r: 35,
                g: 95,
                b: 35
            })
        );
        assert_eq!(
            theme.get("SmeltDiffDeleteInlineBg").bg,
            Some(Color::Rgb {
                r: 110,
                g: 35,
                b: 35
            })
        );
    }
}
