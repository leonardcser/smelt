//! `smelt.terminal` bindings - side-effectful terminal integration.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

const ESC: char = '\u{1b}';
const BEL: char = '\u{7}';

fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .filter(|ch| match *ch {
            ESC | BEL => false,
            ch if ch.is_control() => false,
            _ => true,
        })
        .collect()
}

fn title_sequence(title: Option<&str>) -> String {
    let clean = title.map(sanitize_title).unwrap_or_default();
    format!("{ESC}]0;{clean}{BEL}")
}

fn set_terminal_title(bytes: &[u8]) -> LuaResult<bool> {
    crate::lua::try_with_app(|app| -> std::io::Result<bool> {
        let Some(terminal) = app.terminal.as_mut() else {
            return Ok(false);
        };
        terminal.set_title_sequence(bytes)?;
        Ok(true)
    })
    .unwrap_or(Ok(false))
    .map_err(mlua::Error::external)
}

fn clear_terminal_title(bytes: &[u8]) -> LuaResult<bool> {
    crate::lua::try_with_app(|app| -> std::io::Result<bool> {
        let Some(terminal) = app.terminal.as_mut() else {
            return Ok(false);
        };
        terminal.clear_title_sequence(bytes)?;
        Ok(true)
    })
    .unwrap_or(Ok(false))
    .map_err(mlua::Error::external)
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "terminal",
        "Terminal integration helpers. UiHost-only.",
        Tier::UiHost,
    )?;

    m.fn_(
        "set_title",
        "Set the terminal window/tab title using OSC 0. Pass nil to clear it. Control characters are stripped from titles before writing.",
        &["title"],
        |_, title: Option<String>| -> LuaResult<bool> {
            let seq = title_sequence(title.as_deref());
            if title.is_some() {
                set_terminal_title(seq.as_bytes())
            } else {
                clear_terminal_title(seq.as_bytes())
            }
        },
    )?;

    m.fn_(
        "clear_title",
        "Clear the terminal window/tab title using OSC 0 with an empty payload.",
        &[],
        |_, ()| -> LuaResult<bool> {
            let seq = title_sequence(None);
            clear_terminal_title(seq.as_bytes())
        },
    )?;

    m.fn_(
        "size",
        "Return the current terminal size as `{ width, height }` in cells.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let (width, height) = crate::lua::try_with_app(|app| {
                app.terminal
                    .as_ref()
                    .map(|terminal| terminal.size())
                    .unwrap_or_else(crossterm::terminal::size)
            })
            .unwrap_or_else(crossterm::terminal::size)
            .map_err(mlua::Error::external)?;
            let out = lua.create_table()?;
            out.set("width", width)?;
            out.set("height", height)?;
            Ok(out)
        },
    )?;

    m.fn_(
        "info",
        "Return environment-derived terminal information: `{ term, term_program, color_term, platform, tmux, screen, ssh }`.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            if let Some(value) = env_var("TERM") {
                out.set("term", value)?;
            }
            if let Some(value) = env_var("TERM_PROGRAM") {
                out.set("term_program", value)?;
            }
            if let Some(value) = env_var("COLORTERM") {
                out.set("color_term", value)?;
            }
            out.set("platform", std::env::consts::OS)?;
            out.set("tmux", env_var("TMUX").is_some())?;
            out.set("screen", env_var("STY").is_some())?;
            out.set(
                "ssh",
                env_var("SSH_CONNECTION").is_some() || env_var("SSH_TTY").is_some(),
            )?;
            Ok(out)
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_sequence_strips_controls() {
        assert_eq!(
            title_sequence(Some("a\u{1b}]0;x\u{7}\nb")),
            "\u{1b}]0;a]0;xb\u{7}"
        );
    }

    #[test]
    fn clear_title_uses_empty_payload() {
        assert_eq!(title_sequence(None), "\u{1b}]0;\u{7}");
    }
}
