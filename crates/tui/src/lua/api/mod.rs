//! UiHost-tier Lua API bindings (require a terminal UI context).
//! Host-tier bindings are registered first via `smelt_core::lua::api::register_host_api`.

mod buf;
mod confirm;
mod engine;
mod history;
mod keymap;
mod metrics;
mod model;
mod notebook;
mod overlay;
mod paint;
mod permissions;
mod picker;
mod prompt;
mod render;
mod session;
mod settings;
mod statusline;
mod text;
mod theme;
mod transcript;
mod ui;
pub(crate) mod ui_layout;
pub(crate) mod vim;
pub(crate) mod win;

use super::{LuaRuntime, LuaShared};
use mlua::prelude::*;
use smelt_core::lua::api::mode::LuaAgentMode;
use smelt_core::lua::api::reasoning::LuaReasoningEffort;
use smelt_core::lua::doc::{record_module_doc, register_ui_fn};
use std::sync::Arc;

/// Semantic version of the Lua API surface, exposed as `smelt.version`.
/// Increments on breaking changes; additive changes do not bump it.
pub(crate) const VERSION: &str = "1";

pub(crate) use smelt_core::lua::json_to_lua as json_to_lua_value;

impl LuaRuntime {
    pub(super) fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;
        let smelt_ui = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        smelt.set("version", VERSION)?;
        record_module_doc("smelt", "Root smelt namespace. Host-tier bindings are registered first; UiHost-tier bindings are injected when a TUI is active.");

        smelt_core::lua::api::register_host_api(lua, &smelt, &smelt_keymap, &shared.core)?;

        // UiHost-tier bindings
        buf::register(lua, &smelt, shared)?;
        win::register(lua, &smelt, shared)?;
        overlay::register(lua, &smelt)?;
        picker::register(lua, &smelt)?;
        self::ui::register(lua, &smelt_ui)?;
        prompt::register(lua, &smelt)?;
        theme::register(lua, &smelt)?;
        statusline::register(lua, &smelt, shared)?;
        confirm::register(lua, &smelt)?;
        notebook::register(lua, &smelt)?;
        paint::register(lua, &smelt, shared)?;
        render::register(lua, &smelt)?;
        text::register(lua, &smelt)?;
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

        smelt.set("keymap", smelt_keymap)?;

        // Override `smelt.layout.leaf` so it accepts a `Buf` userdata in
        // addition to the raw `u64` the host-tier registration accepts.
        // Tools' `render` callbacks now own Buf handles, not numeric ids.
        let layout_tbl: mlua::Table = smelt.get("layout")?;
        register_ui_fn(
            &layout_tbl,
            "smelt.layout",
            "leaf",
            "Wrap a `Buf` handle (or raw buf id) into a leaf block layout that renders the buffer's contents in place.",
            &["buf"],
            lua,
            |_, buf: mlua::Value| -> LuaResult<smelt_core::lua::api::layout::LuaBlockLayout> {
                let id = match buf {
                    mlua::Value::Integer(n) => smelt_core::buffer::BufId(n as u64),
                    mlua::Value::UserData(ud) => ud.borrow::<buf::LuaBuf>()?.id,
                    other => {
                        return Err(mlua::Error::FromLuaConversionError {
                            from: other.type_name(),
                            to: "smelt.buf.Buf or integer".into(),
                            message: Some(
                                "smelt.layout.leaf: expected a Buf userdata or integer".into(),
                            ),
                        });
                    }
                };
                Ok(smelt_core::lua::api::layout::LuaBlockLayout(
                    smelt_core::content::block_layout::BlockLayout::Leaf(
                        smelt_core::content::block_layout::LuaLeaf::Buf(id),
                    ),
                ))
            },
        )?;

        // Cross-cutting UiHost-tier additions to host modules.
        let cmd_tbl: mlua::Table = smelt.get("cmd")?;
        register_ui_fn(
            &cmd_tbl,
            "smelt.cmd",
            "run",
            "Execute the slash-command line `line` (with or without leading `/`) as if the user had typed it. Errors are surfaced as in-app notifications.",
            &["line"],
            lua,
            |_, line: String|  -> LuaResult<()>{
                crate::lua::with_app(|app| app.apply_lua_command(&line));
                Ok(())
            },
        )?;
        // Replace the host-tier `__call` no-op stub on `smelt.mode` /
        // `smelt.reasoning` with the live setter, so `smelt.mode("plan")`
        // and `smelt.reasoning("high")` actually take effect.
        install_selector_call(
            lua,
            &smelt,
            "mode",
            |_, mode: LuaAgentMode| {
                crate::lua::with_app(|app| app.set_mode(mode.into()));
                Ok(())
            },
            |lua, ()| -> LuaResult<mlua::Value> {
                let cur = crate::lua::try_with_app(|app| LuaAgentMode::from(app.core.config.mode))
                    .unwrap_or(LuaAgentMode::Normal);
                cur.into_lua(lua)
            },
        )?;
        install_selector_call(
            lua,
            &smelt,
            "reasoning",
            |_, effort: LuaReasoningEffort| {
                crate::lua::with_app(|app| app.set_reasoning_effort(effort.into()));
                Ok(())
            },
            |lua, ()| -> LuaResult<mlua::Value> {
                let cur = crate::lua::try_with_app(|app| {
                    LuaReasoningEffort::from(app.core.config.reasoning_effort)
                })
                .unwrap_or(LuaReasoningEffort::Medium);
                cur.into_lua(lua)
            },
        )?;
        register_ui_fn(
            &smelt_ui,
            "smelt.ui",
            "notify",
            "Show an informational notification in the status area.",
            &["msg"],
            lua,
            |_, msg: String| -> LuaResult<()> {
                crate::lua::with_app(|app| app.notify(msg));
                Ok(())
            },
        )?;
        register_ui_fn(
            &smelt_ui,
            "smelt.ui",
            "notify_error",
            "Show an error notification in the status area (highlighted with the error color).",
            &["msg"],
            lua,
            |_, msg: String| -> LuaResult<()> {
                crate::lua::with_app(|app| app.notify_error(msg));
                Ok(())
            },
        )?;
        register_ui_fn(
            &smelt,
            "smelt",
            "ns",
            "Look up or allocate a stable namespace id for `name`. Namespaces scope `buf:mark` / `buf:clear_ns` calls so plugins can repaint their region without disturbing others.",
            &["name"],
            lua,
            |_, name: String| -> LuaResult<u32> {
                Ok(smelt_core::buffer::create_namespace(&name).0)
            },
        )?;
        register_ui_fn(
            &smelt,
            "smelt",
            "focus",
            "Return which top-level pane currently has focus: `\"transcript\"` or `\"prompt\"`.",
            &[],
            lua,
            |_, ()| -> LuaResult<String> {
                Ok(crate::lua::try_with_app(|app| match app.app_focus {
                    crate::app::AppFocus::Content => "transcript".to_string(),
                    crate::app::AppFocus::Prompt => "prompt".to_string(),
                })
                .unwrap_or_default())
            },
        )?;
        register_ui_fn(
            &smelt,
            "smelt",
            "quit",
            "Request a clean shutdown of the app. The quit fires on the next tick after the current handler returns.",
            &[],
            lua,
            |_, ()|  -> LuaResult<()>{
                crate::lua::with_app(|app| app.pending_quit = true);
                Ok(())
            },
        )?;

        smelt.set("ui", smelt_ui)?;

        lua.globals().set("smelt", smelt)?;

        smelt_core::lua::runtime::load_bootstrap_chunks(lua)?;

        Ok(())
    }
}

/// Wire a callable selector module: when `smelt.<name>(v)` is called
/// with a value, `set(v)` runs; with no arg, `get()` returns the
/// current value. Replaces the `__call` stub the host-tier registration
/// installed.
fn install_selector_call<T, S, G>(
    lua: &Lua,
    smelt: &mlua::Table,
    name: &'static str,
    set: S,
    get: G,
) -> LuaResult<()>
where
    T: 'static + FromLua,
    S: Fn(&Lua, T) -> LuaResult<()> + 'static,
    G: Fn(&Lua, ()) -> LuaResult<mlua::Value> + 'static,
{
    let tbl: mlua::Table = smelt.get(name)?;
    let call = lua.create_function(
        move |lua, (_tbl, v): (mlua::Table, Option<T>)| -> LuaResult<mlua::Value> {
            match v {
                Some(value) => {
                    set(lua, value)?;
                    Ok(mlua::Value::Nil)
                }
                None => get(lua, ()),
            }
        },
    )?;
    let mt = tbl
        .metatable()
        .unwrap_or_else(|| lua.create_table().unwrap());
    mt.set("__call", call)?;
    tbl.set_metatable(Some(mt))?;
    Ok(())
}

// ── theme + color helpers ──────────────────────────────────────────────

/// Encode a `Color` as a Lua table: `{ansi=u8}`, `{rgb={r,g,b}}`, or `{named="red"}`.
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

/// Project a `Color` to an ANSI palette index. `Color::Reset` → `None`.
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

/// Approximate an RGB triple to the nearest ANSI 256 palette entry.
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

/// Decode a Lua color table (`{ansi=u8}`, `{preset="name"}`, or `{rgb={r,g,b}}`) to ANSI index.
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

/// Nearest ANSI 256-color index for an sRGB triple.
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

/// Resolved color for a highlight group: fg preferred, then bg, then `Color::Reset`.
pub(super) fn group_color(
    theme: &crate::smelt_term::Theme,
    group: &str,
) -> smelt_core::style::Color {
    let style = theme.get(group);
    style
        .fg
        .or(style.bg)
        .unwrap_or(smelt_core::style::Color::Reset)
}

/// Set highlight group `group` to fg = `Color::AnsiValue(ansi)`. For the two
/// "palette" groups (`SmeltAccent`, `SmeltSlug`), bumps the corresponding ANSI
/// index on the `Theme` and re-runs `populate_ui_theme` so derived groups
/// follow. Any other group is set in place — only the fg moves.
pub(super) fn theme_set(theme: &mut crate::smelt_term::Theme, group: &str, ansi: u8) {
    match group {
        "SmeltAccent" => {
            theme.set_accent(ansi);
            crate::theme::populate_ui_theme(theme);
        }
        "SmeltSlug" => {
            theme.set_slug(ansi);
            crate::theme::populate_ui_theme(theme);
        }
        other => {
            theme.set(
                other,
                smelt_core::style::Style::new().fg(smelt_core::style::Color::AnsiValue(ansi)),
            );
        }
    }
}

/// Well-known smelt highlight groups, in the order `theme.snapshot()` reports them.
pub(super) const SMELT_GROUPS: &[&str] = &[
    "SmeltAccent",
    "SmeltSlug",
    "SmeltUserBg",
    "SmeltCodeBlockBg",
    "SmeltBar",
    "SmeltToolPending",
    "SmeltReasonOff",
    "Comment",
];

/// List of (group_name, current_color) pairs for `theme.snapshot()`.
pub(super) fn theme_snapshot_pairs(
    theme: &crate::smelt_term::Theme,
) -> Vec<(&'static str, smelt_core::style::Color)> {
    SMELT_GROUPS
        .iter()
        .map(|g| (*g, group_color(theme, g)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> crate::smelt_term::Theme {
        let mut t = crate::smelt_term::Theme::new();
        crate::theme::populate_ui_theme(&mut t);
        t
    }

    #[test]
    fn group_color_known_groups_return_color() {
        let t = theme();
        for g in SMELT_GROUPS {
            // All built-in smelt groups are populated by `populate_ui_theme`;
            // none should fall through to `Color::Reset`.
            assert_ne!(
                group_color(&t, g),
                smelt_core::style::Color::Reset,
                "expected populated color for {g}"
            );
        }
    }

    #[test]
    fn group_color_unknown_returns_reset() {
        let t = theme();
        assert_eq!(group_color(&t, "Bogus"), smelt_core::style::Color::Reset);
    }

    #[test]
    fn theme_set_smelt_accent_rebuilds_palette() {
        let mut t = theme();
        theme_set(&mut t, "SmeltAccent", 42);
        assert_eq!(t.accent(), 42);
        assert_eq!(
            t.get("SmeltAccent").fg,
            Some(smelt_core::style::Color::AnsiValue(42))
        );
    }

    #[test]
    fn theme_set_smelt_accent_via_preset_decode() {
        // sage maps to ANSI 108.
        let v = crate::theme::preset_by_name("sage").unwrap();
        let mut t = theme();
        theme_set(&mut t, "SmeltAccent", v);
        assert_eq!(t.accent(), 108);
    }

    #[test]
    fn theme_set_arbitrary_group_only_moves_fg() {
        let mut t = theme();
        let accent_before = t.accent();
        theme_set(&mut t, "Comment", 99);
        assert_eq!(
            t.get("Comment").fg,
            Some(smelt_core::style::Color::AnsiValue(99))
        );
        // Accent palette index is untouched when setting a non-palette group.
        assert_eq!(t.accent(), accent_before);
    }

    #[test]
    fn theme_snapshot_pairs_lists_all_groups() {
        let t = theme();
        let pairs = theme_snapshot_pairs(&t);
        let names: Vec<&str> = pairs.iter().map(|(n, _)| *n).collect();
        for expected in SMELT_GROUPS {
            assert!(
                names.contains(expected),
                "snapshot missing {expected}: {names:?}"
            );
        }
    }
}
