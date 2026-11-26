//! `smelt.shell` - shell command splitting and interactive/background-operator validators.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

const INTERACTIVE_BINS: &[&str] = &[
    "vim", "nvim", "vi", "nano", "emacs", "pico", "less", "more", "top", "htop", "btop", "nmon",
    "irb", "ghci",
];

const GIT_INTERACTIVE_SUBCMDS: &[&str] = &["rebase", "add", "checkout", "clean", "stash"];

/// One command of a shell line and the operator that ends it.
#[derive(Debug, PartialEq, Eq)]
struct Segment {
    command: String,
    op: Option<&'static str>,
}

/// Split a shell line at `;`, `&`, `&&`, `||`, `|`, `|&` and newlines,
/// outside quotes. Redirections such as `2>&1` and `&>` stay in the command.
/// This is a guard against commands that would hang, not a parser.
fn split(line: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    let mut push = |current: &mut String, op: Option<&'static str>| {
        let command = current.trim();
        if !command.is_empty() || op.is_some() {
            segments.push(Segment {
                command: command.to_string(),
                op,
            });
        }
        current.clear();
    };
    while let Some(ch) = chars.next() {
        if let Some(open) = quote {
            current.push(ch);
            if ch == open {
                quote = None;
            } else if ch == '\\' && open == '"' {
                current.extend(chars.next());
            }
            continue;
        }
        let op = match ch {
            '\'' | '"' => {
                quote = Some(ch);
                None
            }
            '\\' => {
                current.push(ch);
                current.extend(chars.next());
                continue;
            }
            ';' | '\n' => Some(";"),
            '|' if chars.next_if_eq(&'|').is_some() => Some("||"),
            '|' if chars.next_if_eq(&'&').is_some() => Some("|&"),
            '|' => Some("|"),
            '&' if chars.next_if_eq(&'&').is_some() => Some("&&"),
            '&' if current.ends_with(['>', '<']) || chars.peek() == Some(&'>') => None,
            '&' => Some("&"),
            _ => None,
        };
        match op {
            Some(op) => push(&mut current, Some(op)),
            None => current.push(ch),
        }
    }
    push(&mut current, None);
    segments
}

fn commands(line: &str) -> Vec<String> {
    split(line)
        .into_iter()
        .map(|segment| segment.command)
        .filter(|command| !command.is_empty())
        .collect()
}

fn check_interactive(command: &str) -> Option<&'static str> {
    for subcmd in commands(command) {
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
    let has = split(command).iter().any(|segment| segment.op == Some("&"));
    if has {
        Some(
            "shell backgrounding (`&`) is not supported in `bash` commands here; remove `&`, set `run_mode` to `background` on the tool call, then use `read_process_output` and `stop_process` with the returned job ID"
                .to_string(),
        )
    } else {
        None
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "shell",
        "Shell command splitting and interactive/background-operator validators.",
        Tier::Host,
    )?;
    m.fn_(
        "split",
        "Split `command` into the sequence of subcommands separated by shell operators (`;`, `&`, `&&`, `||`, `|`) outside quotes. Operators themselves are dropped.",
        &["command"],
        |_, command: String| Ok(commands(&command)),
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_operators_outside_quotes() {
        assert_eq!(
            commands("cd a && echo 'x; y' | grep \"a|b\" || ls; pwd"),
            ["cd a", "echo 'x; y'", "grep \"a|b\"", "ls", "pwd"]
        );
    }

    #[test]
    fn background_operator_is_found_but_redirections_are_not() {
        assert!(check_shell_background_operator("sleep 1 & echo").is_some());
        assert!(check_shell_background_operator("make 2>&1 | tail").is_none());
        assert!(check_shell_background_operator("make &> log && echo ok").is_none());
        assert!(check_shell_background_operator("echo '&'").is_none());
    }

    #[test]
    fn interactive_programs_are_found_in_any_command() {
        assert!(check_interactive("git status && vim a.txt").is_some());
        assert!(check_interactive("git rebase -i HEAD~2").is_some());
        assert!(check_interactive("echo vim").is_none());
    }
}
