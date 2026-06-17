use crate::{config, permissions::workspace};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single persisted workspace permission rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Tool name (e.g. `"bash"`) or `"directory"` for dir-based approvals.
    pub tool: String,
    /// Glob patterns; empty means "allow all" for this tool.
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    rules: Vec<Rule>,
}

/// Encode a cwd path as a directory name: `/` → `-`, `-` → `--`, leading `-` stripped.
/// Reversible and collision-free (see `decode_path`).
fn encode_path(cwd: &str) -> String {
    let mut out = String::with_capacity(cwd.len());
    for c in cwd.chars() {
        match c {
            '/' => out.push('-'),
            '-' => out.push_str("--"),
            c => out.push(c),
        }
    }
    if out.starts_with('-') && !out.starts_with("--") {
        out.remove(0);
    }
    out
}

#[cfg(test)]
fn decode_path(encoded: &str) -> String {
    let full = format!("-{encoded}"); // restore the leading `/`
    let mut out = String::with_capacity(full.len());
    let mut chars = full.chars();
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.as_str().starts_with('-') {
                chars.next();
                out.push('-');
            } else {
                out.push('/');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn workspaces_dir() -> PathBuf {
    config::state_dir().join("workspaces")
}

fn workspace_dir(cwd: &str) -> PathBuf {
    workspaces_dir().join(encode_path(cwd))
}

fn permissions_path(cwd: &str) -> PathBuf {
    workspace_dir(cwd).join("permissions.json")
}

pub fn load(cwd: &str) -> Vec<Rule> {
    let path = permissions_path(cwd);
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let store: Store = serde_json::from_str(&contents).unwrap_or_default();
    store.rules
}

pub fn load_for_roots(cwd: &str, roots: &[PathBuf]) -> Vec<Rule> {
    let mut loaded: Vec<PathBuf> = Vec::new();
    let mut rules = Vec::new();
    for root in std::iter::once(Path::new(cwd).to_path_buf()).chain(roots.iter().cloned()) {
        if loaded
            .iter()
            .any(|existing| workspace::paths_equivalent(existing, &root))
        {
            continue;
        }
        merge_rules(&mut rules, load(&root.to_string_lossy()));
        loaded.push(root);
    }
    rules
}

fn merge_rules(rules: &mut Vec<Rule>, incoming: Vec<Rule>) {
    for rule in incoming {
        if let Some(existing) = rules.iter_mut().find(|existing| existing.tool == rule.tool) {
            if rule.patterns.is_empty() || existing.patterns.is_empty() {
                existing.patterns.clear();
            } else {
                for pattern in rule.patterns {
                    if !existing.patterns.contains(&pattern) {
                        existing.patterns.push(pattern);
                    }
                }
            }
        } else {
            rules.push(rule);
        }
    }
}

pub fn save(cwd: &str, rules: &[Rule]) {
    let path = permissions_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let store = Store {
        rules: rules.to_vec(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&store) {
        let _ = std::fs::write(&path, json);
    }
}

/// Add a tool-level approval rule, merging with any existing rule for the same tool.
pub fn add_tool(cwd: &str, tool: &str, patterns: Vec<String>) {
    let mut rules = load(cwd);
    if let Some(existing) = rules.iter_mut().find(|r| r.tool == tool) {
        if patterns.is_empty() || existing.patterns.is_empty() {
            existing.patterns.clear(); // empty = "allow all"; that always wins
        } else {
            for p in &patterns {
                if !existing.patterns.contains(p) {
                    existing.patterns.push(p.clone());
                }
            }
        }
    } else {
        rules.push(Rule {
            tool: tool.to_string(),
            patterns,
        });
    }
    save(cwd, &rules);
}

/// Add a directory-level approval rule (idempotent).
pub fn add_dir(cwd: &str, dir: &str) {
    let mut rules = load(cwd);
    let already = rules
        .iter()
        .any(|r| r.tool == "directory" && r.patterns.iter().any(|p| p == dir));
    if !already {
        rules.push(Rule {
            tool: "directory".into(),
            patterns: vec![dir.to_string()],
        });
    }
    save(cwd, &rules);
}

/// Build compiled approval maps from persisted workspace rules.
pub fn into_approvals(rules: &[Rule]) -> (HashMap<String, Vec<glob::Pattern>>, Vec<PathBuf>) {
    let mut tool_map: HashMap<String, Vec<glob::Pattern>> = HashMap::new();
    let mut dirs = Vec::new();
    for rule in rules {
        if rule.tool == "directory" {
            for p in &rule.patterns {
                dirs.push(PathBuf::from(p));
            }
        } else {
            let compiled: Vec<glob::Pattern> = rule
                .patterns
                .iter()
                .filter(|p| *p != "*")
                .filter_map(|p| glob::Pattern::new(p).ok())
                .collect();
            tool_map
                .entry(rule.tool.clone())
                .or_default()
                .extend(compiled);
        }
    }
    (tool_map, dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_rules_combines_workspace_roots() {
        let mut rules = vec![
            Rule {
                tool: "directory".into(),
                patterns: vec!["/tmp".into()],
            },
            Rule {
                tool: "bash".into(),
                patterns: vec!["git status".into()],
            },
        ];
        merge_rules(
            &mut rules,
            vec![
                Rule {
                    tool: "directory".into(),
                    patterns: vec!["/var/tmp".into(), "/tmp".into()],
                },
                Rule {
                    tool: "bash".into(),
                    patterns: vec!["git diff".into()],
                },
            ],
        );

        assert_eq!(
            rules
                .iter()
                .find(|rule| rule.tool == "directory")
                .unwrap()
                .patterns,
            vec!["/tmp".to_string(), "/var/tmp".to_string()]
        );
        assert_eq!(
            rules
                .iter()
                .find(|rule| rule.tool == "bash")
                .unwrap()
                .patterns,
            vec!["git status".to_string(), "git diff".to_string()]
        );
    }

    #[test]
    fn merge_rules_blanket_tool_wins() {
        let mut rules = vec![Rule {
            tool: "bash".into(),
            patterns: vec!["git status".into()],
        }];
        merge_rules(
            &mut rules,
            vec![Rule {
                tool: "bash".into(),
                patterns: Vec::new(),
            }],
        );

        assert!(rules
            .iter()
            .find(|rule| rule.tool == "bash")
            .unwrap()
            .patterns
            .is_empty());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let paths = [
            "/Users/leo/dev/rust/agent",
            "/Users/leo/dev-rust/agent",
            "/tmp/foo",
            "/a/b-c/d",
            "/a/b/c/d",
            "/home/user/my--project",
        ];
        for path in paths {
            let encoded = encode_path(path);
            let decoded = decode_path(&encoded);
            assert_eq!(
                decoded, path,
                "roundtrip failed for {path} (encoded: {encoded})"
            );
        }
    }

    #[test]
    fn encode_no_collision() {
        // These previously collided with naive `-` replacement.
        let a = encode_path("/a/b-c/d");
        let b = encode_path("/a/b/c/d");
        assert_ne!(a, b, "collision between /a/b-c/d and /a/b/c/d");
    }

    #[test]
    fn encode_readable() {
        assert_eq!(
            encode_path("/Users/leo/dev/rust/agent"),
            "Users-leo-dev-rust-agent"
        );
        assert_eq!(
            encode_path("/Users/leo/dev-rust/agent"),
            "Users-leo-dev--rust-agent"
        );
    }
}
