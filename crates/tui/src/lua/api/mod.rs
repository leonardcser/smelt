//! UiHost-tier Lua API bindings (require a terminal UI context).
//! Host-tier bindings are registered first via `smelt_core::lua::api::register_host_api`.

mod buf;
mod confirm;
mod engine;
mod history;
mod keymap;
mod log;
mod metrics;
mod model;
mod notebook;
mod notify;
mod overlay;
pub(crate) mod overlay_layout;
mod paint;
mod permissions;
mod picker;
mod prompt;
mod render;
mod session;
mod settings;
mod spinner;
mod statusline;
mod text;
pub(crate) mod theme;
mod transcript;
pub(crate) mod vim;
pub(crate) mod win;

use super::{LuaRuntime, LuaShared};
use mlua::prelude::*;
use smelt_core::lua::api::mode::LuaAgentMode;
use smelt_core::lua::api::reasoning::LuaReasoningEffort;
use smelt_core::lua::doc::{record_module_doc, Tier};
use smelt_core::lua::module::LuaMod;
use std::sync::Arc;

/// Schema version of the Lua API surface, exposed as `smelt.api_version`.
/// Increments on breaking changes; additive changes do not bump it.
pub(crate) const API_VERSION: &str = "1";

/// Build identity, exposed as `smelt.build`. Versions are sourced from
/// `CARGO_PKG_VERSION` so the workspace's `0.x.y` version stays in
/// lockstep with what plugins see; sha/date/target come from the build
/// script (`build.rs`).
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const BUILD_SHA: &str = env!("SMELT_BUILD_SHA");
pub(crate) const BUILD_DATE: &str = env!("SMELT_BUILD_DATE");
pub(crate) const BUILD_TARGET: &str = env!("SMELT_TARGET");

pub(crate) use smelt_core::lua::json_to_lua as json_to_lua_value;

/// `"unknown"` collapses to Lua nil so plugins can branch on `smelt.build.sha == nil`
/// rather than string-matching a sentinel.
fn optional_str(s: &str) -> Option<&str> {
    if s.is_empty() || s == "unknown" {
        None
    } else {
        Some(s)
    }
}

impl LuaRuntime {
    pub(super) fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        let build = lua.create_table()?;
        build.set("version", APP_VERSION)?;
        build.set("sha", optional_str(BUILD_SHA))?;
        build.set("date", optional_str(BUILD_DATE))?;
        build.set("target", BUILD_TARGET)?;
        smelt.set("build", build)?;
        smelt.set("api_version", API_VERSION)?;
        record_module_doc("smelt", "Root smelt namespace. Host-tier bindings are registered first; UiHost-tier bindings are injected when a TUI is active.");
        record_module_doc("smelt.build", "Compile-time build identity: `version` (CARGO_PKG_VERSION), `sha` (short git commit or nil), `date` (committer ISO timestamp or nil), `target` (Rust target triple).");
        record_module_doc("smelt.tick", "Reload-safe periodic work. Subscribes to the host's one-second `now` cell and throttles your callback to a fixed interval — safe to call from plugin module bodies. Use this for recurring polling; reserve `smelt.timer.every` for transient timers armed by user actions.");

        smelt_core::lua::api::register_host_api(lua, &smelt, &smelt_keymap, &shared.core)?;

        // UiHost-tier bindings
        buf::register(lua, &smelt, shared)?;
        win::register(lua, &smelt, shared)?;
        overlay::register(lua, &smelt)?;
        picker::register(lua, &smelt)?;
        notify::register(lua, &smelt)?;
        spinner::register(lua, &smelt)?;
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
        log::register(lua, &smelt)?;
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
        LuaMod::extend(lua, layout_tbl, "smelt.layout", Tier::UiHost).fn_(
            "leaf",
            "Wrap a `Buf` handle (or raw buf id) into a leaf block layout that renders the buffer's contents in place.",
            &["buf"],
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
        LuaMod::extend(lua, cmd_tbl, "smelt.cmd", Tier::UiHost).fn_(
            "run",
            "Execute the slash-command line `line` (with or without leading `/`) as if the user had typed it. Errors are surfaced as in-app notifications.",
            &["line"],
            |_, line: String| -> LuaResult<()> {
                crate::lua::with_app(|app| app.apply_lua_command(&line));
                Ok(())
            },
        )?;
        // Replace the host-tier `__call` no-op stub on `smelt.mode` /
        // `smelt.reasoning` with the live setter, so `smelt.mode("plan")`
        // and `smelt.reasoning("high")` actually take effect.
        LuaMod::extend(lua, smelt.get("mode")?, "smelt.mode", Tier::UiHost).callable(
            |lua, (_tbl, v): (mlua::Table, Option<LuaAgentMode>)| -> LuaResult<mlua::Value> {
                if let Some(mode) = v {
                    crate::lua::with_app(|app| app.set_mode(mode.into(), true));
                    return Ok(mlua::Value::Nil);
                }
                let cur = crate::lua::try_with_app(|app| LuaAgentMode::from(app.core.config.mode))
                    .unwrap_or(LuaAgentMode::Normal);
                cur.into_lua(lua)
            },
        )?;
        LuaMod::extend(
            lua,
            smelt.get("reasoning")?,
            "smelt.reasoning",
            Tier::UiHost,
        )
        .callable(
            |lua, (_tbl, v): (mlua::Table, Option<LuaReasoningEffort>)| -> LuaResult<mlua::Value> {
                if let Some(effort) = v {
                    crate::lua::with_app(|app| app.set_reasoning_effort(effort.into(), true));
                    return Ok(mlua::Value::Nil);
                }
                let cur = crate::lua::try_with_app(|app| {
                    LuaReasoningEffort::from(app.core.config.reasoning_effort)
                })
                .unwrap_or(LuaReasoningEffort::Medium);
                cur.into_lua(lua)
            },
        )?;
        let smelt_root = LuaMod::extend(lua, smelt.clone(), "smelt", Tier::UiHost);
        smelt_root.fn_(
            "ns",
            "Look up or allocate a stable namespace id for `name`. Namespaces scope `buf:mark` / `buf:clear_ns` calls so plugins can repaint their region without disturbing others.",
            &["name"],
            |_, name: String| -> LuaResult<u32> {
                Ok(smelt_core::buffer::create_namespace(&name).0)
            },
        )?;
        smelt_root.fn_(
            "focus",
            "Return which top-level pane currently has focus: `\"transcript\"` or `\"prompt\"`.",
            &[],
            |_, ()| -> LuaResult<String> {
                Ok(crate::lua::try_with_app(|app| match app.app_focus {
                    crate::app::AppFocus::Content => "transcript".to_string(),
                    crate::app::AppFocus::Prompt => "prompt".to_string(),
                })
                .unwrap_or_default())
            },
        )?;
        smelt_root.fn_(
            "quit",
            "Request a clean shutdown of the app. The quit fires on the next tick after the current handler returns.",
            &[],
            |_, ()| -> LuaResult<()> {
                crate::lua::with_app(|app| app.pending_quit = true);
                Ok(())
            },
        )?;

        lua.globals().set("smelt", smelt)?;

        smelt_core::lua::runtime::load_bootstrap_chunks(lua)?;

        Ok(())
    }
}

// ── color projection helpers ──────────────────────────────────────────

/// Project a `Color` to an ANSI 256 palette index. `Color::Reset` → `None`.
/// The 16 named colors map to their canonical ANSI slots; RGB triples are
/// snapped to the nearest 6×6×6 cube entry. Used by call sites that ship
/// colors out to Lua (e.g. `statusline.snapshot`) where mlua tables are
/// keyed by ANSI index.
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

/// Nearest ANSI 256-color index for an sRGB triple. Uses the canonical
/// xterm 6×6×6 cube (0/95/135/175/215/255) plus the 24-step greyscale
/// ramp.
pub(super) fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return 232 + ((r - 8) / 10);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::style::Color;

    #[test]
    fn color_to_ansi_named_round_trips() {
        assert_eq!(color_to_ansi(Color::Red), Some(9));
        assert_eq!(color_to_ansi(Color::White), Some(15));
        assert_eq!(color_to_ansi(Color::Reset), None);
        assert_eq!(color_to_ansi(Color::AnsiValue(208)), Some(208));
    }

    #[test]
    fn rgb_to_ansi256_greyscale_ramp() {
        // Mid-grey snaps into the 232-255 ramp.
        let v = rgb_to_ansi256(128, 128, 128);
        assert!((232..=255).contains(&v), "got {v}");
    }

    #[test]
    fn rgb_to_ansi256_pure_red_hits_cube_corner() {
        // 255,0,0 sits at the (5,0,0) corner of the 6×6×6 cube: 16+36*5 = 196.
        assert_eq!(rgb_to_ansi256(255, 0, 0), 196);
    }
}
