//! `smelt.terminal` bindings - side-effectful terminal integration.

use mlua::prelude::*;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

const ESC: char = '\u{1b}';
const BEL: char = '\u{7}';

fn sanitize_control_payload(text: &str) -> String {
    text.chars()
        .filter(|ch| match *ch {
            ESC | BEL => false,
            ch if ch.is_control() => false,
            _ => true,
        })
        .collect()
}

fn sanitize_title(title: &str) -> String {
    sanitize_control_payload(title)
}

fn title_sequence(title: Option<&str>) -> String {
    let clean = title.map(sanitize_title).unwrap_or_default();
    format!("{ESC}]0;{clean}{BEL}")
}

fn osc9_notification_sequence(message: &str, dcs_passthrough: bool) -> String {
    let clean = sanitize_control_payload(message);
    if dcs_passthrough {
        let escaped = clean.replace(ESC, "\u{1b}\u{1b}");
        format!("{ESC}Ptmux;{ESC}{ESC}]9;{escaped}{BEL}{ESC}\\")
    } else {
        format!("{ESC}]9;{clean}{BEL}")
    }
}

fn write_terminal_control(bytes: &[u8]) -> LuaResult<bool> {
    crate::lua::try_with_app(|app| -> std::io::Result<bool> {
        let Some(terminal) = app.terminal.as_mut() else {
            return Ok(false);
        };
        terminal.write_control_sequence(bytes)?;
        Ok(true)
    })
    .unwrap_or(Ok(false))
    .map_err(mlua::Error::external)
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
        "bell",
        "Ring the terminal bell (BEL). Returns false when no interactive terminal is attached.",
        &[],
        |_, ()| -> LuaResult<bool> { write_terminal_control(&[BEL as u8]) },
    )?;

    m.fn_(
        "osc9_notify",
        "Post an OSC 9 terminal notification with `message`. Pass `{ dcs_passthrough = true }` inside tmux to wrap the notification for tmux passthrough. Control characters are stripped from messages before writing.",
        &["message", "opts"],
        |_, (message, opts): (String, Option<mlua::Table>)| -> LuaResult<bool> {
            let dcs_passthrough = opts
                .as_ref()
                .and_then(|table| table.get::<Option<bool>>("dcs_passthrough").ok())
                .flatten()
                .unwrap_or(false);
            let seq = osc9_notification_sequence(&message, dcs_passthrough);
            write_terminal_control(seq.as_bytes())
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
    fn osc9_sequence_strips_controls() {
        assert_eq!(
            osc9_notification_sequence("done\u{1b}]9;x\u{7}\nnow", false),
            "\u{1b}]9;done]9;xnow\u{7}"
        );
    }

    #[test]
    fn osc9_sequence_supports_tmux_passthrough() {
        assert_eq!(
            osc9_notification_sequence("done", true),
            "\u{1b}Ptmux;\u{1b}\u{1b}]9;done\u{7}\u{1b}\\"
        );
    }

    #[test]
    fn clear_title_uses_empty_payload() {
        assert_eq!(title_sequence(None), "\u{1b}]0;\u{7}");
    }
}
