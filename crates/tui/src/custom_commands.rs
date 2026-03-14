use serde::Deserialize;
use std::path::Path;

use crate::config::config_dir;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RuleOverride {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CommandOverrides {
    pub description: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub tools: Option<RuleOverride>,
    pub bash: Option<RuleOverride>,
    pub web_fetch: Option<RuleOverride>,
}

#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub body: String,
    pub overrides: CommandOverrides,
}

fn commands_dir() -> std::path::PathBuf {
    config_dir().join("commands")
}

/// List all custom commands: (name, description) pairs.
pub fn list() -> Vec<(String, String)> {
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

/// Resolve a slash-command input (e.g. "/commit") to a parsed CustomCommand.
pub fn resolve(input: &str) -> Option<CustomCommand> {
    let name = input.strip_prefix('/')?;
    if name.is_empty() || name.contains('/') || name.contains('.') {
        return None;
    }
    let path = commands_dir().join(format!("{name}.md"));
    parse_command(&path, name)
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

fn parse_frontmatter(content: &str) -> (CommandOverrides, &str) {
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

/// Evaluate `!`-prefixed fenced code blocks by executing them and replacing
/// with a regular code block containing the output.
pub fn evaluate(body: &str) -> String {
    let mut result = String::with_capacity(body.len());
    let mut lines = body.lines().peekable();

    while let Some(line) = lines.next() {
        if is_exec_fence(line) {
            // Collect the code block content
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
            // Execute
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&script)
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
                .unwrap_or_else(|e| format!("error: {e}"));
            result.push_str("```\n");
            result.push_str(&output);
            result.push_str("\n```\n");
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }

    // Remove trailing newline added by the loop
    if result.ends_with('\n') && !body.ends_with('\n') {
        result.pop();
    }
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
    fn is_exec_fence_cases() {
        assert!(is_exec_fence("```!"));
        assert!(is_exec_fence("```!bash"));
        assert!(is_exec_fence("  ```!sh"));
        assert!(!is_exec_fence("```"));
        assert!(!is_exec_fence("```rust"));
        assert!(!is_exec_fence("not a fence"));
    }
}
