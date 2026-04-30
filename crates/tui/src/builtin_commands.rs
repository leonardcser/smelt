//! Built-in `/reflect` and `/simplify` prompt templates. Bodies live as
//! `.md` files under `engine/src/prompts/commands/`; this module renders
//! them with the current `multi_agent` context, applies frontmatter
//! overrides, and returns a [`CustomCommand`] ready for
//! `TuiApp::begin_custom_command_turn`. The only public surface is
//! `resolve`, called from the `smelt.engine.submit_builtin_command` Lua
//! binding (the `/reflect` and `/simplify` Lua plugins are the user-
//! visible entry points).

use crate::custom_commands::CustomCommand;

struct BuiltinCommand {
    name: &'static str,
    content: &'static str,
}

const COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "reflect",
        content: include_str!("../../engine/src/prompts/commands/reflect.md"),
    },
    BuiltinCommand {
        name: "simplify",
        content: include_str!("../../engine/src/prompts/commands/simplify.md"),
    },
];

/// Resolve a builtin command by name, appending any extra arguments.
/// Builtin command bodies are minijinja templates; `multi_agent` controls
/// whether sections gated on multi-agent mode are included.
pub fn resolve(input: &str, multi_agent: bool) -> Option<CustomCommand> {
    let after_slash = input.strip_prefix('/')?;
    let name = after_slash.split_whitespace().next()?;
    let extra = after_slash[name.len()..].trim();
    let cmd = COMMANDS.iter().find(|c| c.name == name)?;
    let (overrides, body) = crate::custom_commands::parse_frontmatter(cmd.content);
    let mut body = render_template(body, multi_agent);
    if !extra.is_empty() {
        body.push_str("\n\n## Additional Focus\n\n");
        body.push_str(extra);
    }
    Some(CustomCommand {
        name: name.to_string(),
        body,
        overrides,
    })
}

fn render_template(body: &str, multi_agent: bool) -> String {
    let env = minijinja::Environment::new();
    match env.template_from_str(body) {
        Ok(tmpl) => tmpl
            .render(minijinja::context! { multi_agent => multi_agent })
            .unwrap_or_else(|_| body.to_string()),
        Err(_) => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_simplify_multi_agent() {
        let cmd = resolve("/simplify", true).unwrap();
        assert_eq!(cmd.name, "simplify");
        assert!(cmd.body.contains("Launch Three Review Agents in Parallel"));
        assert!(!cmd.body.contains("Do not launch subagents"));
    }

    #[test]
    fn resolve_simplify_single_agent() {
        let cmd = resolve("/simplify", false).unwrap();
        assert_eq!(cmd.name, "simplify");
        assert!(cmd.body.contains("Do not launch subagents"));
        assert!(!cmd.body.contains("Launch Three Review Agents in Parallel"));
    }

    #[test]
    fn resolve_simplify_with_args() {
        let cmd = resolve("/simplify focus on error handling", true).unwrap();
        assert!(cmd.body.contains("focus on error handling"));
    }

    #[test]
    fn unknown_returns_none() {
        assert!(resolve("/nonexistent", true).is_none());
    }
}
