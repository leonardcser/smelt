//! `smelt.*` binding setup. `LuaRuntime::register_api` wires one
//! Rust module per top-level `smelt.<name>` namespace, plus a few
//! cross-cutting top-level bindings (notify, clipboard, confirm)
//! and shared helpers (color / theme / json conversion).

mod au;
mod bash;
mod buf;
mod cell;
mod clipboard;
mod cmd;
mod confirm;
mod diff;
mod engine;
mod frontend;
mod fs;
mod fuzzy;
mod grep;
mod history;
mod html;
mod http;
mod image;
mod keymap;
mod mcp;
mod metrics;
mod mode;
mod model;
mod notebook;
mod os;
mod parse;
mod path;
mod permissions;
mod process;
mod prompt;
mod provider;
mod reasoning;
mod session;
mod settings;
mod shell;
mod skills;
mod spawn;
mod statusline;
mod syntax;
mod task;
mod theme;
mod timer;
mod tools;
mod transcript;
mod ui;
mod vim;
mod win;

use super::{LuaRuntime, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

/// Register a 0-arg getter that reads live state from `TuiApp` via
/// `try_with_app`. Replaces the old snapshot-mirror pattern — every
/// read goes through the TLS pointer installed at the top of each
/// tick / Lua-entry boundary.
///
/// Reads use `try_with_app` (not `with_app`) so callers from a context
/// without `install_app_ptr` get the type's `Default` instead of a
/// panic. In production every Lua-entry path installs the pointer, so
/// the fallback is dead; tests that exercise bindings without a `TuiApp`
/// get empty/zeroed values rather than panics.
macro_rules! app_read {
    ($lua:expr, |$app:ident| $body:expr) => {{
        $lua.create_function(
            |_, ()| Ok(crate::lua::try_with_app(|$app| $body).unwrap_or_default()),
        )?
    }};
}
pub(crate) use app_read;

impl LuaRuntime {
    pub(super) fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;
        let smelt_ui = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        smelt.set("version", crate::api::VERSION)?;

        transcript::register(lua, &smelt)?;
        engine::register(lua, &smelt, shared)?;
        mode::register(lua, &smelt)?;
        model::register(lua, &smelt)?;
        reasoning::register(lua, &smelt)?;
        session::register(lua, &smelt)?;
        process::register(lua, &smelt)?;
        shell::register(lua, &smelt)?;
        skills::register(lua, &smelt)?;
        permissions::register(lua, &smelt, shared)?;
        parse::register(lua, &smelt)?;
        path::register(lua, &smelt)?;
        fs::register(lua, &smelt)?;
        os::register(lua, &smelt)?;
        provider::register(lua, &smelt, shared)?;
        mcp::register(lua, &smelt, shared)?;
        frontend::register(lua, &smelt)?;
        fuzzy::register(lua, &smelt)?;
        grep::register(lua, &smelt)?;
        history::register(lua, &smelt)?;
        html::register(lua, &smelt)?;
        http::register(lua, &smelt)?;
        image::register(lua, &smelt)?;
        theme::register(lua, &smelt)?;
        buf::register(lua, &smelt, shared)?;
        win::register(lua, &smelt, shared)?;
        ui::register(lua, &smelt_ui)?;
        prompt::register(lua, &smelt)?;
        settings::register(lua, &smelt, shared)?;
        vim::register(lua, &smelt)?;
        cmd::register(lua, &smelt, shared)?;
        keymap::register(lua, &smelt_keymap, shared)?;
        task::register(lua, &smelt, shared)?;
        tools::register(lua, &smelt, shared)?;
        statusline::register(lua, &smelt, shared)?;
        timer::register(lua, &smelt)?;
        cell::register(lua, &smelt)?;
        au::register(lua, &smelt)?;
        spawn::register(lua, &smelt, shared)?;
        metrics::register(lua, &smelt)?;

        smelt.set(
            "notify",
            lua.create_function(|_, msg: String| {
                crate::lua::with_app(|app| app.notify(msg));
                Ok(())
            })?,
        )?;
        smelt.set(
            "notify_error",
            lua.create_function(|_, msg: String| {
                crate::lua::with_app(|app| app.notify_error(msg));
                Ok(())
            })?,
        )?;
        smelt.set(
            "quit",
            lua.create_function(|_, ()| {
                crate::lua::with_app(|app| app.pending_quit = true);
                Ok(())
            })?,
        )?;
        clipboard::register(lua, &smelt)?;

        smelt.set("ui", smelt_ui)?;
        smelt.set("keymap", smelt_keymap)?;

        // Renderer primitives — `smelt.{diff,syntax,bash,notebook}.render`
        // share `content::to_buffer::render_into_buffer`. Any plugin
        // can paint highlit content into a buffer it owns.
        diff::register(lua, &smelt)?;
        syntax::register(lua, &smelt)?;
        bash::register(lua, &smelt)?;
        notebook::register(lua, &smelt)?;
        // smelt.confirm.* primitives consumed by confirm.lua.
        confirm::register(lua, &smelt)?;

        lua.globals().set("smelt", smelt)?;

        super::load_bootstrap_chunks(lua)?;

        Ok(())
    }
}

// ── theme + color helpers ──────────────────────────────────────────────

/// Encode a `crossterm::style::Color` as a Lua table.
///
/// Shapes: `{ ansi = u8 }` for palette colors, `{ rgb = { r, g, b } }`
/// for truecolor, `{ named = "red" }` for the 16 legacy names.
pub(super) fn color_to_lua(lua: &Lua, color: crossterm::style::Color) -> LuaResult<mlua::Table> {
    use crossterm::style::Color;
    let t = lua.create_table()?;
    match color {
        Color::AnsiValue(v) => t.set("ansi", v)?,
        Color::Rgb { r, g, b } => {
            let rgb = lua.create_table()?;
            rgb.set("r", r)?;
            rgb.set("g", g)?;
            rgb.set("b", b)?;
            t.set("rgb", rgb)?;
        }
        Color::Reset => t.set("named", "reset")?,
        Color::Black => t.set("named", "black")?,
        Color::DarkGrey => t.set("named", "dark_grey")?,
        Color::Red => t.set("named", "red")?,
        Color::DarkRed => t.set("named", "dark_red")?,
        Color::Green => t.set("named", "green")?,
        Color::DarkGreen => t.set("named", "dark_green")?,
        Color::Yellow => t.set("named", "yellow")?,
        Color::DarkYellow => t.set("named", "dark_yellow")?,
        Color::Blue => t.set("named", "blue")?,
        Color::DarkBlue => t.set("named", "dark_blue")?,
        Color::Magenta => t.set("named", "magenta")?,
        Color::DarkMagenta => t.set("named", "dark_magenta")?,
        Color::Cyan => t.set("named", "cyan")?,
        Color::DarkCyan => t.set("named", "dark_cyan")?,
        Color::White => t.set("named", "white")?,
        Color::Grey => t.set("named", "grey")?,
    }
    Ok(t)
}

/// Project a `crossterm::style::Color` to an ANSI palette index for
/// the `statusline_item_from` decoder, which only reads `u8` fg/bg.
/// `Color::Reset` returns `None` (no override). Named legacy colors
/// map to the canonical 0..15 ANSI slots.
pub(super) fn color_to_ansi(color: crossterm::style::Color) -> Option<u8> {
    use crossterm::style::Color;
    match color {
        Color::AnsiValue(v) => Some(v),
        Color::Reset => None,
        Color::Black => Some(0),
        Color::DarkRed => Some(1),
        Color::DarkGreen => Some(2),
        Color::DarkYellow => Some(3),
        Color::DarkBlue => Some(4),
        Color::DarkMagenta => Some(5),
        Color::DarkCyan => Some(6),
        Color::Grey => Some(7),
        Color::DarkGrey => Some(8),
        Color::Red => Some(9),
        Color::Green => Some(10),
        Color::Yellow => Some(11),
        Color::Blue => Some(12),
        Color::Magenta => Some(13),
        Color::Cyan => Some(14),
        Color::White => Some(15),
        Color::Rgb { r, g, b } => Some(rgb_to_ansi256(r, g, b)),
    }
}

/// Approximate an RGB triple to the nearest ANSI 256 palette entry
/// using the standard 16 + 6×6×6 cube + 24-step grayscale layout.
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return 232 + ((r - 8) / 10);
    }
    let to_cube = |c: u8| -> u8 {
        if c < 48 {
            0
        } else if c < 115 {
            1
        } else {
            ((c - 35) / 40).min(5)
        }
    };
    16 + 36 * to_cube(r) + 6 * to_cube(g) + to_cube(b)
}

/// Decode a Lua color table to an ANSI palette index. Accepts
/// `{ ansi = u8 }`, `{ preset = "name" }`, or `{ rgb = { r, g, b } }`
/// (rgb is down-sampled via the nearest-palette approximation).
pub(super) fn color_ansi_from_lua(table: &mlua::Table) -> LuaResult<u8> {
    if let Ok(v) = table.get::<u8>("ansi") {
        return Ok(v);
    }
    if let Ok(name) = table.get::<String>("preset") {
        return crate::theme::preset_by_name(&name)
            .ok_or_else(|| LuaError::RuntimeError(format!("unknown preset: {name}")));
    }
    if let Ok(rgb) = table.get::<mlua::Table>("rgb") {
        let r: u8 = rgb.get("r")?;
        let g: u8 = rgb.get("g")?;
        let b: u8 = rgb.get("b")?;
        return Ok(rgb_to_ansi_256(r, g, b));
    }
    Err(LuaError::RuntimeError(
        "color table must have one of: ansi, preset, rgb".into(),
    ))
}

/// Nearest 6×6×6 palette index for an sRGB triple.
fn rgb_to_ansi_256(r: u8, g: u8, b: u8) -> u8 {
    fn band(c: u8) -> u8 {
        let levels = [0u8, 95, 135, 175, 215, 255];
        levels
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| (c as i32 - **v as i32).abs())
            .map(|(i, _)| i as u8)
            .unwrap_or(0)
    }
    16 + 36 * band(r) + 6 * band(g) + band(b)
}

/// Map a Lua-facing role name to its `::ui::Theme` highlight group.
fn role_to_group(role: &str) -> Option<&'static str> {
    Some(match role {
        "accent" => "SmeltAccent",
        "slug" => "SmeltSlug",
        "user_bg" => "SmeltUserBg",
        "code_block_bg" => "SmeltCodeBlockBg",
        "bar" => "SmeltBar",
        "tool_pending" => "SmeltToolPending",
        "reason_off" => "SmeltReasonOff",
        "muted" => "Comment",
        _ => return None,
    })
}

/// Resolved color for a `::ui::Theme` highlight group: prefer fg, then
/// bg, then `Color::Reset`. Matches the convention used by
/// `to_buffer::resolve` for `ColorRole` lookups.
fn group_color(theme: &::ui::Theme, group: &str) -> crossterm::style::Color {
    let style = theme.get(group);
    style
        .fg
        .or(style.bg)
        .unwrap_or(crossterm::style::Color::Reset)
}

/// Read a named theme role from `theme`. Returns `None` for unknown names.
pub(super) fn theme_role_get(theme: &::ui::Theme, role: &str) -> Option<crossterm::style::Color> {
    role_to_group(role).map(|g| group_color(theme, g))
}

/// Set a writable theme role on `theme`. Only `accent` and `slug` are
/// mutable. Caller must `populate_ui_theme` afterwards (or wait for
/// the next frame's render-loop bridge) to flush the new value into
/// the corresponding highlight group.
pub(super) fn theme_role_set(theme: &mut ::ui::Theme, role: &str, ansi: u8) -> LuaResult<()> {
    match role {
        "accent" => {
            theme.set_accent(ansi);
            crate::theme::populate_ui_theme(theme);
            Ok(())
        }
        "slug" => {
            theme.set_slug(ansi);
            crate::theme::populate_ui_theme(theme);
            Ok(())
        }
        other => Err(LuaError::RuntimeError(format!(
            "theme role is read-only: {other}"
        ))),
    }
}

/// List of (role_name, current_color) pairs for `theme.snapshot()`.
pub(super) fn theme_snapshot_pairs(
    theme: &::ui::Theme,
) -> Vec<(&'static str, crossterm::style::Color)> {
    [
        "accent",
        "slug",
        "user_bg",
        "code_block_bg",
        "bar",
        "tool_pending",
        "reason_off",
        "muted",
    ]
    .into_iter()
    .map(|role| {
        let group = role_to_group(role).expect("known role");
        (role, group_color(theme, group))
    })
    .collect()
}

/// Convert a Lua table to a `serde_json::Value`. Tables with contiguous
/// 1..N integer keys become JSON arrays; anything else becomes an object.
pub(super) fn lua_table_to_json(lua: &Lua, table: &mlua::Table) -> serde_json::Value {
    let mut pairs: Vec<(mlua::Value, mlua::Value)> = Vec::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let Ok(kv) = pair else { continue };
        pairs.push(kv);
    }

    let is_array = !pairs.is_empty()
        && pairs
            .iter()
            .all(|(k, _)| matches!(k, mlua::Value::Integer(_)))
        && {
            let mut ints: Vec<i64> = pairs
                .iter()
                .filter_map(|(k, _)| match k {
                    mlua::Value::Integer(i) => Some(*i),
                    _ => None,
                })
                .collect();
            ints.sort_unstable();
            ints.first().copied() == Some(1) && ints.windows(2).all(|w| w[1] == w[0] + 1)
        };

    if is_array || pairs.is_empty() {
        let len = table.raw_len();
        let mut arr = Vec::with_capacity(len);
        for i in 1..=len {
            let val: mlua::Value = table.raw_get(i).unwrap_or(mlua::Value::Nil);
            arr.push(lua_value_to_json(lua, &val));
        }
        serde_json::Value::Array(arr)
    } else {
        let mut map = serde_json::Map::new();
        for (key, val) in pairs {
            let key_str = match &key {
                mlua::Value::String(s) => s.to_string_lossy().to_string(),
                mlua::Value::Integer(i) => i.to_string(),
                _ => continue,
            };
            map.insert(key_str, lua_value_to_json(lua, &val));
        }
        serde_json::Value::Object(map)
    }
}

fn lua_value_to_json(lua: &Lua, val: &mlua::Value) -> serde_json::Value {
    match val {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::json!(*i),
        mlua::Value::Number(n) => serde_json::json!(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        mlua::Value::Table(t) => lua_table_to_json(lua, t),
        _ => serde_json::Value::Null,
    }
}

/// Convert a `serde_json::Value` into a Lua value. Objects become
/// tables keyed by string; arrays become 1-indexed sequences. Used by
/// FFI bindings that surface JSON-shaped metadata back to Lua.
pub(super) fn json_to_lua_value(lua: &Lua, value: &serde_json::Value) -> LuaResult<mlua::Value> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => mlua::Value::Nil,
        J::Bool(b) => mlua::Value::Boolean(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                mlua::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                mlua::Value::Number(f)
            } else {
                mlua::Value::Nil
            }
        }
        J::String(s) => mlua::Value::String(lua.create_string(s)?),
        J::Array(arr) => {
            let t = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                t.set(i + 1, json_to_lua_value(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
        J::Object(obj) => {
            let t = lua.create_table()?;
            for (k, v) in obj {
                t.set(k.as_str(), json_to_lua_value(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
    })
}

/// Treat a Lua table as a `{ string => json }` arg map, the shape every
/// tool call accepts. Skips non-string keys.
pub(super) fn lua_table_to_args(
    lua: &Lua,
    table: &mlua::Table,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut out = std::collections::HashMap::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>().flatten() {
        let (k, v) = pair;
        let key = match k {
            mlua::Value::String(s) => s.to_string_lossy().to_string(),
            _ => continue,
        };
        out.insert(key, lua_value_to_json(lua, &v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> ::ui::Theme {
        let mut t = ::ui::Theme::new();
        crate::theme::populate_ui_theme(&mut t);
        t
    }

    #[test]
    fn theme_role_get_known_roles() {
        let t = theme();
        for role in [
            "accent",
            "slug",
            "user_bg",
            "code_block_bg",
            "bar",
            "tool_pending",
            "reason_off",
            "muted",
        ] {
            assert!(
                theme_role_get(&t, role).is_some(),
                "expected color for {role}"
            );
        }
    }

    #[test]
    fn theme_role_get_unknown_returns_none() {
        let t = theme();
        assert!(theme_role_get(&t, "bogus").is_none());
    }

    #[test]
    fn theme_role_set_accent_round_trips() {
        let mut t = theme();
        theme_role_set(&mut t, "accent", 42).unwrap();
        assert_eq!(t.accent(), 42);
        // The SmeltAccent group is rebuilt on set.
        assert_eq!(
            t.get("SmeltAccent").fg,
            Some(crossterm::style::Color::AnsiValue(42))
        );
    }

    #[test]
    fn theme_role_set_preset_via_color_decode() {
        // sage = 108 in PRESETS
        let v = crate::theme::preset_by_name("sage").unwrap();
        let mut t = theme();
        theme_role_set(&mut t, "accent", v).unwrap();
        assert_eq!(t.accent(), 108);
    }

    #[test]
    fn theme_role_set_read_only_errors() {
        let mut t = theme();
        let err = theme_role_set(&mut t, "muted", 1).unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    #[test]
    fn theme_snapshot_pairs_lists_all_roles() {
        let t = theme();
        let pairs = theme_snapshot_pairs(&t);
        let names: Vec<&str> = pairs.iter().map(|(n, _)| *n).collect();
        for expected in [
            "accent",
            "bar",
            "code_block_bg",
            "muted",
            "reason_off",
            "slug",
            "tool_pending",
            "user_bg",
        ] {
            assert!(
                names.contains(&expected),
                "snapshot missing {expected}: {names:?}"
            );
        }
    }
}
