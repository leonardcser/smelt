//! UiHost-tier Lua API bindings — require a terminal UI context.
//!
//! Host-tier bindings live in `smelt_core::lua::api` and are registered
//! first via `register_host_api`; this module registers the UiHost-tier
//! namespaces on top.

mod bash;
mod buf;
mod confirm;
mod diff;
mod engine;
mod history;
mod keymap;
mod metrics;
mod model;
mod notebook;
mod permissions;
mod prompt;
mod session;
mod settings;
mod statusline;
mod syntax;
mod theme;
mod transcript;
mod ui;
mod vim;
mod win;

use super::{LuaRuntime, LuaShared};
use mlua::prelude::*;
use std::sync::Arc;

/// Register a 0-arg getter that reads live state from `TuiApp` via
/// `try_with_app`. Returns a Lua function that, when called, invokes
/// `try_with_app` and returns the closure result (or `Default`).
macro_rules! app_read {
    ($lua:expr, |$app:ident| $body:expr) => {{
        $lua.create_function(
            |_, ()| Ok(crate::lua::try_with_app(|$app| $body).unwrap_or_default()),
        )?
    }};
}
pub(crate) use app_read;

pub(crate) use smelt_core::lua::json_to_lua as json_to_lua_value;

impl LuaRuntime {
    pub(super) fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;
        let smelt_ui = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        smelt.set("version", crate::api::VERSION)?;

        // Host-tier bindings (registered by core)
        smelt_core::lua::api::register_host_api(lua, &smelt, &smelt_keymap, &shared.core)?;

        // UiHost-tier bindings
        buf::register(lua, &smelt, shared)?;
        win::register(lua, &smelt, shared)?;
        self::ui::register(lua, &smelt_ui)?;
        prompt::register(lua, &smelt)?;
        theme::register(lua, &smelt)?;
        statusline::register(lua, &smelt, shared)?;
        confirm::register(lua, &smelt)?;
        notebook::register(lua, &smelt)?;
        diff::register(lua, &smelt)?;
        syntax::register(lua, &smelt)?;
        bash::register(lua, &smelt)?;
        engine::register(lua, &smelt, shared)?;
        history::register(lua, &smelt)?;
        keymap::register(lua, &smelt_keymap, shared)?;
        metrics::register(lua, &smelt)?;
        model::register(lua, &smelt)?;
        permissions::register(lua, &smelt, shared)?;
        session::register(lua, &smelt)?;
        settings::register(lua, &smelt, shared)?;
        transcript::register(lua, &smelt)?;
        vim::register(lua, &smelt)?;

        smelt.set("ui", smelt_ui)?;
        smelt.set("keymap", smelt_keymap)?;

        // Cross-cutting bindings that need TuiApp
        let cmd_tbl: mlua::Table = smelt.get("cmd")?;
        cmd_tbl.set(
            "run",
            lua.create_function(|_, line: String| {
                crate::lua::with_app(|app| app.apply_lua_command(&line));
                Ok(())
            })?,
        )?;
        let mode_tbl: mlua::Table = smelt.get("mode")?;
        mode_tbl.set(
            "set",
            lua.create_function(|_, v: String| {
                crate::lua::with_app(|app| match protocol::AgentMode::parse(&v) {
                    Some(mode) => app.set_mode(mode),
                    None => app.notify_error(format!("unknown mode: {v}")),
                });
                Ok(())
            })?,
        )?;
        let reasoning_tbl: mlua::Table = smelt.get("reasoning")?;
        reasoning_tbl.set(
            "set",
            lua.create_function(|_, v: String| {
                crate::lua::with_app(|app| match protocol::ReasoningEffort::parse(&v) {
                    Some(effort) => app.set_reasoning_effort(effort),
                    None => app.notify_error(format!("unknown reasoning effort: {v}")),
                });
                Ok(())
            })?,
        )?;
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

        lua.globals().set("smelt", smelt)?;

        smelt_core::lua::runtime::load_bootstrap_chunks(lua)?;

        Ok(())
    }
}

// ── theme + color helpers ──────────────────────────────────────────────

/// Encode a `smelt_core::style::Color` as a Lua table.
///
/// Shapes: `{ ansi = u8 }` for palette colors, `{ rgb = { r, g, b } }`
/// for truecolor, `{ named = "red" }` for the 16 legacy names.
pub(super) fn color_to_lua(lua: &Lua, color: smelt_core::style::Color) -> LuaResult<mlua::Table> {
    use smelt_core::style::Color;
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

/// Project a `smelt_core::style::Color` to an ANSI palette index for
/// the `statusline_item_from` decoder, which only reads `u8` fg/bg.
/// `Color::Reset` returns `None` (no override). Named legacy colors
/// map to the canonical 0..15 ANSI slots.
pub(super) fn color_to_ansi(color: smelt_core::style::Color) -> Option<u8> {
    use smelt_core::style::Color;
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

/// Map a Lua-facing role name to its `crate::ui::Theme` highlight group.
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

/// Resolved color for a `crate::ui::Theme` highlight group: prefer fg, then
/// bg, then `Color::Reset`. Matches the convention used by
/// `to_buffer::resolve` for `ColorRole` lookups.
fn group_color(theme: &crate::ui::Theme, group: &str) -> smelt_core::style::Color {
    let style = theme.get(group);
    style
        .fg
        .or(style.bg)
        .unwrap_or(smelt_core::style::Color::Reset)
}

/// Read a named theme role from `theme`. Returns `None` for unknown names.
pub(super) fn theme_role_get(
    theme: &crate::ui::Theme,
    role: &str,
) -> Option<smelt_core::style::Color> {
    role_to_group(role).map(|g| group_color(theme, g))
}

/// Set a writable theme role on `theme`. Only `accent` and `slug` are
/// mutable. Caller must `populate_ui_theme` afterwards (or wait for
/// the next frame's render-loop bridge) to flush the new value into
/// the corresponding highlight group.
pub(super) fn theme_role_set(theme: &mut crate::ui::Theme, role: &str, ansi: u8) -> LuaResult<()> {
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
    theme: &crate::ui::Theme,
) -> Vec<(&'static str, smelt_core::style::Color)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> crate::ui::Theme {
        let mut t = crate::ui::Theme::new();
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
            Some(smelt_core::style::Color::AnsiValue(42))
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
