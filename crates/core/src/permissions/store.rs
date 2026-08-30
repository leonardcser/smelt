use crate::permissions::PermissionGrant;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
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

    fn lock_filename(self) -> &'static str {
        match self {
            Self::Workspace => "permissions.lock",
            Self::Repository => "repository-permissions.lock",
        }
    }
}

/// A single persisted workspace or repository permission rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    revision: u64,
    #[serde(default)]
    rules: Vec<Rule>,
}

/// A validated, canonical view of one persisted permission file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub revision: u64,
    pub rules: Vec<Rule>,
}

/// Revision-checked replacement accepted by [`PermissionStore::replace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub expected_revision: u64,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Default)]
pub(crate) struct CompiledApprovals {
    pub(crate) tools: HashMap<String, Vec<glob::Pattern>>,
    pub(crate) dirs: Vec<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PermissionSet {
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

    fn scope_dir(&self, root: &str) -> PathBuf {
        self.state_root.join("workspaces").join(encode_path(root))
    }

    fn permissions_path(&self, root: &str, scope: PersistenceScope) -> PathBuf {
        self.scope_dir(root).join(scope.filename())
    }

    fn lock_path(&self, root: &str, scope: PersistenceScope) -> PathBuf {
        self.scope_dir(root).join(scope.lock_filename())
    }

    pub fn load(&self, root: &str, scope: PersistenceScope) -> io::Result<Vec<Rule>> {
        Ok(self.load_snapshot(root, scope)?.rules)
    }

    pub fn load_snapshot(&self, root: &str, scope: PersistenceScope) -> io::Result<Snapshot> {
        load_path(&self.permissions_path(root, scope))
    }

    pub(crate) fn load_approvals(
        &self,
        root: &str,
        scope: PersistenceScope,
    ) -> io::Result<CompiledApprovals> {
        let snapshot = self.load_snapshot(root, scope)?;
        PermissionSet {
            rules: snapshot.rules,
        }
        .compile()
    }

    /// Replace one scope only if it still has the revision the caller read.
    /// This prevents a stale full snapshot from discarding concurrent grants.
    pub fn replace(
        &self,
        root: &str,
        scope: PersistenceScope,
        replacement: Replacement,
    ) -> io::Result<u64> {
        let _lock = StoreLock::acquire(&self.lock_path(root, scope))?;
        let path = self.permissions_path(root, scope);
        let current = load_path(&path)?;
        if current.revision != replacement.expected_revision {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "permission store changed since it was read (expected revision {}, found {}); reload and retry",
                    replacement.expected_revision, current.revision
                ),
            ));
        }

        let replacement = PermissionSet::from_rules(&path, replacement.rules)?;
        if replacement.rules == current.rules {
            return Ok(current.revision);
        }
        let revision = next_revision(current.revision)?;
        save_path(&path, revision, &replacement)?;
        Ok(revision)
    }

    fn update(
        &self,
        root: &str,
        scope: PersistenceScope,
        update: impl FnOnce(&mut PermissionSet) -> bool,
    ) -> io::Result<bool> {
        let _lock = StoreLock::acquire(&self.lock_path(root, scope))?;
        let path = self.permissions_path(root, scope);
        let current = load_path(&path)?;
        let mut permissions = PermissionSet {
            rules: current.rules,
        };
        let changed = update(&mut permissions);
        if changed {
            permissions.normalize_order();
            save_path(&path, next_revision(current.revision)?, &permissions)?;
        }
        Ok(changed)
    }

    pub fn add_tool(
        &self,
        root: &str,
        scope: PersistenceScope,
        tool: &str,
        patterns: Vec<String>,
    ) -> io::Result<()> {
        validate_tool_rule(&self.permissions_path(root, scope), tool, &patterns)?;
        self.update(root, scope, |permissions| {
            permissions.add_rule(Rule {
                tool: tool.to_string(),
                patterns,
            })
        })?;
        Ok(())
    }

    pub fn add_dir(&self, root: &str, scope: PersistenceScope, dir: &str) -> io::Result<()> {
        validate_directory(&self.permissions_path(root, scope), dir)?;
        self.update(root, scope, |permissions| permissions.add_dir(dir))?;
        Ok(())
    }

    pub fn add_grant(
        &self,
        root: &str,
        scope: PersistenceScope,
        grant: PermissionGrant,
    ) -> io::Result<()> {
        self.add_grants(root, scope, &[grant])
    }

    pub fn add_grants(
        &self,
        root: &str,
        scope: PersistenceScope,
        grants: &[PermissionGrant],
    ) -> io::Result<()> {
        let path = self.permissions_path(root, scope);
        for grant in grants {
            validate_grant(&path, grant)?;
        }
        self.update(root, scope, |permissions| {
            let mut changed = false;
            for grant in grants {
                changed = permissions.add_grant(grant) || changed;
            }
            changed
        })?;
        Ok(())
    }

    /// Remove one exact persisted entry. `pattern == "*"` removes a blanket
    /// tool approval represented by an empty pattern list.
    pub fn remove(
        &self,
        root: &str,
        scope: PersistenceScope,
        tool: &str,
        pattern: &str,
    ) -> io::Result<bool> {
        self.update(root, scope, |permissions| permissions.remove(tool, pattern))
    }
}

fn path_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

fn load_path(path: &Path) -> io::Result<Snapshot> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Snapshot::default()),
        Err(error) => return Err(path_error("read", path, error)),
    };
    let store: Store = serde_json::from_str(&contents).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parse {}: {error}", path.display()),
        )
    })?;
    if store.revision > i64::MAX as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("parse {}: permission revision is too large", path.display()),
        ));
    }
    let permissions = PermissionSet::from_rules(path, store.rules)?;
    Ok(Snapshot {
        revision: store.revision,
        rules: permissions.rules,
    })
}

fn validate_rules(path: &Path, rules: &[Rule]) -> io::Result<()> {
    for rule in rules {
        if rule.tool.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "parse {}: permission tool name cannot be empty",
                    path.display()
                ),
            ));
        }
        if rule.tool == "directory" && rule.patterns.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "parse {}: permission directory rule cannot be empty",
                    path.display()
                ),
            ));
        }
        for pattern in &rule.patterns {
            if rule.tool == "directory" {
                if pattern.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "parse {}: permission directory cannot be empty",
                            path.display()
                        ),
                    ));
                }
            } else if let Err(error) = glob::Pattern::new(pattern) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "parse {}: invalid pattern for {}: {error}",
                        path.display(),
                        rule.tool
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn validate_tool_rule(path: &Path, tool: &str, patterns: &[String]) -> io::Result<()> {
    validate_rules(
        path,
        &[Rule {
            tool: tool.to_string(),
            patterns: patterns.to_vec(),
        }],
    )
}

fn validate_directory(path: &Path, dir: &str) -> io::Result<()> {
    validate_rules(
        path,
        &[Rule {
            tool: "directory".into(),
            patterns: vec![dir.to_string()],
        }],
    )
}

fn validate_grant(path: &Path, grant: &PermissionGrant) -> io::Result<()> {
    match grant {
        PermissionGrant::Tool { tool } => validate_tool_rule(path, tool, &[]),
        PermissionGrant::Command { tool, pattern } => {
            validate_tool_rule(path, tool, std::slice::from_ref(pattern))
        }
        PermissionGrant::PathPrefix { dir } => validate_directory(path, &dir.to_string_lossy()),
    }
}

fn next_revision(revision: u64) -> io::Result<u64> {
    let revision = revision
        .checked_add(1)
        .ok_or_else(|| io::Error::other("permission store revision overflow"))?;
    if revision > i64::MAX as u64 {
        return Err(io::Error::other("permission store revision overflow"));
    }
    Ok(revision)
}

fn save_path(path: &Path, revision: u64, permissions: &PermissionSet) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(&Store {
        revision,
        rules: permissions.rules.clone(),
    })
    .map_err(|error| io::Error::other(format!("serialize {}: {error}", path.display())))?;
    crate::fs::write_atomic(path, &json).map_err(|error| path_error("write", path, error))
}

struct StoreLock {
    _file: std::fs::File,
}

impl StoreLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| path_error("create directory", parent, error))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| path_error("open lock", path, error))?;
        file.lock()
            .map_err(|error| path_error("lock", path, error))?;
        Ok(Self { _file: file })
    }
}

impl PermissionSet {
    fn from_rules(path: &Path, rules: Vec<Rule>) -> io::Result<Self> {
        validate_rules(path, &rules)?;
        let mut permissions = Self::default();
        for rule in rules {
            permissions.add_rule(rule);
        }
        permissions.normalize_order();
        Ok(permissions)
    }

    fn normalize_order(&mut self) {
        for rule in &mut self.rules {
            rule.patterns.sort();
        }
        self.rules.sort_by(|left, right| left.tool.cmp(&right.tool));
    }

    fn add_rule(&mut self, rule: Rule) -> bool {
        if rule.tool == "directory" {
            let mut changed = false;
            for dir in &rule.patterns {
                changed = self.add_dir(dir) || changed;
            }
            return changed;
        }
        self.add_tool(&rule.tool, rule.patterns)
    }

    fn add_grant(&mut self, grant: &PermissionGrant) -> bool {
        match grant {
            PermissionGrant::Tool { tool } => self.add_tool(tool, Vec::new()),
            PermissionGrant::Command { tool, pattern } => {
                self.add_tool(tool, vec![pattern.clone()])
            }
            PermissionGrant::PathPrefix { dir } => self.add_dir(&dir.to_string_lossy()),
        }
    }

    fn add_tool(&mut self, tool: &str, mut patterns: Vec<String>) -> bool {
        if patterns.iter().any(|pattern| pattern == "*") {
            patterns.clear();
        } else {
            patterns.sort();
            patterns.dedup();
        }
        if let Some(existing) = self.rules.iter_mut().find(|rule| rule.tool == tool) {
            if existing.patterns.is_empty() {
                return false;
            }
            if patterns.is_empty() {
                existing.patterns.clear();
                return true;
            }
            let mut changed = false;
            for pattern in patterns {
                if !existing.patterns.contains(&pattern) {
                    existing.patterns.push(pattern);
                    changed = true;
                }
            }
            return changed;
        }
        self.rules.push(Rule {
            tool: tool.to_string(),
            patterns,
        });
        true
    }

    fn add_dir(&mut self, dir: &str) -> bool {
        if let Some(existing) = self.rules.iter_mut().find(|rule| rule.tool == "directory") {
            if existing.patterns.iter().any(|pattern| pattern == dir) {
                return false;
            }
            existing.patterns.push(dir.to_string());
            return true;
        }
        self.rules.push(Rule {
            tool: "directory".into(),
            patterns: vec![dir.to_string()],
        });
        true
    }

    fn remove(&mut self, tool: &str, pattern: &str) -> bool {
        let Some(index) = self.rules.iter().position(|rule| rule.tool == tool) else {
            return false;
        };
        if self.rules[index].patterns.is_empty() {
            if pattern != "*" {
                return false;
            }
            self.rules.remove(index);
            return true;
        }
        let Some(pattern_index) = self.rules[index]
            .patterns
            .iter()
            .position(|existing| existing == pattern)
        else {
            return false;
        };
        self.rules[index].patterns.remove(pattern_index);
        if self.rules[index].patterns.is_empty() {
            self.rules.remove(index);
        }
        true
    }

    fn compile(&self) -> io::Result<CompiledApprovals> {
        let mut compiled = CompiledApprovals::default();
        for rule in &self.rules {
            if rule.tool == "directory" {
                compiled
                    .dirs
                    .extend(rule.patterns.iter().map(PathBuf::from));
                continue;
            }
            let patterns = rule
                .patterns
                .iter()
                .map(|pattern| {
                    glob::Pattern::new(pattern).map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid pattern for {}: {error}", rule.tool),
                        )
                    })
                })
                .collect::<io::Result<Vec<_>>>()?;
            compiled.tools.insert(rule.tool.clone(), patterns);
        }
        Ok(compiled)
    }
}

#[cfg(test)]
pub(crate) fn compile_rules(rules: &[Rule]) -> io::Result<CompiledApprovals> {
    PermissionSet::from_rules(Path::new("permission rules"), rules.to_vec())?.compile()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_set_canonicalizes_duplicate_rows_and_patterns() {
        let permissions = PermissionSet::from_rules(
            Path::new("permissions.json"),
            vec![
                Rule {
                    tool: "directory".into(),
                    patterns: vec!["/tmp".into()],
                },
                Rule {
                    tool: "bash".into(),
                    patterns: vec!["git status".into()],
                },
                Rule {
                    tool: "directory".into(),
                    patterns: vec!["/var/tmp".into(), "/tmp".into()],
                },
                Rule {
                    tool: "bash".into(),
                    patterns: vec!["git diff".into(), "git status".into()],
                },
            ],
        )
        .unwrap();

        assert_eq!(
            permissions.rules,
            vec![
                Rule {
                    tool: "bash".into(),
                    patterns: vec!["git diff".into(), "git status".into()],
                },
                Rule {
                    tool: "directory".into(),
                    patterns: vec!["/tmp".into(), "/var/tmp".into()],
                },
            ]
        );
    }

    #[test]
    fn permission_set_blanket_tool_wins_across_duplicate_rows() {
        let permissions = PermissionSet::from_rules(
            Path::new("permissions.json"),
            vec![
                Rule {
                    tool: "bash".into(),
                    patterns: vec!["git status".into()],
                },
                Rule {
                    tool: "bash".into(),
                    patterns: Vec::new(),
                },
            ],
        )
        .unwrap();

        assert_eq!(
            permissions.rules,
            vec![Rule {
                tool: "bash".into(),
                patterns: Vec::new(),
            }]
        );
    }

    #[test]
    fn workspace_repository_and_sibling_stores_are_independent() {
        let state = tempfile::tempdir().unwrap();
        let dirs = tempfile::tempdir().unwrap();
        let workspace = dirs.path().join("feature");
        let repository = dirs.path().join("repo");
        let sibling = dirs.path().join("other-feature");
        let store = PermissionStore::new(state.path().to_path_buf());
        store
            .add_tool(
                &workspace.to_string_lossy(),
                PersistenceScope::Workspace,
                "bash",
                vec!["cargo test *".into()],
            )
            .unwrap();
        store
            .add_tool(
                &repository.to_string_lossy(),
                PersistenceScope::Repository,
                "bash",
                vec!["git status".into()],
            )
            .unwrap();
        store
            .add_tool(
                &sibling.to_string_lossy(),
                PersistenceScope::Workspace,
                "bash",
                vec!["rm *".into()],
            )
            .unwrap();

        assert_eq!(
            store
                .load(&workspace.to_string_lossy(), PersistenceScope::Workspace)
                .unwrap()[0]
                .patterns,
            vec!["cargo test *"]
        );
        assert_eq!(
            store
                .load(&repository.to_string_lossy(), PersistenceScope::Repository,)
                .unwrap()[0]
                .patterns,
            vec!["git status"]
        );
        assert_eq!(
            store
                .load(&sibling.to_string_lossy(), PersistenceScope::Workspace)
                .unwrap()[0]
                .patterns,
            vec!["rm *"]
        );
    }

    #[test]
    fn repository_rules_are_distinct_from_main_checkout_rules() {
        let state = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = repository.path().to_string_lossy();
        store
            .add_tool(
                &root,
                PersistenceScope::Workspace,
                "bash",
                vec!["checkout-only *".into()],
            )
            .unwrap();
        store
            .add_grant(
                &root,
                PersistenceScope::Repository,
                PermissionGrant::Command {
                    tool: "bash".into(),
                    pattern: "repository-wide *".into(),
                },
            )
            .unwrap();

        assert_eq!(
            store.load(&root, PersistenceScope::Workspace).unwrap()[0].patterns,
            vec!["checkout-only *"]
        );
        assert_eq!(
            store.load(&root, PersistenceScope::Repository).unwrap()[0].patterns,
            vec!["repository-wide *"]
        );
    }

    #[test]
    fn concurrent_additions_preserve_every_grant() {
        use std::sync::{Arc, Barrier};

        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = Arc::new(PermissionStore::new(state.path().to_path_buf()));
        let root = workspace.path().to_string_lossy().into_owned();
        let writers = 16;
        let barrier = Arc::new(Barrier::new(writers));
        let threads: Vec<_> = (0..writers)
            .map(|index| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let root = root.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.add_tool(
                        &root,
                        PersistenceScope::Workspace,
                        "bash",
                        vec![format!("command-{index} *")],
                    )
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }

        let rules = store.load(&root, PersistenceScope::Workspace).unwrap();
        let patterns = &rules
            .iter()
            .find(|rule| rule.tool == "bash")
            .unwrap()
            .patterns;
        assert_eq!(patterns.len(), writers);
        for index in 0..writers {
            assert!(patterns.contains(&format!("command-{index} *")));
        }
    }

    #[test]
    fn legacy_store_without_revision_loads_at_zero_and_upgrades_on_mutation() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = workspace.path().to_string_lossy();
        let path = store.permissions_path(&root, PersistenceScope::Workspace);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"rules":[{"tool":"bash","patterns":["git status"]}]}"#,
        )
        .unwrap();

        assert_eq!(
            store
                .load_snapshot(&root, PersistenceScope::Workspace)
                .unwrap()
                .revision,
            0
        );
        store
            .add_tool(
                &root,
                PersistenceScope::Workspace,
                "bash",
                vec!["git diff".into()],
            )
            .unwrap();
        let snapshot = store
            .load_snapshot(&root, PersistenceScope::Workspace)
            .unwrap();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.rules[0].patterns, vec!["git diff", "git status"]);
    }

    #[test]
    fn stale_replacement_is_rejected_without_discarding_newer_grants() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = workspace.path().to_string_lossy();
        let stale = store
            .load_snapshot(&root, PersistenceScope::Workspace)
            .unwrap();

        store
            .add_tool(
                &root,
                PersistenceScope::Workspace,
                "bash",
                vec!["cargo test *".into()],
            )
            .unwrap();
        let error = store
            .replace(
                &root,
                PersistenceScope::Workspace,
                Replacement {
                    expected_revision: stale.revision,
                    rules: stale.rules,
                },
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("reload and retry"));
        assert_eq!(
            store
                .load_snapshot(&root, PersistenceScope::Workspace)
                .unwrap(),
            Snapshot {
                revision: 1,
                rules: vec![Rule {
                    tool: "bash".into(),
                    patterns: vec!["cargo test *".into()],
                }],
            }
        );
    }

    #[test]
    fn replacements_increment_revision_only_when_canonical_state_changes() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = workspace.path().to_string_lossy();
        let rules = vec![
            Rule {
                tool: "bash".into(),
                patterns: vec!["git status".into(), "git status".into()],
            },
            Rule {
                tool: "bash".into(),
                patterns: vec!["git diff".into()],
            },
        ];

        assert_eq!(
            store
                .replace(
                    &root,
                    PersistenceScope::Workspace,
                    Replacement {
                        expected_revision: 0,
                        rules,
                    },
                )
                .unwrap(),
            1
        );
        let canonical = store
            .load_snapshot(&root, PersistenceScope::Workspace)
            .unwrap();
        assert_eq!(canonical.revision, 1);
        assert_eq!(canonical.rules.len(), 1);
        assert_eq!(canonical.rules[0].patterns, vec!["git diff", "git status"]);
        assert_eq!(
            store
                .replace(
                    &root,
                    PersistenceScope::Workspace,
                    Replacement {
                        expected_revision: canonical.revision,
                        rules: canonical.rules,
                    },
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn malformed_store_fails_closed_without_being_overwritten() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = workspace.path().to_string_lossy();
        let path = store.permissions_path(&root, PersistenceScope::Workspace);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();

        let load_error = store.load(&root, PersistenceScope::Workspace).unwrap_err();
        assert_eq!(load_error.kind(), io::ErrorKind::InvalidData);
        let add_error = store
            .add_tool(
                &root,
                PersistenceScope::Workspace,
                "bash",
                vec!["git status".into()],
            )
            .unwrap_err();
        assert_eq!(add_error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{not json");
    }

    #[test]
    fn invalid_glob_is_rejected_instead_of_becoming_blanket_approval() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = workspace.path().to_string_lossy();

        let error = store
            .replace(
                &root,
                PersistenceScope::Workspace,
                Replacement {
                    expected_revision: 0,
                    rules: vec![Rule {
                        tool: "bash".into(),
                        patterns: vec!["[".into()],
                    }],
                },
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(store
            .load(&root, PersistenceScope::Workspace)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn persistence_failures_are_returned() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = workspace.path().to_string_lossy();
        let lock_path = store.lock_path(&root, PersistenceScope::Workspace);
        std::fs::create_dir_all(&lock_path).unwrap();

        let error = store
            .add_tool(
                &root,
                PersistenceScope::Workspace,
                "bash",
                vec!["git status".into()],
            )
            .unwrap_err();

        assert!(error.to_string().contains("open lock"));
        assert!(error.to_string().contains("permissions.lock"));
    }

    #[test]
    fn exact_removal_preserves_other_rules() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = PermissionStore::new(state.path().to_path_buf());
        let root = workspace.path().to_string_lossy();
        store
            .add_tool(
                &root,
                PersistenceScope::Workspace,
                "bash",
                vec!["git status".into(), "cargo test *".into()],
            )
            .unwrap();
        store
            .add_tool(
                &root,
                PersistenceScope::Workspace,
                "web_fetch",
                vec!["https://example.com/*".into()],
            )
            .unwrap();

        assert!(store
            .remove(&root, PersistenceScope::Workspace, "bash", "git status")
            .unwrap());
        assert!(!store
            .remove(&root, PersistenceScope::Workspace, "bash", "missing")
            .unwrap());

        assert_eq!(
            store.load(&root, PersistenceScope::Workspace).unwrap(),
            vec![
                Rule {
                    tool: "bash".into(),
                    patterns: vec!["cargo test *".into()],
                },
                Rule {
                    tool: "web_fetch".into(),
                    patterns: vec!["https://example.com/*".into()],
                },
            ]
        );
    }

    #[test]
    fn blanket_rule_wins_across_duplicate_tool_rows() {
        let rules = vec![
            Rule {
                tool: "bash".into(),
                patterns: Vec::new(),
            },
            Rule {
                tool: "bash".into(),
                patterns: vec!["git status".into()],
            },
        ];

        let compiled = compile_rules(&rules).unwrap();

        assert!(compiled.tools.get("bash").unwrap().is_empty());
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
