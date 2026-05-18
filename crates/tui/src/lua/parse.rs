//! Lua-value → typed-Rust converters (pure, no `TuiApp` dependencies).
//! Errors are returned as `String`; call sites wrap them in `LuaError::RuntimeError`.

use crate::smelt_term::layout::{Border, BorderStyle, Constraint, Corner, EdgeStyle};
use crate::smelt_term::{Line, Span, Style};
use smelt_core::style::Color;

// ── color ──────────────────────────────────────────────────────────────

/// Accepts:
///
/// - `nil` → `None`
/// - integer `0..=255` → `AnsiValue`
/// - string `"reset"`, named ANSI (`"red"`, `"darkred"`, …, `"grey"`)
/// - string `"#RRGGBB"` (case-insensitive)
/// - table `{ r=, g=, b= }`
pub(crate) fn color_opt(v: Option<mlua::Value>) -> Result<Option<Color>, String> {
    match v {
        None | Some(mlua::Value::Nil) => Ok(None),
        Some(mlua::Value::Integer(n)) if (0..=255).contains(&n) => {
            Ok(Some(Color::AnsiValue(n as u8)))
        }
        Some(mlua::Value::String(s)) => {
            let raw = s.to_str().map_err(|e| e.to_string())?.to_string();
            color_str(&raw).map(Some)
        }
        Some(mlua::Value::Table(t)) => {
            let r: u8 = t.get("r").map_err(|e| format!("color.r: {e}"))?;
            let g: u8 = t.get("g").map_err(|e| format!("color.g: {e}"))?;
            let b: u8 = t.get("b").map_err(|e| format!("color.b: {e}"))?;
            Ok(Some(Color::Rgb { r, g, b }))
        }
        Some(other) => Err(format!(
            "color: expected string | integer | table, got {}",
            other.type_name()
        )),
    }
}

fn color_str(s: &str) -> Result<Color, String> {
    let lower = s.trim().to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix('#') {
        if hex.len() != 6 {
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

/// Read style fields (`fg`, `bg`, `bold`, `dim`, `italic`, `underline`, `crossedout`)
/// off a table. Unrecognised keys are ignored.
pub(crate) fn style(t: &mlua::Table) -> Result<Style, String> {
    let mut style = Style::new();
    if let Some(c) = color_opt(t.get::<mlua::Value>("fg").ok())? {
        style = style.fg(c);
    }
    if let Some(c) = color_opt(t.get::<mlua::Value>("bg").ok())? {
        style = style.bg(c);
    }
    if t.get::<bool>("bold").unwrap_or(false) {
        style = style.bold();
    }
    if t.get::<bool>("dim").unwrap_or(false) {
        style = style.dim();
    }
    if t.get::<bool>("italic").unwrap_or(false) {
        style = style.italic();
    }
    if t.get::<bool>("underline").unwrap_or(false) {
        style = style.underline();
    }
    if t.get::<bool>("crossedout").unwrap_or(false) {
        style = style.crossedout();
    }
    Ok(style)
}

fn span(t: &mlua::Table) -> Result<Span<'static>, String> {
    let text: String = t
        .get::<String>("text")
        .map_err(|e| format!("span.text: {e}"))?;
    Ok(Span::styled(text, style(t)?))
}

/// Parse a Lua-side title spec into a styled [`Line`]. Surface:
///
/// - omitted / `nil` → `None`
/// - string → single-span Line with default style
/// - single-span table `{ text = "...", fg = "red", bold = true }` →
///   single-span Line
/// - sequence of span tables `{ {text=..,fg=..}, " ", {text=..} }` →
///   multi-span Line. List items can be plain strings (default style)
///   or span tables.
pub(crate) fn title(v: Option<mlua::Value>) -> Result<Option<Line<'static>>, String> {
    match v {
        None | Some(mlua::Value::Nil) => Ok(None),
        Some(mlua::Value::String(s)) => Ok(Some(Line::raw(s.to_string_lossy().to_string()))),
        Some(mlua::Value::Table(t)) => {
            // Table with `text` key → single-span Line.
            if t.contains_key("text").unwrap_or(false) {
                return Ok(Some(Line::from_spans([span(&t)?])));
            }
            let mut spans: Vec<Span<'static>> = Vec::new();
            for v in t.sequence_values::<mlua::Value>() {
                let v = v.map_err(|e| format!("title span: {e}"))?;
                match v {
                    mlua::Value::String(s) => {
                        spans.push(Span::raw(s.to_string_lossy().to_string()));
                    }
                    mlua::Value::Table(st) => spans.push(span(&st)?),
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
/// - integer `n > 0` → `Length(n)`
/// - string `"fill"` / `"fit"` → `Fill` / `Fit`
/// - string `"N%"` → `Percentage(N)` (shorthand for the common case)
/// - string `"min:N"` / `"max:N"` / `"pct:N"` / `"ratio:N/M"` →
///   matching variant; `"length:N"` / `"len:N"` are aliases of an
///   integer literal.
/// - table `{ kind = "min", n = 5 }` etc. — long form.
pub(crate) fn constraint(v: Option<mlua::Value>, ctx: &str) -> Result<Constraint, String> {
    match v {
        None | Some(mlua::Value::Nil) => Ok(Constraint::Fill),
        Some(mlua::Value::Integer(n)) if n > 0 => Ok(Constraint::Length(n as u16)),
        Some(mlua::Value::Number(n)) if n > 0.0 => Ok(Constraint::Length(n as u16)),
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
/// - `border = "single"` (default) / `"rounded"` / `"double"` — all four sides,
///   default color.
/// - `border = "none"` or `border = false` — no border at all.
/// - `border = "top"` — single style, top edge only, default color.
/// - `border = { style = "rounded", top = "accent", bottom = true }` — per-side
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
            // Back-compat: `sides = { "top", "left" }` or `sides = { top = true }`.
            if let mlua::Value::Table(st) =
                t.get::<mlua::Value>("sides").unwrap_or(mlua::Value::Nil)
            {
                apply_legacy_sides(&st, &mut b)?;
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

fn apply_legacy_sides(st: &mlua::Table, b: &mut Border) -> Result<(), String> {
    let mut saw_list = false;
    let mut top = false;
    let mut right = false;
    let mut bottom = false;
    let mut left = false;
    for v in st.clone().sequence_values::<String>().flatten() {
        saw_list = true;
        match v.as_str() {
            "top" => top = true,
            "right" => right = true,
            "bottom" => bottom = true,
            "left" => left = true,
            other => {
                return Err(format!(
                    "unknown border side '{other}' (expected top|right|bottom|left)"
                ))
            }
        }
    }
    if !saw_list {
        top = st.get::<bool>("top").unwrap_or(false);
        right = st.get::<bool>("right").unwrap_or(false);
        bottom = st.get::<bool>("bottom").unwrap_or(false);
        left = st.get::<bool>("left").unwrap_or(false);
    }
    if top {
        b.top = Some(EdgeStyle::new());
    }
    if right {
        b.right = Some(EdgeStyle::new());
    }
    if bottom {
        b.bottom = Some(EdgeStyle::new());
    }
    if left {
        b.left = Some(EdgeStyle::new());
    }
    Ok(())
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
    default: crate::smelt_term::Align,
) -> Result<crate::smelt_term::Align, String> {
    use crate::smelt_term::Align;
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

    #[test]
    fn color_opt_nil_returns_none() {
        assert_eq!(color_opt(None).unwrap(), None);
        assert_eq!(color_opt(Some(mlua::Value::Nil)).unwrap(), None);
    }

    #[test]
    fn color_opt_named_ansi() {
        let lua = lua();
        let v = eval_value(&lua, r#"return "red""#);
        assert_eq!(color_opt(Some(v)).unwrap(), Some(Color::Red));
    }

    #[test]
    fn color_opt_hex_rgb() {
        let lua = lua();
        let v = eval_value(&lua, r##"return "#FF8000""##);
        assert_eq!(
            color_opt(Some(v)).unwrap(),
            Some(Color::Rgb {
                r: 0xFF,
                g: 0x80,
                b: 0x00
            })
        );
    }

    #[test]
    fn color_opt_rgb_table() {
        let lua = lua();
        let v = eval_value(&lua, "return { r = 18, g = 22, b = 30 }");
        assert_eq!(
            color_opt(Some(v)).unwrap(),
            Some(Color::Rgb {
                r: 18,
                g: 22,
                b: 30
            })
        );
    }

    #[test]
    fn color_opt_ansi_integer() {
        let lua = lua();
        let v = eval_value(&lua, "return 42");
        assert_eq!(color_opt(Some(v)).unwrap(), Some(Color::AnsiValue(42)));
    }

    #[test]
    fn color_opt_unknown_name_errors() {
        let lua = lua();
        let v = eval_value(&lua, r#"return "fuchsia""#);
        assert!(color_opt(Some(v)).unwrap_err().contains("unknown name"));
    }

    #[test]
    fn color_opt_short_hex_errors() {
        let lua = lua();
        let v = eval_value(&lua, r##"return "#abc""##);
        assert!(color_opt(Some(v))
            .unwrap_err()
            .contains("must be 6 hex digits"));
    }

    // ── style ──────────────────────────────────────────────────────────

    #[test]
    fn style_reads_fg_bg_attrs() {
        let lua = lua();
        let t = eval_table(
            &lua,
            r#"return { fg = "green", bg = "black", bold = true, italic = true }"#,
        );
        let s = style(&t).unwrap();
        assert_eq!(s.fg, Some(Color::Green));
        assert_eq!(s.bg, Some(Color::Black));
        assert!(s.bold);
        assert!(s.italic);
        assert!(!s.dim);
    }

    #[test]
    fn style_unrecognised_keys_are_ignored() {
        let lua = lua();
        let t = eval_table(&lua, r#"return { fg = "red", text = "hi", kind = "min" }"#);
        let s = style(&t).unwrap();
        assert_eq!(s.fg, Some(Color::Red));
    }

    // ── title ──────────────────────────────────────────────────────────

    #[test]
    fn title_string_becomes_single_span_line() {
        let lua = lua();
        let v = eval_value(&lua, r#"return "hello""#);
        let line = title(Some(v)).unwrap().expect("some line");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].text, "hello");
    }

    #[test]
    fn title_single_span_table() {
        let lua = lua();
        let v = eval_value(
            &lua,
            r#"return { text = "warn", fg = "yellow", bold = true }"#,
        );
        let line = title(Some(v)).unwrap().expect("some line");
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
            r#"return { { text = "[", fg = "grey" }, "info", { text = "]", fg = "grey" } }"#,
        );
        let line = title(Some(v)).unwrap().expect("some line");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].text, "info");
        assert_eq!(line.spans[1].style.fg, None);
    }

    #[test]
    fn title_empty_sequence_returns_none() {
        let lua = lua();
        let v = eval_value(&lua, "return {}");
        assert!(title(Some(v)).unwrap().is_none());
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
    fn border_table_with_partial_sides_legacy() {
        let lua = lua();
        let t = eval_table(
            &lua,
            r#"return { border = { style = "rounded", sides = { "top", "left" } } }"#,
        );
        let b = border(&t).unwrap().expect("some border");
        assert_eq!(b.style, BorderStyle::Rounded);
        assert!(b.top.is_some() && b.left.is_some());
        assert!(b.right.is_none() && b.bottom.is_none());
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
