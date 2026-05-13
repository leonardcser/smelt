//! `smelt.shell` — shell command splitting and interactive/background-operator validators.

use crate::lua::doc::{record_module_doc, register_fn};
use lua_doc_derive::lua_module;
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
            return Some("Interactive commands (editors, REPLs, pagers) cannot run here — they require a terminal. If there is no non-interactive alternative, ask the user to run it themselves.");
        }
        if base == "git" {
            let has_interactive_flag = parts.iter().any(|p| *p == "-i" || *p == "--interactive");
            if has_interactive_flag {
                let has_interactive_subcmd =
                    parts.iter().any(|p| GIT_INTERACTIVE_SUBCMDS.contains(p));
                if has_interactive_subcmd {
                    return Some("Interactive git commands (rebase -i, add -i, etc.) cannot run here — they require a terminal. If there is no non-interactive alternative, ask the user to run it themselves.");
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
            "Shell backgrounding (`&`) is not supported in `bash` commands here. Remove `&` and set `run_in_background=true` on the tool call. Then use `read_process_output` and `stop_process` with the returned process id."
                .to_string(),
        )
    } else {
        None
    }
}

#[lua_module]
pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let shell_tbl = lua.create_table()?;
    record_module_doc(
        "smelt.shell",
        "Shell command splitting and interactive/background-operator validators.",
    );
    register_fn(
        &shell_tbl,
        "smelt.shell",
        "split",
        "Split `command` into the sequence of subcommands separated by shell operators (`;`, `&&`, `||`, `|`). Operators themselves are dropped.",
        &["command"],
        lua,
        |_, command: String| Ok(crate::permissions::split_shell_commands(&command)),
    )?;
    register_fn(
        &shell_tbl,
        "smelt.shell",
        "split_with_ops",
        "Split `command` into subcommands and pair each with the operator that followed it. Returns rows of `{ command = string, op = string? }`.",
        &["command"],
        lua,
        |lua, command: String|  -> LuaResult<mlua::Table>{
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
    register_fn(
        &shell_tbl,
        "smelt.shell",
        "check_interactive",
        "Return a user-facing error message if `command` would invoke an interactive program (editor, REPL, pager, `git -i`, etc.), or `nil` if it is safe to run non-interactively.",
        &["command"],
        lua,
        |_, command: String| Ok(check_interactive(&command).map(String::from)),
    )?;
    register_fn(
        &shell_tbl,
        "smelt.shell",
        "check_background_op",
        "Return a user-facing error message if `command` uses the shell `&` background operator, or `nil` otherwise.",
        &["command"],
        lua,
        |_, command: String| Ok(check_shell_background_operator(&command)),
    )?;
    register_fn(
        &shell_tbl,
        "smelt.shell",
        "extract_paths",
        "Extract filesystem paths referenced by `command` for workspace permission checks.",
        &["command"],
        lua,
        |_, command: String| -> LuaResult<Vec<String>> {
            Ok(crate::permissions::workspace::extract_paths_from_command(
                &command,
            ))
        },
    )?;
    smelt.set("shell", shell_tbl)?;
    Ok(())
}
