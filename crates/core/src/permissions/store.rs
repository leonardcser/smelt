use crate::{config, permissions::PermissionGrant};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const WORKSPACE_PERMISSIONS_FILE: &str = "permissions.json";
const REPOSITORY_PERMISSIONS_FILE: &str = "repository-permissions.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceScope {
    Workspace,
    Repository,
}

impl PersistenceScope {
    fn filename(self) -> &'static str {
        match self {
            Self::Workspace => WORKSPACE_PERMISSIONS_FILE,
            Self::Repository => REPOSITORY_PERMISSIONS_FILE,
        }
    }
}

/// A single persisted workspace or repository permission rule.
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

#[derive(Debug, Clone)]
pub struct PermissionStore {
    state_root: PathBuf,
}

impl PermissionStore {
    pub fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    pub fn from_process() -> Self {
        Self::new(config::state_dir())
    }

    fn permissions_path(&self, root: &str, scope: PersistenceScope) -> PathBuf {
        self.state_root
            .join("workspaces")
            .join(encode_path(root))
            .join(scope.filename())
    }

    pub fn load(&self, root: &str, scope: PersistenceScope) -> Vec<Rule> {
        let path = self.permissions_path(root, scope);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let store: Store = serde_json::from_str(&contents).unwrap_or_default();
        store.rules
    }

    pub fn load_for_scopes(&self, workspace: &Path, repository_key: Option<&Path>) -> Vec<Rule> {
        let mut rules = self.load(&workspace.to_string_lossy(), PersistenceScope::Workspace);
        if let Some(repository_key) = repository_key {
            merge_rules(
                &mut rules,
                self.load(
                    &repository_key.to_string_lossy(),
                    PersistenceScope::Repository,
                ),
            );
        }
        rules
    }

    pub fn save(&self, root: &str, scope: PersistenceScope, rules: &[Rule]) {
        let path = self.permissions_path(root, scope);
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

    pub fn add_tool(&self, root: &str, scope: PersistenceScope, tool: &str, patterns: Vec<String>) {
        let mut rules = self.load(root, scope);
        if let Some(existing) = rules.iter_mut().find(|r| r.tool == tool) {
            if patterns.is_empty() || existing.patterns.is_empty() {
                existing.patterns.clear();
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
        self.save(root, scope, &rules);
    }

    pub fn add_dir(&self, root: &str, scope: PersistenceScope, dir: &str) {
        let mut rules = self.load(root, scope);
        let already = rules
            .iter()
            .any(|r| r.tool == "directory" && r.patterns.iter().any(|p| p == dir));
        if !already {
            rules.push(Rule {
                tool: "directory".into(),
                patterns: vec![dir.to_string()],
            });
        }
        self.save(root, scope, &rules);
    }

    pub fn add_grant(&self, root: &str, scope: PersistenceScope, grant: PermissionGrant) {
        match grant {
            PermissionGrant::Tool { tool } => self.add_tool(root, scope, &tool, Vec::new()),
            PermissionGrant::Command { tool, pattern } => {
                self.add_tool(root, scope, &tool, vec![pattern]);
            }
            PermissionGrant::PathPrefix { dir } => {
                self.add_dir(root, scope, &dir.to_string_lossy());
            }
        }
    }
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

pub fn load(cwd: &str) -> Vec<Rule> {
    PermissionStore::from_process().load(cwd, PersistenceScope::Workspace)
}

pub fn save(cwd: &str, rules: &[Rule]) {
    PermissionStore::from_process().save(cwd, PersistenceScope::Workspace, rules);
}

pub fn add_tool(cwd: &str, tool: &str, patterns: Vec<String>) {
    PermissionStore::from_process().add_tool(cwd, PersistenceScope::Workspace, tool, patterns);
}

pub fn add_dir(cwd: &str, dir: &str) {
    PermissionStore::from_process().add_dir(cwd, PersistenceScope::Workspace, dir);
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
    fn scoped_load_combines_workspace_and_repository_without_siblings() {
        let state = tempfile::tempdir().unwrap();
        let dirs = tempfile::tempdir().unwrap();
        let workspace = dirs.path().join("feature");
        let repository = dirs.path().join("repo");
        let sibling = dirs.path().join("other-feature");
        for path in [&workspace, &repository, &sibling] {
            std::fs::create_dir(path).unwrap();
        }
        let store = PermissionStore::new(state.path().to_path_buf());
        store.add_tool(
            &workspace.to_string_lossy(),
            PersistenceScope::Workspace,
            "bash",
            vec!["cargo test *".into()],
        );
        store.add_grant(
            &repository.to_string_lossy(),
            PersistenceScope::Repository,
            PermissionGrant::Command {
                tool: "bash".into(),
                pattern: "git status".into(),
            },
        );
        store.add_tool(
            &sibling.to_string_lossy(),
            PersistenceScope::Workspace,
            "bash",
            vec!["rm *".into()],
        );

        let rules = store.load_for_scopes(&workspace, Some(&repository));
        let patterns = &rules
            .iter()
            .find(|rule| rule.tool == "bash")
            .unwrap()
            .patterns;
        assert_eq!(
            patterns,
            &vec!["cargo test *".to_string(), "git status".to_string()]
        );
    }

    #[test]
    fn repository_rules_are_distinct_from_main_checkout_rules() {
        let state = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = repository.path().to_string_lossy();
        store.add_tool(
            &root,
            PersistenceScope::Workspace,
            "bash",
            vec!["checkout-only *".into()],
        );
        store.add_grant(
            &root,
            PersistenceScope::Repository,
            PermissionGrant::Command {
                tool: "bash".into(),
                pattern: "repository-wide *".into(),
            },
        );

        assert_eq!(
            store.load(&root, PersistenceScope::Workspace)[0].patterns,
            vec!["checkout-only *"]
        );
        assert_eq!(
            store.load(&root, PersistenceScope::Repository)[0].patterns,
            vec!["repository-wide *"]
        );
        let merged = store.load_for_scopes(repository.path(), Some(repository.path()));
        assert_eq!(
            merged[0].patterns,
            vec!["checkout-only *", "repository-wide *"]
        );
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
