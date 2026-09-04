//! UiHost-tier Lua API bindings (require a terminal UI context).
//! Host-tier bindings are registered first via `smelt_core::lua::api::register_host_api`.

mod buf;
mod config;
pub(crate) mod confirm;
mod dialog;
mod engine;
mod history;
mod input;
mod inspect;
mod keymap;
mod layout;
mod metrics;
mod model;
mod notebook;
pub(crate) mod notify;
mod overlay;
pub(crate) mod overlay_layout;
mod paint;
mod permissions;
mod picker;
mod prompt;
mod render;
mod search;
mod session;
mod settings;
pub(crate) mod terminal;
mod text;
pub(crate) mod theme;
pub(crate) mod transcript;
pub(crate) mod vim;
pub(crate) mod win;
mod work;

use super::{LuaRuntime, LuaShared};
use mlua::prelude::*;
use smelt_core::lua::doc::{record_module_doc, ApiClassification, Tier};
use smelt_core::lua::module::LuaMod;
use std::path::Path;
use std::sync::Arc;

/// Identifier for the current alpha Lua API, exposed as `smelt.api_version`.
pub const API_VERSION: &str = "1";

/// Build identity, exposed as `smelt.build`. `version` is sourced from
/// `CARGO_PKG_VERSION` (for programmatic semver comparison); the rest come
/// from the build script (`build.rs`). `DISPLAY` is the single canonical
/// user-facing identity string consumed by the banner, `/version`,
/// `/upgrade`, and `--version`.
pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD_SHA: &str = env!("SMELT_BUILD_SHA");
pub(crate) const BUILD_DATE: &str = env!("SMELT_BUILD_DATE");
pub const BUILD_TARGET: &str = env!("SMELT_TARGET");
pub(crate) const BUILD_TAG: &str = env!("SMELT_BUILD_TAG");
pub(crate) const BUILD_COMMITS: &str = env!("SMELT_BUILD_COMMITS");
pub(crate) const BUILD_DIRTY: &str = env!("SMELT_BUILD_DIRTY");
pub const DISPLAY: &str = env!("SMELT_DISPLAY");

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
    pub(super) fn register_api(
        lua: &Lua,
        shared: &Arc<LuaShared>,
        state_root: &Path,
        cache_root: &Path,
    ) -> LuaResult<()> {
        smelt_core::lua::doc::install_ui_host_availability(
            lua,
            crate::lua::app_ref::ui_host_available,
        );
        let smelt = lua.create_table()?;
        let smelt_keymap = lua.create_table()?;

        let build = lua.create_table()?;
        build.set("version", APP_VERSION)?;
        build.set("sha", optional_str(BUILD_SHA))?;
        build.set("date", optional_str(BUILD_DATE))?;
        build.set("target", BUILD_TARGET)?;
        build.set("tag", optional_str(BUILD_TAG))?;
        build.set("commits", BUILD_COMMITS.parse::<u32>().unwrap_or(0))?;
        build.set("dirty", BUILD_DIRTY == "1")?;
        build.set("display", DISPLAY)?;
        smelt.set("build", build)?;
        smelt.set("api_version", API_VERSION)?;
        record_module_doc(
            "smelt",
            "Root smelt namespace. Host-tier bindings are registered first; UiHost-tier bindings are injected when a TUI is active.",
            ApiClassification::Supported,
        );
        record_module_doc(
            "smelt.build",
            "Compile-time build identity and version metadata for plugins. Fields: `version` (CARGO_PKG_VERSION, for semver comparison), `sha` (short git commit or nil), `date` (committer ISO timestamp or nil), `target` (Rust target triple), `tag` (most recent reachable git tag or nil), `commits` (number of commits since that tag), `dirty` (true when the working tree had uncommitted changes at build time), and `display` (canonical user-facing identity). `display` is `v0.5.0-alpha.2` for a clean tagged build or `v0.5.0-alpha.2-122-97dce0e8-dirty` for a dev build. Shared by the banner, `/version`, `/upgrade`, and `smelt --version`.",
            ApiClassification::Supported,
        );
        record_module_doc(
            "smelt.tick",
            "Reload-safe periodic work. Subscribes to the host's one-second `now` cell and throttles your callback to a fixed interval - safe to call from plugin module bodies. Use this for recurring polling; reserve `smelt.timer.every` for transient timers armed by user actions.",
            ApiClassification::Supported,
        );
        record_module_doc(
            "smelt.dialog",
            "Root-docked modal builders. A dialog replaces the complete composer block while preserving transcript context and the statusline; convenience entry points (`smelt.dialog.input`, `.list`, `.picker`, `.markdown`) wrap common panel shapes. UiHost-only.",
            ApiClassification::Supported,
        );
        record_module_doc(
            "smelt.input",
            "First-class single-line input widget. `smelt.input.new(opts)` returns a handle with `:win()`, `:buf()`, `:text()`, `:set_text()`, `:on()`, and `:key()`; editing keys and paste use the shared line-input core. UiHost-only.",
            ApiClassification::Supported,
        );
        record_module_doc(
            "smelt.list",
            "Picker-style virtual list widget. `smelt.list.new(opts)` returns a handle that owns the buffer, current selection, and keymaps so a plugin can render a scrollable selectable list inside any window or dialog leaf. UiHost-only.",
            ApiClassification::Supported,
        );

        smelt_core::lua::api::register_host_api(
            lua,
            &smelt,
            &smelt_keymap,
            &shared.core,
            state_root,
            cache_root,
        )?;

        // UiHost-tier bindings
        buf::register(lua, &smelt, shared)?;
        win::register(lua, &smelt, shared)?;
        dialog::register(lua, &smelt)?;
        overlay::register(lua, &smelt)?;
        picker::register(lua, &smelt)?;
        notify::register(lua, &smelt, shared)?;
        work::register(lua, &smelt)?;
        prompt::register(lua, &smelt)?;
        theme::register(lua, &smelt)?;
        confirm::register(lua, &smelt, shared)?;
        layout::register(lua, &smelt, shared)?;
        notebook::register(lua, &smelt, shared)?;
        paint::register(lua, &smelt, shared)?;
        render::register(lua, &smelt)?;
        search::register(lua, &smelt)?;
        text::register(lua, &smelt)?;
        engine::register(lua, &smelt, shared)?;
        inspect::register(lua, &smelt, shared)?;
        history::register(lua, &smelt)?;
        input::register(lua, &smelt, shared)?;
        keymap::register(lua, &smelt_keymap, shared)?;
        metrics::register(lua, &smelt)?;
        config::register(lua, &smelt)?;
        model::register(lua, &smelt)?;
        permissions::register(lua, &smelt, shared)?;
        session::register(lua, &smelt, shared)?;
        settings::register(lua, &smelt, shared)?;
        terminal::register(lua, &smelt, shared)?;
        transcript::register(lua, &smelt)?;
        vim::register(lua, &smelt)?;

        smelt.set("keymap", smelt_keymap)?;

        // Cross-cutting UiHost-tier additions to host modules.
        let cmd_tbl: mlua::Table = smelt.get("cmd")?;
        let command_shared = Arc::clone(shared);
        LuaMod::extend_supported(lua, cmd_tbl, "smelt.cmd", Tier::UiHost).live_only_fn(
            "run",
            "Schedule the slash-command line `line` (with or without leading `/`) as if the user had typed it. The app executes it after the current Lua callback returns. Errors are surfaced as in-app notifications.",
            &["line"],
            move |_, line: String| -> LuaResult<()> {
                command_shared
                    .queue_command(line)
                    .map_err(LuaError::RuntimeError)
            },
        )?;
        LuaMod::extend_supported(lua, smelt.get("mode")?, "smelt.mode", Tier::UiHost)
            .live_only_fn(
            "set",
            "Set the active agent mode. The change is applied immediately to the UI and persisted according to the active remember policy.",
            &["mode"],
            |_, mode: String| -> LuaResult<()> {
                let mode = protocol::AgentMode::parse(&mode)
                    .ok_or_else(|| LuaError::RuntimeError(format!("invalid mode `{mode}`")))?;
                crate::lua::with_agent_host(|host| host.set_mode(mode));
                Ok(())
            },
        )?;
        LuaMod::extend_supported(
            lua,
            smelt.get("reasoning")?,
            "smelt.reasoning",
            Tier::UiHost,
        )
        .live_only_fn(
            "set",
            "Set the active reasoning effort. The change is applied immediately to the UI and persisted according to the active remember policy.",
            &["effort"],
            |_, effort: String| -> LuaResult<()> {
                let effort = protocol::ReasoningEffort::parse(&effort).ok_or_else(|| {
                    LuaError::RuntimeError("reasoning effort must not be empty".into())
                })?;
                crate::lua::with_agent_host(|host| host.set_reasoning_effort(effort))
                    .map_err(LuaError::RuntimeError)
            },
        )?;
        let host_root = LuaMod::extend_supported(lua, smelt.clone(), "smelt", Tier::Host);
        host_root.private_fn("__ui_host_available", &[], |_, ()| {
            Ok(crate::lua::app_ref::ui_host_available())
        })?;
        host_root.advanced_fn(
            "ns",
            "Look up or allocate a stable namespace id for `name`. Namespaces scope `buf:mark` / `buf:clear_ns` calls so plugins can repaint their region without disturbing others.",
            &["name"],
            |_, name: String| -> LuaResult<u32> {
                Ok(smelt_core::buffer::create_namespace(&name).0)
            },
        )?;
        let smelt_root = LuaMod::extend_supported(lua, smelt.clone(), "smelt", Tier::UiHost);
        smelt_root.fn_(
            "focus",
            "Return which top-level pane currently has focus: `\"transcript\"` or `\"prompt\"`.",
            &[],
            |_, ()| -> LuaResult<String> {
                Ok(
                    crate::lua::try_with_agent_host(|host| host.focus_name().to_string())
                        .unwrap_or_default(),
                )
            },
        )?;
        smelt_root.live_only_fn(
            "quit",
            "Request a clean shutdown of the app. The quit fires on the next tick after the current handler returns.",
            &[],
            |_, ()| -> LuaResult<()> {
                crate::lua::with_agent_host(|host| host.request_quit());
                Ok(())
            },
        )?;

        lua.globals().set("smelt", smelt)?;
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
