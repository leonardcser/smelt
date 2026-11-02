//! Lua-value → typed-Rust converters (pure, no `TuiApp` dependencies; theme
//! groups come in through a [`Groups`] lookup).
//! Errors are returned as `String`; call sites wrap them in `LuaError::RuntimeError`.

use crate::smelt_edit::layout::{Border, BorderStyle, Constraint, EdgeStyle};
use crate::smelt_edit::Corner;
use crate::smelt_edit::{Line, Span, Style};
use mlua::prelude::*;
use smelt_core::style::Color;

// ── color ──────────────────────────────────────────────────────────────

/// The side of a cell a color paints. A theme group name resolves to that
/// group's color on the same side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    Fg,
    Bg,
}

impl Layer {
    fn of(self, style: Style) -> Option<Color> {
        match self {
            Layer::Fg => style.fg,
            Layer::Bg => style.bg,
        }
    }
}

/// Theme group lookup: a group's style, or `None` for a name the theme does
/// not have. `crate::lua::theme_group` reads the active theme.
pub(crate) type Groups<'a> = &'a dyn Fn(&str) -> Option<Style>;

/// The Lua type of a color in the shared grammar, for generated stubs.
pub(crate) const COLOR_TYPE: &str = "integer | string | false | number[]";

/// A color as Lua gives it: a color, or the name of a theme group whose color
/// it stands for. Every Lua API that takes a color reads it this way.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ColorRef {
    Color(Color),
    Group(String),
}

impl ColorRef {
    /// Parse the shared color grammar: any [`color_literal`], or a theme group
    /// name. Groups start with an uppercase letter (`Comment`, `SmeltAccent`);
    /// ANSI names and `reset` are lowercase.
    pub(crate) fn parse(v: &mlua::Value) -> Result<Option<Self>, String> {
        if let mlua::Value::String(s) = v {
            let name = s.to_str().map_err(|e| e.to_string())?;
            if name.starts_with(|c: char| c.is_ascii_uppercase()) {
                return Ok(Some(ColorRef::Group(name.to_string())));
            }
        }
        Ok(color_literal(v)?.map(ColorRef::Color))
    }

    /// The color on `layer`, looking a group up in `groups`. An unknown group
    /// is an error, so a typo does not silently drop the color.
    pub(crate) fn resolve(&self, layer: Layer, groups: Groups) -> Result<Option<Color>, String> {
        match self {
            ColorRef::Color(c) => Ok(Some(*c)),
            ColorRef::Group(name) => groups(name)
                .map(|style| layer.of(style))
                .ok_or_else(|| format!("color: unknown theme group '{name}'")),
        }
    }
}

impl smelt_core::lua::lua_type::LuaType for ColorRef {
    fn lua_type() -> String {
        COLOR_TYPE.into()
    }
}

impl mlua::FromLua for ColorRef {
    fn from_lua(value: mlua::Value, _: &Lua) -> LuaResult<Self> {
        ColorRef::parse(&value)
            .and_then(|c| c.ok_or_else(|| "color: expected a color, got nil".to_string()))
            .map_err(LuaError::RuntimeError)
    }
}

/// Parse and resolve a color in one step: [`ColorRef::parse`] then
/// [`ColorRef::resolve`].
pub(crate) fn color(
    v: &mlua::Value,
    layer: Layer,
    groups: Groups,
) -> Result<Option<Color>, String> {
    ColorRef::parse(v)?
        .map(|c| c.resolve(layer, groups))
        .transpose()
        .map(Option::flatten)
}

/// Parse a color literal: the shared grammar without theme group names, for
/// places that cannot refer to groups, such as a colorscheme's own colors.
///
/// - `nil` → `None`
/// - `false` or `"reset"` → the terminal default
/// - integer `0..=255` → an ANSI 256-color slot
/// - string `"#rrggbb"` (case-insensitive) or an ANSI name (`"red"`, `"darkred"`, …)
/// - `{ r, g, b }` → RGB; channels are rounded and clamped to `0..=255`
pub(crate) fn color_literal(v: &mlua::Value) -> Result<Option<Color>, String> {
    match v {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Boolean(false) => Ok(Some(Color::Reset)),
        mlua::Value::Integer(n) if (0..=255).contains(n) => Ok(Some(Color::AnsiValue(*n as u8))),
        mlua::Value::String(s) => color_str(&s.to_str().map_err(|e| e.to_string())?).map(Some),
        mlua::Value::Table(t) => {
            let channel = |i: usize| -> Result<u8, String> {
                match t.raw_get::<mlua::Value>(i).map_err(|e| e.to_string())? {
                    mlua::Value::Integer(n) => Ok(n.clamp(0, 255) as u8),
                    mlua::Value::Number(n) if n.is_finite() => {
                        Ok(n.round().clamp(0.0, 255.0) as u8)
                    }
                    other => Err(format!(
                        "color[{i}]: expected a number, got {}",
                        other.type_name()
                    )),
                }
            };
            Ok(Some(Color::Rgb {
                r: channel(1)?,
                g: channel(2)?,
                b: channel(3)?,
            }))
        }
        other => Err(format!(
            "color: expected false, an integer, a string or {{ r, g, b }}, got {}",
            other.type_name()
        )),
    }
}

fn color_str(s: &str) -> Result<Color, String> {
    let lower = s.trim().to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("color: '#{hex}' must be 6 hex digits"));
        }
        let r = u8::from_str_radix(&hex[0..2], 16).map_err(|e| format!("color: {e}"))?;
        let g = u8::from_str_radix(&hex[2..4], 16).map_err(|e| format!("color: {e}"))?;
        let b = u8::from_str_radix(&hex[4..6], 16).map_err(|e| format!("color: {e}"))?;
        return Ok(Color::Rgb { r, g, b });
    }
    Ok(match lower.as_str() {
        "reset" => Color::Reset,
        "black" => Color::Black,
        "darkgrey" | "darkgray" => Color::DarkGrey,
        "red" => Color::Red,
        "darkred" => Color::DarkRed,
        "green" => Color::Green,
        "darkgreen" => Color::DarkGreen,
        "yellow" => Color::Yellow,
        "darkyellow" => Color::DarkYellow,
        "blue" => Color::Blue,
        "darkblue" => Color::DarkBlue,
        "magenta" => Color::Magenta,
        "darkmagenta" => Color::DarkMagenta,
        "cyan" => Color::Cyan,
        "darkcyan" => Color::DarkCyan,
        "white" => Color::White,
        "grey" | "gray" => Color::Grey,
        other => return Err(format!("color: unknown name '{other}'")),
    })
}

// ── style + span + title ───────────────────────────────────────────────

/// Shared style value: a theme group or a table of overrides.
#[derive(Clone, Debug)]
pub(crate) enum LuaStyle {
    Group(String),
    Spec(StylePatch),
}

impl smelt_core::lua::lua_type::LuaType for LuaStyle {
    fn lua_type() -> String {
        "smelt.Style | string".into()
    }
}

pub(crate) fn register_style_doc() {
    use smelt_core::lua::lua_type::{LuaClassDecl, LuaClassField};
    let mut fields = vec![LuaClassField {
        name: "hl_group",
        ty: "string".into(),
        optional: true,
        doc: "Theme group used as the base style; explicit fields override it.",
    }];
    for name in ["fg", "bg"] {
        fields.push(LuaClassField {
            name,
            ty: COLOR_TYPE.into(),
            optional: true,
            doc: "Color override. false selects the terminal default.",
        });
    }
    for name in [
        "bold",
        "dim",
        "italic",
        "underline",
        "crossedout",
        "reverse",
    ] {
        fields.push(LuaClassField {
            name,
            ty: "boolean".into(),
            optional: true,
            doc: "Attribute override. false clears it.",
        });
    }
    smelt_core::lua::doc::record_class(LuaClassDecl {
            name: "smelt.Style", classification: smelt_core::lua::doc::classification_for_type("smelt.Style"),
            doc: "Shared style fields for Paint, spans, marks, bars and window chrome. A string theme-group name is shorthand for { hl_group = name }.", fields,
        });
}

impl FromLua for LuaStyle {
    fn from_lua(value: mlua::Value, _: &Lua) -> LuaResult<Self> {
        Self::parse(value, &crate::lua::theme_group).map_err(LuaError::external)
    }
}

impl LuaStyle {
    fn parse(value: mlua::Value, groups: Groups) -> Result<Self, String> {
        match value {
            mlua::Value::String(name) => Ok(Self::Group(
                name.to_str().map_err(|e| e.to_string())?.to_string(),
            )),
            mlua::Value::Table(table) => Ok(Self::Spec(style_patch(&table, groups)?)),
            other => Err(format!(
                "style: expected theme group or style table, got {}",
                other.type_name()
            )),
        }
    }

    fn resolve_with(&self, groups: Groups) -> Result<Style, String> {
        match self {
            Self::Group(name) => {
                groups(name).ok_or_else(|| format!("style: unknown theme group '{name}'"))
            }
            Self::Spec(patch) => Ok(patch.apply_raw(Style::new())),
        }
    }

    pub(crate) fn resolve(&self) -> LuaResult<Style> {
        self.resolve_with(&crate::lua::theme_group)
            .map_err(LuaError::external)
    }

    pub(crate) fn resolved_patch(&self) -> LuaResult<StylePatch> {
        match self {
            Self::Group(_) => Ok(StylePatch {
                group: Some(self.resolve()?),
                ..Default::default()
            }),
            Self::Spec(patch) => Ok(*patch),
        }
    }

    pub(crate) fn group(&self) -> LuaResult<smelt_core::theme::HlGroup> {
        match self {
            Self::Group(name) => {
                self.resolve()?;
                Ok(smelt_core::theme::intern(name))
            }
            Self::Spec(_) => Ok(smelt_core::theme::intern_anonymous_style(self.resolve()?)),
        }
    }
}

pub(crate) use smelt_core::lua::options::validate_keys;

/// Read style fields (`fg`, `bg`, `bold`, `dim`, `italic`, `underline`, `crossedout`, `reverse`)
/// off a table. Unknown keys raise. Colors take any form [`color`]
/// accepts, with theme groups looked up in `groups`; `fg = false` /
/// `bg = false` ask for the terminal default.
#[cfg(test)]
fn style(t: &mlua::Table, groups: Groups) -> Result<Style, String> {
    Ok(style_patch(t, groups)?.apply_raw(Style::new()))
}

/// A partial style: `None` fields keep the base style's value.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StylePatch {
    group: Option<Style>,
    fg: Option<Option<Color>>,
    bg: Option<Option<Color>>,
    bold: Option<bool>,
    dim: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    crossedout: Option<bool>,
    reverse: Option<bool>,
}

impl StylePatch {
    pub(crate) fn apply(self, base: Style) -> Style {
        let mut out = self.apply_raw(base);
        out.fg = out.fg.filter(|color| *color != Color::Reset);
        out.bg = out.bg.filter(|color| *color != Color::Reset);
        out
    }

    fn apply_raw(self, base: Style) -> Style {
        let base = self.group.unwrap_or(base);
        Style {
            fg: self.fg.unwrap_or(base.fg),
            bg: self.bg.unwrap_or(base.bg),
            bold: self.bold.unwrap_or(base.bold),
            dim: self.dim.unwrap_or(base.dim),
            italic: self.italic.unwrap_or(base.italic),
            underline: self.underline.unwrap_or(base.underline),
            crossedout: self.crossedout.unwrap_or(base.crossedout),
            reverse: self.reverse.unwrap_or(base.reverse),
        }
    }
}

/// Parse a partial style table: only keys present in `t` change.
/// `fg = false` / `bg = false` clear a color.
pub(crate) fn style_patch(t: &mlua::Table, groups: Groups) -> Result<StylePatch, String> {
    validate_keys(
        t,
        &[
            "hl_group",
            "fg",
            "bg",
            "bold",
            "dim",
            "italic",
            "underline",
            "crossedout",
            "reverse",
        ],
        "style",
    )
    .map_err(|e| e.to_string())?;
    let mut patch = StylePatch::default();
    if let Some(name) = t
        .get::<Option<String>>("hl_group")
        .map_err(|e| e.to_string())?
    {
        patch.group =
            Some(groups(&name).ok_or_else(|| format!("style: unknown theme group '{name}'"))?);
    }
    for (key, layer, slot) in [
        ("fg", Layer::Fg, &mut patch.fg),
        ("bg", Layer::Bg, &mut patch.bg),
    ] {
        match t.get::<mlua::Value>(key).map_err(|e| e.to_string())? {
            mlua::Value::Nil => {}
            mlua::Value::Boolean(false) => *slot = Some(Some(Color::Reset)),
            value => *slot = Some(color(&value, layer, groups).map_err(|e| format!("{key}: {e}"))?),
        }
    }
    for (key, slot) in [
        ("bold", &mut patch.bold),
        ("dim", &mut patch.dim),
        ("italic", &mut patch.italic),
        ("underline", &mut patch.underline),
        ("crossedout", &mut patch.crossedout),
        ("reverse", &mut patch.reverse),
    ] {
        *slot = match t.get::<mlua::Value>(key).map_err(|e| e.to_string())? {
            mlua::Value::Nil => None,
            mlua::Value::Boolean(value) => Some(value),
            _ => return Err(format!("style.{key}: expected boolean")),
        };
    }
    Ok(patch)
}

/// A rect as the `{ x, y, width, height }` table every Lua API uses.
pub(crate) fn rect_to_lua(lua: &Lua, rect: crate::smelt_edit::Rect) -> LuaResult<mlua::Table> {
    let t = lua.create_table_with_capacity(0, 4)?;
    t.set("x", rect.x)?;
    t.set("y", rect.y)?;
    t.set("width", rect.width)?;
    t.set("height", rect.height)?;
    Ok(t)
}

/// Project a `Style` to a Lua style table. Unset fields are omitted so the
/// table round-trips through [`style`] and `smelt.theme.apply`.
pub(crate) fn style_to_lua(lua: &Lua, style: Style) -> LuaResult<mlua::Table> {
    // Room for the colors; attribute flags are rare.
    let t = lua.create_table_with_capacity(0, 2)?;
    if let Some(fg) = style.fg {
        t.set("fg", color_to_lua(lua, fg)?)?;
    }
    if let Some(bg) = style.bg {
        t.set("bg", color_to_lua(lua, bg)?)?;
    }
    if style.bold {
        t.set("bold", true)?;
    }
    if style.italic {
        t.set("italic", true)?;
    }
    if style.dim {
        t.set("dim", true)?;
    }
    if style.underline {
        t.set("underline", true)?;
    }
    if style.crossedout {
        t.set("crossedout", true)?;
    }
    if style.reverse {
        t.set("reverse", true)?;
    }
    Ok(t)
}

/// Project a color to the shared grammar [`color_literal`] reads: `{ r, g, b }`
/// for RGB, an integer for an ANSI slot, and `false` for the terminal default.
pub(crate) fn color_to_lua(lua: &Lua, color: Color) -> LuaResult<mlua::Value> {
    Ok(match color {
        Color::Rgb { r, g, b } => mlua::Value::Table(lua.create_sequence_from([r, g, b])?),
        Color::AnsiValue(v) => mlua::Value::Integer(v.into()),
        Color::Reset => mlua::Value::Boolean(false),
        named => match crate::lua::api::color_to_ansi(named) {
            Some(idx) => mlua::Value::Integer(idx.into()),
            None => mlua::Value::Boolean(false),
        },
    })
}

pub(crate) fn resolved_style(value: mlua::Value, groups: Groups) -> Result<Style, String> {
    if value.is_nil() {
        Ok(Style::new())
    } else {
        LuaStyle::parse(value, groups)?.resolve_with(groups)
    }
}

fn span(t: &mlua::Table, groups: Groups) -> Result<Span<'static>, String> {
    let text: String = t
        .get::<String>("text")
        .map_err(|e| format!("span.text: {e}"))?;
    let value = t.get::<mlua::Value>("style").map_err(|e| e.to_string())?;
    let resolved = resolved_style(value, groups)?;
    Ok(Span::styled(text, resolved))
}

/// Parse a Lua-side title spec into a styled [`Line`]. Surface:
///
/// - omitted / `nil` → `None`
/// - string → single-span Line with default style
/// - single-span table `{ text = "...", style = { fg = "red", bold = true } }` →
///   single-span Line
/// - sequence of span tables `{ {text=..,style=..}, " ", {text=..} }` →
///   multi-span Line. List items can be plain strings (default style)
///   or span tables.
pub(crate) fn title(
    v: Option<mlua::Value>,
    groups: Groups,
) -> Result<Option<Line<'static>>, String> {
    match v {
        None | Some(mlua::Value::Nil) => Ok(None),
        Some(mlua::Value::String(s)) => Ok(Some(Line::raw(s.to_string_lossy().to_string()))),
        Some(mlua::Value::Table(t)) => {
            // Table with `text` key → single-span Line.
            if t.contains_key("text").unwrap_or(false) {
                return Ok(Some(Line::from_spans([span(&t, groups)?])));
            }
            let mut spans: Vec<Span<'static>> = Vec::new();
            for v in t.sequence_values::<mlua::Value>() {
                let v = v.map_err(|e| format!("title span: {e}"))?;
                match v {
                    mlua::Value::String(s) => {
                        spans.push(Span::raw(s.to_string_lossy().to_string()));
                    }
                    mlua::Value::Table(st) => spans.push(span(&st, groups)?),
                    other => {
                        return Err(format!(
                            "title span: expected string or table, got {}",
                            other.type_name()
                        ))
                    }
                }
            }
            if spans.is_empty() {
                return Ok(None);
            }
            Ok(Some(Line::from_spans(spans)))
        }
        Some(other) => Err(format!(
            "expected string | table | nil, got {}",
            other.type_name()
        )),
    }
}

// ── constraint ─────────────────────────────────────────────────────────

/// Parse a layout `Constraint`. Surface:
///
/// - omitted / `nil` → `Fill`
/// - integer `n >= 0` → `Length(n)` (zero is valid: a slot with no rows /
///   columns, useful as a hidden focus anchor that still participates in
///   `overlay_for_leaf`)
/// - string `"fill"` / `"fit"` → `Fill` / `Fit`
/// - string `"N%"` → `Percentage(N)` (shorthand for the common case)
/// - string `"min:N"` / `"max:N"` / `"pct:N"` / `"ratio:N/M"` →
///   matching variant; `"length:N"` / `"len:N"` are aliases of an
///   integer literal.
/// - table `{ kind = "min", n = 5 }` etc. - long form.
pub(crate) fn constraint(v: Option<mlua::Value>, ctx: &str) -> Result<Constraint, String> {
    match v {
        None | Some(mlua::Value::Nil) => Ok(Constraint::Fill),
        Some(mlua::Value::Integer(n)) if n >= 0 => Ok(Constraint::Length(n as u16)),
        Some(mlua::Value::Number(n)) if n >= 0.0 => Ok(Constraint::Length(n as u16)),
        Some(mlua::Value::Integer(n)) => Err(format!(
            "{ctx}: length constraint must be non-negative, got {n}"
        )),
        Some(mlua::Value::Number(n)) => Err(format!(
            "{ctx}: length constraint must be non-negative, got {n}"
        )),
        Some(mlua::Value::String(s)) => {
            let raw = s.to_str().map_err(|e| e.to_string())?.to_string();
            constraint_str(&raw, ctx)
        }
        Some(mlua::Value::Table(t)) => constraint_table(&t, ctx),
        Some(other) => Err(format!(
            "{ctx}: expected int | string | table | nil, got {}",
            other.type_name()
        )),
    }
}

fn constraint_str(raw: &str, ctx: &str) -> Result<Constraint, String> {
    let s = raw.trim();
    if s == "fill" {
        return Ok(Constraint::Fill);
    }
    if s == "fit" {
        return Ok(Constraint::Fit);
    }
    if let Some(rest) = s.strip_suffix('%') {
        return parse_u16(rest.trim(), ctx).map(Constraint::Percentage);
    }
    if let Some((kind, rest)) = s.split_once(':') {
        let rest = rest.trim();
        return match kind.trim() {
            "length" | "len" => parse_u16(rest, ctx).map(Constraint::Length),
            "min" => parse_u16(rest, ctx).map(Constraint::Min),
            "max" => parse_u16(rest, ctx).map(Constraint::Max),
            "pct" | "percentage" => parse_u16(rest, ctx).map(Constraint::Percentage),
            "ratio" => {
                let (a, b) = rest
                    .split_once('/')
                    .ok_or_else(|| format!("{ctx}: ratio expects 'N/M', got '{rest}'"))?;
                Ok(Constraint::Ratio(
                    parse_u16(a.trim(), ctx)?,
                    parse_u16(b.trim(), ctx)?,
                ))
            }
            other => Err(format!(
                "{ctx}: unknown kind '{other}' (expected length|fit|fill|min|max|pct|ratio)"
            )),
        };
    }
    Err(format!(
        "{ctx}: unknown value '{s}' (expected fit|fill|'N%'|'<kind>:<n>')"
    ))
}

fn constraint_table(t: &mlua::Table, ctx: &str) -> Result<Constraint, String> {
    let kind: String = t
        .get::<String>("kind")
        .map_err(|e| format!("{ctx}: missing 'kind': {e}"))?;
    match kind.as_str() {
        "fill" => Ok(Constraint::Fill),
        "fit" => Ok(Constraint::Fit),
        "length" | "len" => Ok(Constraint::Length(table_u16(t, "n", ctx)?)),
        "min" => Ok(Constraint::Min(table_u16(t, "n", ctx)?)),
        "max" => Ok(Constraint::Max(table_u16(t, "n", ctx)?)),
        "pct" | "percentage" => Ok(Constraint::Percentage(table_u16(t, "n", ctx)?)),
        "ratio" => Ok(Constraint::Ratio(
            table_u16(t, "num", ctx)?,
            table_u16(t, "den", ctx)?,
        )),
        other => Err(format!(
            "{ctx}: unknown kind '{other}' (expected length|fit|fill|min|max|pct|ratio)"
        )),
    }
}

fn parse_u16(s: &str, ctx: &str) -> Result<u16, String> {
    s.parse::<u16>()
        .map_err(|e| format!("{ctx}: expected u16, got '{s}': {e}"))
}

fn table_u16(t: &mlua::Table, key: &str, ctx: &str) -> Result<u16, String> {
    t.get::<u16>(key)
        .map_err(|e| format!("{ctx}: missing '{key}' (u16): {e}"))
}

// ── border + corner ────────────────────────────────────────────────────

/// Parse a border spec from a table that may carry the `border` key:
///
/// - `border = "single"` (default) / `"rounded"` / `"double"` - all four sides,
///   default color.
/// - `border = "none"` or `border = false` - no border at all.
/// - `border = "top"` - single style, top edge only, default color.
/// - `border = { style = "rounded", top = "accent", bottom = true }` - per-side
///   table form. Each side key is `nil`/`false` (off), `true` (on, default
///   color), a theme-role string (on, fg = `theme.resolve(role)`), or a table
///   `{ color = "..." }`. `all = "..."` sugar applies to every side; per-side
///   keys override `all`.
pub(crate) fn border(opts: &mlua::Table) -> Result<Option<Border>, String> {
    let v = opts
        .get::<mlua::Value>("border")
        .unwrap_or(mlua::Value::Nil);
    match v {
        mlua::Value::Nil => Ok(Some(Border::SINGLE)),
        mlua::Value::Boolean(false) => Ok(None),
        mlua::Value::Boolean(true) => Ok(Some(Border::SINGLE)),
        mlua::Value::String(s) => {
            let raw = s.to_string_lossy();
            match raw.as_ref() {
                "none" => Ok(None),
                "rounded" => Ok(Some(Border::ROUNDED)),
                "double" => Ok(Some(Border::DOUBLE)),
                "single" => Ok(Some(Border::SINGLE)),
                "top" => Ok(Some(Border::single().top(()))),
                other => Err(format!(
                    "unknown border preset '{other}' (expected single|rounded|double|none|top)"
                )),
            }
        }
        mlua::Value::Table(t) => {
            let style = match t.get::<Option<String>>("style").ok().flatten().as_deref() {
                None | Some("single") => BorderStyle::Single,
                Some("rounded") => BorderStyle::Rounded,
                Some("double") => BorderStyle::Double,
                Some("dashed") => BorderStyle::Dashed,
                Some(other) => {
                    return Err(format!(
                        "unknown border style '{other}' (expected single|rounded|double|dashed)"
                    ))
                }
            };
            let mut b = Border {
                style,
                ..Border::OFF
            };
            // `all = ...` sugar: applies to every side first; per-side keys override.
            if let Some(all) = edge_opt(t.get::<mlua::Value>("all").unwrap_or(mlua::Value::Nil))? {
                b.top = Some(all);
                b.right = Some(all);
                b.bottom = Some(all);
                b.left = Some(all);
            }
            for (key, slot) in [
                ("top", &mut b.top),
                ("right", &mut b.right),
                ("bottom", &mut b.bottom),
                ("left", &mut b.left),
            ] {
                let v = t.get::<mlua::Value>(key).unwrap_or(mlua::Value::Nil);
                match v {
                    mlua::Value::Nil => {} // keep existing (possibly from `all`)
                    mlua::Value::Boolean(false) => *slot = None,
                    other => *slot = edge_opt(other)?,
                }
            }
            if !b.any_side() {
                return Ok(None);
            }
            Ok(Some(b))
        }
        other => Err(format!(
            "border: expected string|table|bool, got {}",
            other.type_name()
        )),
    }
}

/// Parse one side value into `Option<EdgeStyle>`. `nil`/`false` → None;
/// `true` → enabled, default color; string → enabled with theme group; table
/// `{ color = "..." }` → enabled with that color.
fn edge_opt(v: mlua::Value) -> Result<Option<EdgeStyle>, String> {
    match v {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Boolean(false) => Ok(None),
        mlua::Value::Boolean(true) => Ok(Some(EdgeStyle::new())),
        mlua::Value::String(s) => {
            let raw = s.to_str().map_err(|e| e.to_string())?.to_string();
            Ok(Some(EdgeStyle::with_color(smelt_core::theme::intern(&raw))))
        }
        mlua::Value::Table(t) => {
            let color_v = t.get::<mlua::Value>("color").unwrap_or(mlua::Value::Nil);
            match color_v {
                mlua::Value::Nil => Ok(Some(EdgeStyle::new())),
                mlua::Value::String(s) => {
                    let raw = s.to_str().map_err(|e| e.to_string())?.to_string();
                    Ok(Some(EdgeStyle::with_color(smelt_core::theme::intern(&raw))))
                }
                other => Err(format!(
                    "border edge color: expected string, got {}",
                    other.type_name()
                )),
            }
        }
        other => Err(format!(
            "border edge: expected nil|bool|string|table, got {}",
            other.type_name()
        )),
    }
}

/// Parse `"nw"` / `"ne"` / `"sw"` / `"se"` into a `Corner`. Falls back to `default`.
pub(crate) fn corner(name: Option<&str>, default: Corner) -> Corner {
    match name {
        Some("nw") => Corner::NW,
        Some("ne") => Corner::NE,
        Some("sw") => Corner::SW,
        Some("se") => Corner::SE,
        _ => default,
    }
}

/// Parse a 9-point alignment string into an [`Align`]. Accepts
/// `"nw"|"n"|"ne"|"w"|"center"|"e"|"sw"|"s"|"se"` (case-insensitive).
/// Falls back to `default` when `name` is `None`; an unknown name errors.
pub(crate) fn align(
    name: Option<&str>,
    default: crate::smelt_edit::Align,
) -> Result<crate::smelt_edit::Align, String> {
    use crate::smelt_edit::Align;
    let Some(raw) = name else {
        return Ok(default);
    };
    match raw.to_ascii_lowercase().as_str() {
        "nw" => Ok(Align::NW),
        "n" => Ok(Align::N),
        "ne" => Ok(Align::NE),
        "w" => Ok(Align::W),
        "center" | "c" => Ok(Align::Center),
        "e" => Ok(Align::E),
        "sw" => Ok(Align::SW),
        "s" => Ok(Align::S),
        "se" => Ok(Align::SE),
        other => Err(format!(
            "unknown alignment '{other}' (expected nw|n|ne|w|center|e|sw|s|se)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    fn lua() -> Lua {
        Lua::new()
    }

    fn eval_table(lua: &Lua, src: &str) -> mlua::Table {
        lua.load(src).eval().expect("eval")
    }

    fn eval_value(lua: &Lua, src: &str) -> mlua::Value {
        lua.load(src).eval().expect("eval")
    }

    // ── color ──────────────────────────────────────────────────────────

    fn literal(src: &str) -> Result<Option<Color>, String> {
        let lua = lua();
        color_literal(&eval_value(&lua, src))
    }

    #[test]
    fn color_literal_reads_every_form() {
        assert_eq!(literal("return nil").unwrap(), None);
        assert_eq!(literal("return false").unwrap(), Some(Color::Reset));
        assert_eq!(literal(r#"return "reset""#).unwrap(), Some(Color::Reset));
        assert_eq!(literal("return 42").unwrap(), Some(Color::AnsiValue(42)));
        assert_eq!(literal(r#"return "red""#).unwrap(), Some(Color::Red));
        assert_eq!(
            literal(r##"return "#FF8000""##).unwrap(),
            Some(Color::Rgb {
                r: 0xFF,
                g: 0x80,
                b: 0x00
            })
        );
        assert_eq!(
            literal("return { 18, 22, 30 }").unwrap(),
            Some(Color::Rgb {
                r: 18,
                g: 22,
                b: 30
            })
        );
    }

    #[test]
    fn color_literal_rounds_and_clamps_channels() {
        assert_eq!(
            literal("return { 12.6, -40, 300.2 }").unwrap(),
            Some(Color::Rgb {
                r: 13,
                g: 0,
                b: 255
            })
        );
    }

    #[test]
    fn color_round_trips_through_lua() {
        let lua = lua();
        for color in [
            Color::Rgb { r: 1, g: 2, b: 3 },
            Color::AnsiValue(208),
            Color::Reset,
        ] {
            let value = color_to_lua(&lua, color).unwrap();
            assert_eq!(color_literal(&value).unwrap(), Some(color));
        }
    }

    #[test]
    fn color_literal_rejects_bad_input() {
        assert!(literal(r#"return "fuchsia""#)
            .unwrap_err()
            .contains("unknown name"));
        assert!(literal(r##"return "#abc""##)
            .unwrap_err()
            .contains("must be 6 hex digits"));
        assert!(literal("return { 1, 2 }").unwrap_err().contains("color[3]"));
        assert!(literal("return 300").is_err());
    }

    fn groups(name: &str) -> Option<Style> {
        (name == "Comment").then(|| Style::new().fg(Color::AnsiValue(8)).bg(Color::AnsiValue(0)))
    }

    #[test]
    fn group_names_resolve_per_layer() {
        let lua = lua();
        let read = |src: &str, layer| color(&eval_value(&lua, src), layer, &groups);
        assert_eq!(
            read(r#"return "Comment""#, Layer::Fg).unwrap(),
            Some(Color::AnsiValue(8))
        );
        assert_eq!(
            read(r#"return "Comment""#, Layer::Bg).unwrap(),
            Some(Color::AnsiValue(0))
        );
        assert_eq!(
            read(r#"return "red""#, Layer::Fg).unwrap(),
            Some(Color::Red)
        );
        assert!(read(r#"return "Coment""#, Layer::Fg)
            .unwrap_err()
            .contains("unknown theme group 'Coment'"));
    }

    // ── style ──────────────────────────────────────────────────────────

    #[test]
    fn style_reads_fg_bg_attrs() {
        let lua = lua();
        let t = eval_table(
            &lua,
            r#"return { fg = "green", bg = "black", bold = true, italic = true }"#,
        );
        let s = style(&t, &groups).unwrap();
        assert_eq!(s.fg, Some(Color::Green));
        assert_eq!(s.bg, Some(Color::Black));
        assert!(s.bold);
        assert!(s.italic);
        assert!(!s.dim);
    }

    #[test]
    fn style_rejects_unknown_keys() {
        let lua = lua();
        let t = eval_table(&lua, r#"return { fg = "red", boldd = true }"#);
        assert!(style(&t, &groups).unwrap_err().contains("unknown option"));
    }

    // ── title ──────────────────────────────────────────────────────────

    #[test]
    fn title_string_becomes_single_span_line() {
        let lua = lua();
        let v = eval_value(&lua, r#"return "hello""#);
        let line = title(Some(v), &groups).unwrap().expect("some line");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].text, "hello");
    }

    #[test]
    fn title_single_span_table() {
        let lua = lua();
        let v = eval_value(
            &lua,
            r#"return { text = "warn", style = { fg = "yellow", bold = true } }"#,
        );
        let line = title(Some(v), &groups).unwrap().expect("some line");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].text, "warn");
        assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
        assert!(line.spans[0].style.bold);
    }

    #[test]
    fn title_sequence_of_spans() {
        let lua = lua();
        let v = eval_value(
            &lua,
            r#"return { { text = "[", style = { fg = "grey" } }, "info", { text = "]", style = { fg = "grey" } } }"#,
        );
        let line = title(Some(v), &groups).unwrap().expect("some line");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].text, "info");
        assert_eq!(line.spans[1].style.fg, None);
    }

    #[test]
    fn title_empty_sequence_returns_none() {
        let lua = lua();
        let v = eval_value(&lua, "return {}");
        assert!(title(Some(v), &groups).unwrap().is_none());
    }

    // ── constraint ────────────────────────────────────────────────────

    #[test]
    fn constraint_nil_is_fill() {
        assert_eq!(constraint(None, "ctx").unwrap(), Constraint::Fill);
    }

    #[test]
    fn constraint_integer_is_length() {
        let lua = lua();
        let v = eval_value(&lua, "return 5");
        assert_eq!(constraint(Some(v), "ctx").unwrap(), Constraint::Length(5));
    }

    #[test]
    fn constraint_string_short_forms() {
        let lua = lua();
        for (src, expected) in [
            (r#"return "fill""#, Constraint::Fill),
            (r#"return "fit""#, Constraint::Fit),
            (r#"return "min:5""#, Constraint::Min(5)),
            (r#"return "max:10""#, Constraint::Max(10)),
            (r#"return "pct:30""#, Constraint::Percentage(30)),
            // `"N%"` is the percentage shorthand. Same Percentage variant as
            // `"pct:N"`; both exist so users coming from CSS-style sizing get
            // the obvious form.
            (r#"return "30%""#, Constraint::Percentage(30)),
            (r#"return "100%""#, Constraint::Percentage(100)),
            (r#"return "ratio:1/3""#, Constraint::Ratio(1, 3)),
            (r#"return "length:7""#, Constraint::Length(7)),
        ] {
            let v = eval_value(&lua, src);
            assert_eq!(constraint(Some(v), "ctx").unwrap(), expected, "src = {src}");
        }
    }

    #[test]
    fn constraint_table_long_form() {
        let lua = lua();
        let v = eval_value(&lua, r#"return { kind = "ratio", num = 2, den = 5 }"#);
        assert_eq!(constraint(Some(v), "ctx").unwrap(), Constraint::Ratio(2, 5));
    }

    #[test]
    fn constraint_unknown_kind_errors() {
        let lua = lua();
        let v = eval_value(&lua, r#"return "blub:5""#);
        assert!(constraint(Some(v), "h")
            .unwrap_err()
            .contains("unknown kind"));
    }

    // ── border ────────────────────────────────────────────────────────

    #[test]
    fn border_default_is_single_all_sides() {
        let lua = lua();
        let t = eval_table(&lua, "return {}");
        let b = border(&t).unwrap().expect("some border");
        assert_eq!(b.style, BorderStyle::Single);
        assert!(b.top.is_some() && b.right.is_some() && b.bottom.is_some() && b.left.is_some());
    }

    #[test]
    fn border_none_returns_none() {
        let lua = lua();
        let t = eval_table(&lua, r#"return { border = "none" }"#);
        assert!(border(&t).unwrap().is_none());
    }

    #[test]
    fn border_table_per_side_keys_with_color() {
        let lua = lua();
        let t = eval_table(
            &lua,
            r#"return { border = { style = "single", top = "accent", bottom = true } }"#,
        );
        let b = border(&t).unwrap().expect("some border");
        assert_eq!(b.style, BorderStyle::Single);
        assert!(b.top.unwrap().color.is_some());
        assert!(b.bottom.unwrap().color.is_none());
        assert!(b.left.is_none() && b.right.is_none());
    }

    #[test]
    fn border_all_sugar_applies_to_every_side() {
        let lua = lua();
        let t = eval_table(
            &lua,
            r#"return { border = { style = "rounded", all = "accent" } }"#,
        );
        let b = border(&t).unwrap().expect("some border");
        assert_eq!(b.style, BorderStyle::Rounded);
        for e in [b.top, b.right, b.bottom, b.left] {
            assert!(e.unwrap().color.is_some());
        }
    }

    // ── corner ────────────────────────────────────────────────────────

    #[test]
    fn corner_known_names() {
        assert_eq!(corner(Some("nw"), Corner::SE), Corner::NW);
        assert_eq!(corner(Some("ne"), Corner::SE), Corner::NE);
        assert_eq!(corner(Some("sw"), Corner::SE), Corner::SW);
        assert_eq!(corner(Some("se"), Corner::NW), Corner::SE);
    }

    #[test]
    fn corner_unknown_falls_back_to_default() {
        assert_eq!(corner(Some("middle"), Corner::NW), Corner::NW);
        assert_eq!(corner(None, Corner::NE), Corner::NE);
    }
}
