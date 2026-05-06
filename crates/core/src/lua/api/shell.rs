//! `smelt.shell` bindings — pure parsing helpers consumed by the
//! Lua-side `bash` tool and `background_commands` plugin. Wraps
//! `crate::permissions::split_shell_commands` for AST-level
//! splitting, plus inline interactive-binary / shell-background
//! validators (the legacy engine `BashTool` owned these).

use mlua::prelude::*;

/// Known interactive binaries that require a TTY.
const INTERACTIVE_BINS: &[&str] = &[
    "vim", "nvim", "vi", "nano", "emacs", "pico", "less", "more", "top", "htop", "btop", "nmon",
    "irb", "ghci",
];

/// Git subcommands whose `-i`/`--interactive` flag requires a TTY.
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

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let shell_tbl = lua.create_table()?;
    shell_tbl.set(
        "split",
        lua.create_function(|_, command: String| {
            Ok(crate::permissions::split_shell_commands(&command))
        })?,
    )?;
    shell_tbl.set(
        "split_with_ops",
        lua.create_function(|lua, command: String| {
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
        })?,
    )?;
    shell_tbl.set(
        "check_interactive",
        lua.create_function(
            |_, command: String| Ok(check_interactive(&command).map(String::from)),
        )?,
    )?;
    shell_tbl.set(
        "check_background_op",
        lua.create_function(|_, command: String| Ok(check_shell_background_operator(&command)))?,
    )?;
    // smelt.shell.extract_paths(command) -> [string]
    //
    // Pull tokens that look like absolute (`/foo`) or home-rooted
    // (`~/foo`) paths from a shell command. Heredoc bodies are
    // stripped first. Used by the bash tool's
    // `paths_for_workspace(args)` callback.
    shell_tbl.set(
        "extract_paths",
        lua.create_function(|_, command: String| {
            Ok(crate::permissions::workspace::extract_paths_from_command(
                &command,
            ))
        })?,
    )?;
    smelt.set("shell", shell_tbl)?;
    Ok(())
}
