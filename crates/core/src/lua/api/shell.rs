//! `smelt.shell` - shell command splitting and interactive/background-operator validators.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

const INTERACTIVE_BINS: &[&str] = &[
    "vim", "nvim", "vi", "nano", "emacs", "pico", "less", "more", "top", "htop", "btop", "nmon",
    "irb", "ghci",
];

const GIT_INTERACTIVE_SUBCMDS: &[&str] = &["rebase", "add", "checkout", "clean", "stash"];

fn check_interactive(command: &str) -> Option<&'static str> {
    let cmds = crate::permissions::split_shell_commands(command);
    for subcmd in &cmds {
        let parts: Vec<&str> = subcmd.split_whitespace().collect();
        let bin = match parts.first() {
            Some(b) => *b,
            None => continue,
        };
        let base = bin.rsplit('/').next().unwrap_or(bin);
        if INTERACTIVE_BINS.contains(&base) {
            return Some("interactive commands (editors, REPLs, pagers) cannot run here because they require a terminal; if there is no non-interactive alternative, ask the user to run them");
        }
        if base == "git" {
            let has_interactive_flag = parts.iter().any(|p| *p == "-i" || *p == "--interactive");
            if has_interactive_flag {
                let has_interactive_subcmd =
                    parts.iter().any(|p| GIT_INTERACTIVE_SUBCMDS.contains(p));
                if has_interactive_subcmd {
                    return Some("interactive git commands (rebase -i, add -i, etc.) cannot run here because they require a terminal; if there is no non-interactive alternative, ask the user to run them");
                }
            }
        }
    }
    None
}

fn check_shell_background_operator(command: &str) -> Option<String> {
    let has = crate::permissions::split_shell_commands_with_ops(command)
        .iter()
        .any(|(_, op)| op.as_deref() == Some("&"));
    if has {
        Some(
            "shell backgrounding (`&`) is not supported in `bash` commands here; remove `&`, set `background=true` on the tool call, then use `read_process_output` and `stop_process` with the returned job ID"
                .to_string(),
        )
    } else {
        None
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::under(
        lua,
        smelt,
        "shell",
        "Shell command splitting and interactive/background-operator validators.",
        Tier::Host,
    )?;
    m.fn_(
        "split",
        "Split `command` into the sequence of subcommands separated by shell operators (`;`, `&&`, `||`, `|`). Operators themselves are dropped.",
        &["command"],
        |_, command: String| Ok(crate::permissions::split_shell_commands(&command)),
    )?;
    m.fn_(
        "split_with_ops",
        "Split `command` into subcommands and pair each with the operator that followed it. Returns rows of `{ command = string, op = string? }`.",
        &["command"],
        |lua, command: String| -> LuaResult<mlua::Table> {
            let parts = crate::permissions::split_shell_commands_with_ops(&command);
            let out = lua.create_table()?;
            for (i, (cmd, op)) in parts.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("command", cmd)?;
                if let Some(op) = op {
                    row.set("op", op)?;
                }
                out.set(i + 1, row)?;
            }
            Ok(out)
        },
    )?;
    m.fn_(
        "check_interactive",
        "Return a user-facing error message if `command` would invoke an interactive program (editor, REPL, pager, `git -i`, etc.), or `nil` if it is safe to run non-interactively.",
        &["command"],
        |_, command: String| Ok(check_interactive(&command).map(String::from)),
    )?;
    m.fn_(
        "check_background_op",
        "Return a user-facing error message if `command` uses the shell `&` background operator, or `nil` otherwise.",
        &["command"],
        |_, command: String| Ok(check_shell_background_operator(&command)),
    )?;
    m.fn_(
        "extract_paths",
        "Extract filesystem paths referenced by `command` for workspace permission checks.",
        &["command"],
        |_, command: String| -> LuaResult<Vec<String>> {
            Ok(crate::permissions::workspace::extract_paths_from_command(
                &command,
            ))
        },
    )?;
    m.fn_(
        "has_output_redirection",
        "Return true when `command` contains shell output redirection such as `>` or `>>`.",
        &["command"],
        |_, command: String| Ok(crate::permissions::shell_has_output_redirection(&command)),
    )?;
    Ok(())
}
