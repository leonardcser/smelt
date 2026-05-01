use serde::Deserialize;
use std::path::Path;

use crate::config::config_dir;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RuleOverride {
    pub(crate) allow: Vec<String>,
    pub(crate) ask: Vec<String>,
    pub(crate) deny: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CommandOverrides {
    pub(crate) description: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
    pub(crate) top_k: Option<u32>,
    pub(crate) min_p: Option<f64>,
    pub(crate) repeat_penalty: Option<f64>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) tools: Option<RuleOverride>,
    pub(crate) bash: Option<RuleOverride>,
    pub(crate) web_fetch: Option<RuleOverride>,
}

#[derive(Debug, Clone)]
pub(crate) struct CustomCommand {
    pub(crate) name: String,
    pub(crate) body: String,
    pub(crate) overrides: CommandOverrides,
}

fn commands_dir() -> std::path::PathBuf {
    config_dir().join("commands")
}

/// List all custom commands: (name, description) pairs.
pub(crate) fn list() -> Vec<(String, String)> {
    let dir = commands_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let desc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| {
                let (overrides, body) = parse_frontmatter(&content);
                overrides.description.or_else(|| {
                    body.lines().find(|l| !l.trim().is_empty()).map(|l| {
                        let s = l.trim();
                        if s.len() > 60 {
                            format!("{}...", &s[..s.floor_char_boundary(57)])
                        } else {
                            s.to_string()
                        }
                    })
                })
            })
            .unwrap_or_default();
        items.push((name, desc));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

/// Resolve a user-defined custom command (`~/.config/smelt/commands/<name>.md`)
/// from input like `"/commit"` or `"/commit fix typos"`. Any text after the
/// command name is appended to the body as extra user instructions. Returns
/// `None` for missing files; built-in command bodies (`/reflect`, `/simplify`)
/// resolve through `crate::builtin_commands::resolve` directly, called from
/// the `submit_builtin_command` Lua binding.
pub(crate) fn resolve(input: &str) -> Option<CustomCommand> {
    let after_slash = input.strip_prefix('/')?;
    let name = after_slash.split_whitespace().next()?;
    if name.is_empty() || name.contains('/') || name.contains('.') {
        return None;
    }
    let extra = after_slash[name.len()..].trim();
    let path = commands_dir().join(format!("{name}.md"));
    let mut cmd = parse_command(&path, name)?;
    if !extra.is_empty() {
        cmd.body.push_str("\n\n");
        cmd.body.push_str(extra);
    }
    Some(cmd)
}

/// Check whether `input` (e.g. "/commit") matches a user-defined custom
/// command file, ignoring any trailing arguments. Built-ins (`/reflect`,
/// `/simplify`) are Lua-registered slash commands and are detected by the
/// completer through `crate::lua::is_lua_command`.
pub(crate) fn is_custom_command(input: &str) -> bool {
    let name = input
        .strip_prefix('/')
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("");
    if name.is_empty() || name.contains('/') || name.contains('.') {
        return false;
    }
    commands_dir().join(format!("{name}.md")).exists()
}

fn parse_command(path: &Path, name: &str) -> Option<CustomCommand> {
    let content = std::fs::read_to_string(path).ok()?;
    let (overrides, body) = parse_frontmatter(&content);
    Some(CustomCommand {
        name: name.to_string(),
        body: body.to_string(),
        overrides,
    })
}

pub(crate) fn parse_frontmatter(content: &str) -> (CommandOverrides, &str) {
    if !content.starts_with("---") {
        return (CommandOverrides::default(), content);
    }
    let rest = &content[3..];
    if let Some(end) = rest.find("\n---") {
        let yaml = &rest[..end];
        let after = 3 + end + 4;
        let body = if after < content.len() {
            &content[after..]
        } else {
            ""
        };
        let overrides: CommandOverrides = serde_yml::from_str(yaml).unwrap_or_default();
        (overrides, body)
    } else {
        (CommandOverrides::default(), content)
    }
}

/// Evaluate executable blocks in custom command bodies:
/// - Fenced code blocks starting with `` ```! `` are executed and replaced
///   with a regular code block containing the output.
/// - Inline `` !`command` `` is executed and replaced with the output.
pub(crate) fn evaluate(body: &str) -> String {
    let mut result = String::with_capacity(body.len());
    let mut lines = body.lines().peekable();

    while let Some(line) = lines.next() {
        if is_exec_fence(line) {
            let mut script = String::new();
            for inner in lines.by_ref() {
                if inner.trim_start().starts_with("```") {
                    break;
                }
                if !script.is_empty() {
                    script.push('\n');
                }
                script.push_str(inner);
            }
            let output = exec_command(&script);
            result.push_str("```\n");
            result.push_str(&output);
            result.push_str("\n```\n");
        } else {
            result.push_str(&eval_inline_exec(line));
            result.push('\n');
        }
    }

    // Remove trailing newline added by the loop
    if result.ends_with('\n') && !body.ends_with('\n') {
        result.pop();
    }
    result
}

/// Execute a shell command and return its combined stdout/stderr output.
fn exec_command(script: &str) -> String {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .output()
        .map(|o| {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&stderr);
            }
            s.truncate(s.trim_end().len());
            s
        })
        .unwrap_or_else(|e| format!("error: {e}"))
}

/// Replace all `` !`command` `` occurrences in a line with the command output.
fn eval_inline_exec(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut rest = line;

    while let Some(start) = rest.find("!`") {
        // Don't match escaped or double-backtick
        if start > 0 && rest.as_bytes()[start - 1] == b'\\' {
            result.push_str(&rest[..start - 1]);
            result.push_str("!`");
            rest = &rest[start + 2..];
            continue;
        }
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('`') {
            let cmd = &after[..end];
            if !cmd.is_empty() {
                result.push_str(&exec_command(cmd));
            }
            rest = &after[end + 1..];
        } else {
            // No closing backtick — keep literal
            result.push_str("!`");
            rest = after;
        }
    }
    result.push_str(rest);
    result
}

/// Check if a line is an exec fence: starts with ``` followed by !
fn is_exec_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("```") {
        return false;
    }
    let after_backticks = &trimmed[3..];
    after_backticks.starts_with('!')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_basic() {
        let input = "---\nmodel: gpt-4\ntemperature: 0.2\n---\nPrompt here";
        let (overrides, body) = parse_frontmatter(input);
        assert_eq!(overrides.model.as_deref(), Some("gpt-4"));
        assert_eq!(overrides.temperature, Some(0.2));
        assert_eq!(body, "\nPrompt here");
    }

    #[test]
    fn evaluate_exec_blocks() {
        let input = "Before\n```!\necho hello\n```\nAfter";
        let result = evaluate(input);
        assert_eq!(result, "Before\n```\nhello\n```\nAfter");
    }

    #[test]
    fn evaluate_exec_blocks_with_lang() {
        let input = "```!bash\necho world\n```";
        let result = evaluate(input);
        assert_eq!(result, "```\nworld\n```");
    }

    #[test]
    fn evaluate_no_exec_blocks() {
        let input = "```\ncode\n```";
        let result = evaluate(input);
        assert_eq!(result, input);
    }

    #[test]
    fn evaluate_inline_exec() {
        let input = "version: !`echo 42`";
        let result = evaluate(input);
        assert_eq!(result, "version: 42");
    }

    #[test]
    fn evaluate_inline_exec_multiple() {
        let input = "!`echo a` and !`echo b`";
        let result = evaluate(input);
        assert_eq!(result, "a and b");
    }

    #[test]
    fn evaluate_inline_exec_no_closing() {
        let input = "broken !`no close";
        let result = evaluate(input);
        assert_eq!(result, input);
    }

    #[test]
    fn evaluate_inline_exec_escaped() {
        let input = "keep \\!`echo hi` literal";
        let result = evaluate(input);
        assert_eq!(result, "keep !`echo hi` literal");
    }

    #[test]
    fn is_exec_fence_cases() {
        assert!(is_exec_fence("```!"));
        assert!(is_exec_fence("```!bash"));
        assert!(is_exec_fence("  ```!sh"));
        assert!(!is_exec_fence("```"));
        assert!(!is_exec_fence("```rust"));
        assert!(!is_exec_fence("not a fence"));
    }
}
