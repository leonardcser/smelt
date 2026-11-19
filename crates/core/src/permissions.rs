//! Permission state that the Lua policy reads: the workspace roots, where
//! session artifacts live, path rules, and what the user approved for this
//! session. The policy itself lives in `smelt.permissions.check`.
//!
//! A path rule says what may happen at a path, the same way in every mode:
//! - `allow`: like the workspace. Reads run, writes need `write`.
//! - `ask`: reads and writes need `full`.
//! - `deny`: never, even at `full`.
//!
//! A path with no rule may be read; writing it needs `full`. The rule that
//! names the closest ancestor of a path (or the path itself) wins.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub use protocol::Access;

#[derive(Debug, Default)]
pub struct Permissions {
    roots: Vec<PathBuf>,
    project: Option<PathBuf>,
    artifacts: Option<PathBuf>,
    credentials: Option<PathBuf>,
    approvals: HashMap<String, Access>,
    path_approvals: Vec<(PathBuf, Access)>,
}

impl Permissions {
    /// Permissions that treat `dir`, where smelt keeps its logins, as `ask`.
    pub fn with_credentials(dir: PathBuf) -> Self {
        Self {
            credentials: Some(dir),
            ..Self::default()
        }
    }

    /// Use the roots of `project` as the workspace: the active root, every
    /// worktree of its repository, and the shared Git directory.
    pub fn set_project(&mut self, project: &smelt_worktree::ProjectContext) {
        let candidates = project
            .allowed_roots
            .iter()
            .chain(std::iter::once(&project.active_root))
            .chain(project.repository_key.iter());
        let mut roots: Vec<PathBuf> = Vec::new();
        for root in candidates.filter_map(|root| std::fs::canonicalize(root).ok()) {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        self.roots = roots;
        self.project = std::fs::canonicalize(&project.active_root).ok();
    }

    /// The workspace roots, canonical.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Treat everything beneath `dir` as session artifacts, such as plans.
    /// Writing them needs only `read` access.
    pub fn set_artifacts(&mut self, dir: PathBuf) {
        self.artifacts = Some(dir);
    }

    /// The rules for one mode: `explicit` ones, plus `allow` for the
    /// workspace, the temp directories and the session artifacts, `ask` for
    /// smelt's own credentials, and the directories approved this session.
    pub fn rules(&self, explicit: Vec<PathRule>) -> PathRules {
        let implicit = self
            .roots
            .iter()
            .cloned()
            .chain(temp_dirs())
            .chain(self.artifacts.clone())
            .map(|path| PathRule::path(path, Rule::Allow, Layer::Implicit))
            .chain(
                self.credentials
                    .clone()
                    .map(|dir| PathRule::path(dir, Rule::Ask, Layer::Implicit)),
            );
        PathRules {
            rules: implicit.chain(explicit).collect(),
            artifacts: self.artifacts.clone(),
            approved: self.path_approvals.clone(),
            scan_root: self.project.clone(),
        }
    }

    /// Let `tool` run at up to `access` without asking, until the session ends.
    pub fn allow(&mut self, tool: &str, access: Access) {
        let level = self.approvals.entry(tool.to_string()).or_insert(access);
        *level = (*level).max(access);
    }

    /// Whether the user approved `tool` at `access` or more this session.
    pub fn allowed(&self, tool: &str, access: Access) -> bool {
        self.approvals
            .get(tool)
            .is_some_and(|level| *level >= access)
    }

    /// Let calls that touch only paths beneath `dir` run at up to `access`
    /// without asking, until the session ends. `dir` must be absolute.
    pub fn allow_path(&mut self, dir: &Path, access: Access) {
        let dir = real_path(dir);
        match self
            .path_approvals
            .iter_mut()
            .find(|(path, _)| *path == dir)
        {
            Some((_, level)) => *level = (*level).max(access),
            None => self.path_approvals.push((dir, access)),
        }
    }

    /// Whether the user approved a directory holding `path` at `access` or
    /// more this session. `path` must be absolute.
    pub fn allowed_path(&self, path: &Path, access: Access) -> bool {
        let path = real_path(path);
        self.path_approvals
            .iter()
            .any(|(dir, level)| path.starts_with(dir) && *level >= access)
    }

    /// Forget every session approval, for tools and for paths.
    pub fn clear_approvals(&mut self) {
        self.approvals.clear();
        self.path_approvals.clear();
    }
}

/// What a path rule allows; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Allow,
    Ask,
    Deny,
}

impl Rule {
    pub fn parse(rule: &str) -> Option<Self> {
        match rule {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

/// Whether a call reads or writes a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    Read,
    Write,
}

/// The path rules set with `smelt.permissions.paths`: one layer for every
/// mode, and one per mode. Patterns stay as written, so relative ones follow
/// the project when it changes.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    global: BTreeMap<String, Rule>,
    modes: HashMap<String, BTreeMap<String, Rule>>,
}

impl RuleSet {
    /// The layer for `mode`, or the one for every mode.
    pub fn layer_mut(&mut self, mode: Option<&str>) -> &mut BTreeMap<String, Rule> {
        match mode {
            Some(mode) => self.modes.entry(mode.to_string()).or_default(),
            None => &mut self.global,
        }
    }

    /// The rules that apply in `mode`, parsed against `cwd` and `home`.
    pub fn resolve(&self, mode: &str, cwd: &Path, home: &Path) -> Result<Vec<PathRule>, String> {
        let global = self.global.iter().map(|rule| (rule, Layer::Global));
        let moded = self
            .modes
            .get(mode)
            .into_iter()
            .flatten()
            .map(|rule| (rule, Layer::Mode));
        global
            .chain(moded)
            .map(|((pattern, rule), layer)| PathRule::parse(pattern, *rule, layer, cwd, home))
            .collect()
    }
}

/// Where a rule comes from. At the same depth a later layer wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// The workspace, the temp directories, the session artifacts and
    /// smelt's own credentials.
    Implicit,
    /// Rules for every mode.
    Global,
    /// Rules for the current mode only.
    Mode,
}

#[derive(Debug, Clone)]
enum Pattern {
    Path(PathBuf),
    Glob(glob::Pattern),
}

/// One path rule, resolved to an absolute path or glob.
#[derive(Debug, Clone)]
pub struct PathRule {
    pattern: Pattern,
    source_len: usize,
    rule: Rule,
    layer: Layer,
}

impl PathRule {
    /// Parse `pattern`: `~` expands to `home`, a pattern that starts with
    /// `**` matches anywhere, and anything else resolves against `cwd`.
    /// `*`, `?` and `[...]` make it a glob; `**` spans directories.
    pub fn parse(
        pattern: &str,
        rule: Rule,
        layer: Layer,
        cwd: &Path,
        home: &Path,
    ) -> Result<Self, String> {
        let source_len = pattern.len();
        let absolute = if pattern.starts_with("**") {
            format!("/{pattern}")
        } else {
            smelt_paths::path::resolve_from(pattern, cwd, home)
                .to_string_lossy()
                .into_owned()
        };
        let pattern = if absolute.contains(['*', '?', '[']) {
            glob::Pattern::new(&absolute)
                .map(Pattern::Glob)
                .map_err(|error| format!("invalid path pattern `{pattern}`: {error}"))?
        } else {
            Pattern::Path(real_path(Path::new(&absolute)))
        };
        Ok(Self {
            pattern,
            source_len,
            rule,
            layer,
        })
    }

    fn path(path: PathBuf, rule: Rule, layer: Layer) -> Self {
        Self {
            source_len: path.as_os_str().len(),
            pattern: Pattern::Path(real_path(&path)),
            rule,
            layer,
        }
    }

    /// How closely this rule matches `path`: the depth of the ancestor (or
    /// the path itself) it names, then its layer, then the pattern length.
    fn closeness(&self, path: &Path) -> Option<(usize, Layer, usize)> {
        let depth = match &self.pattern {
            Pattern::Path(prefix) => path
                .starts_with(prefix)
                .then(|| prefix.components().count())?,
            Pattern::Glob(glob) => path
                .ancestors()
                .find(|ancestor| glob.matches_path_with(ancestor, GLOB_OPTIONS))
                .map(|ancestor| ancestor.components().count())?,
        };
        Some((depth, self.layer, self.source_len))
    }
}

/// The path rules for one mode, with the directories approved this
/// session.
#[derive(Debug, Clone, Default)]
pub struct PathRules {
    rules: Vec<PathRule>,
    artifacts: Option<PathBuf>,
    approved: Vec<(PathBuf, Access)>,
    /// Where the sandbox looks for files that glob rules hide: the
    /// project unless set with [`PathRules::scanning`].
    scan_root: Option<PathBuf>,
}

impl PathRules {
    /// The rule for `path`, if any rule names it or an ancestor. `path`
    /// must be absolute; symlinks in it are resolved first.
    pub fn rule(&self, path: &Path) -> Option<Rule> {
        self.closest(&real_path(path)).map(|(_, rule)| rule)
    }

    /// The closest rule for the real path `path`, with the depth it names.
    fn closest(&self, path: &Path) -> Option<(usize, Rule)> {
        self.rules
            .iter()
            .filter_map(|rule| rule.closeness(path).map(|key| (key, rule.rule)))
            .max_by_key(|(key, _)| *key)
            .map(|((depth, ..), rule)| (depth, rule))
    }

    /// The directory a session approval for `path` covers: what its `ask`
    /// rule names (`~/.aws`, or the `.env` file itself), or else the
    /// directory that holds it. `None` for a `deny` path.
    pub fn scope(&self, path: &Path) -> Option<PathBuf> {
        let path = real_path(path);
        match self.closest(&path) {
            Some((_, Rule::Deny)) => None,
            Some((depth, Rule::Ask)) => Some(path.components().take(depth).collect()),
            _ if path.is_dir() => Some(path),
            _ => path.parent().map(Path::to_path_buf),
        }
    }

    /// The access a call needs to read or write `path`, or `None` when a
    /// `deny` rule forbids it. Writing a session artifact needs only `read`.
    pub fn need(&self, path: &Path, intent: Intent) -> Option<Access> {
        let rule = self.rule(path);
        let artifact = || {
            self.artifacts
                .as_deref()
                .is_some_and(|dir| real_path(path).starts_with(real_path(dir)))
        };
        Some(match (rule, intent) {
            (Some(Rule::Deny), _) => return None,
            (Some(Rule::Ask), _) => Access::Full,
            (Some(Rule::Allow), Intent::Write) if artifact() => Access::Read,
            (Some(Rule::Allow), Intent::Write) => Access::Write,
            (Some(Rule::Allow) | None, Intent::Read) => Access::Read,
            (None, Intent::Write) => Access::Full,
        })
    }

    /// Search for files that glob rules hide under `root` instead of the
    /// project, for a command that reads only beneath `root`.
    pub fn scanning(mut self, root: &Path) -> Self {
        self.scan_root = Some(real_path(root));
        self
    }

    /// Whether a call at `access` may read `path`.
    pub fn may_read(&self, path: &Path, access: Access) -> bool {
        self.need(path, Intent::Read)
            .is_some_and(|need| need <= access)
    }

    /// The sandbox for a command at `access`. `None` runs it unconfined:
    /// `full` with nothing to hide.
    ///
    /// - `read` may write the temp directories.
    /// - `write` may also write the workspace, `allow` paths and directories
    ///   approved at `write` or more.
    /// - `full` may write anywhere.
    ///
    /// The paths a call at `access` may not read are hidden: `deny` paths,
    /// and `ask` paths below `full` unless approved. Glob rules hide the
    /// files they match in the project when the command starts.
    pub fn sandbox(&self, access: Access) -> Option<smelt_sandbox::Policy> {
        let mut policy = smelt_sandbox::Policy {
            writable: match access {
                Access::Read => Some(temp_dirs()),
                Access::Write => Some(self.writable()),
                Access::Full => None,
            },
            ..Default::default()
        };
        for rule in &self.rules {
            let Pattern::Path(path) = &rule.pattern else {
                continue;
            };
            if self.hides(rule.rule, path, access) {
                policy.hidden.push(path.clone());
            } else if rule.rule == Rule::Allow {
                policy.visible.push(path.clone());
            }
        }
        policy.hidden.extend(self.glob_matches(access));
        policy
            .visible
            .extend(self.approved().map(|(dir, _)| dir.clone()));
        // Only carve-outs from hidden paths matter.
        let hidden = policy.hidden.clone();
        policy
            .visible
            .retain(|path| hidden.iter().any(|hidden| path.starts_with(hidden)));
        (access < Access::Full || !policy.hidden.is_empty()).then_some(policy)
    }

    /// Whether `rule` at `path` keeps a call at `access` from reading it.
    fn hides(&self, rule: Rule, path: &Path, access: Access) -> bool {
        match rule {
            Rule::Deny => true,
            Rule::Ask => {
                access < Access::Full && !self.approved().any(|(dir, _)| path.starts_with(dir))
            }
            Rule::Allow => false,
        }
    }

    /// The files and directories under the scan root that glob rules hide
    /// from a call at `access`. Folders that git ignores are skipped, but
    /// ignored files in the other folders (such as `.env`) are found. The
    /// home directory and `/` are too big to search, so nothing is found
    /// there.
    fn glob_matches(&self, access: Access) -> Vec<PathBuf> {
        let globs: Vec<&glob::Pattern> = self
            .rules
            .iter()
            .filter_map(|rule| match (&rule.pattern, rule.rule) {
                (Pattern::Glob(glob), Rule::Deny) => Some(glob),
                (Pattern::Glob(glob), Rule::Ask) if access < Access::Full => Some(glob),
                _ => None,
            })
            .collect();
        let Some(root) = self.scan_root.as_deref() else {
            return Vec::new();
        };
        let too_big = root.parent().is_none() || real_path(&smelt_paths::home_dir()) == root;
        if globs.is_empty() || too_big {
            return Vec::new();
        }
        // `ignore` skips ignored files, so read each walked directory again
        // to find ignored files such as `.env`. The walk runs on every core.
        let matches = std::sync::Mutex::new(Vec::new());
        ignore::WalkBuilder::new(root)
            .hidden(false)
            .require_git(false)
            // Ignore rules above the root may ignore the root itself.
            .parents(false)
            .filter_entry(|entry| {
                !crate::grep::VCS_DIRS
                    .iter()
                    .any(|dir| entry.file_name() == *dir)
            })
            .build_parallel()
            .run(|| {
                Box::new(|entry| {
                    let Ok(entry) = entry else {
                        return ignore::WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
                        return ignore::WalkState::Continue;
                    }
                    let Ok(children) = std::fs::read_dir(entry.path()) else {
                        return ignore::WalkState::Continue;
                    };
                    let found: Vec<PathBuf> = children
                        .flatten()
                        .map(|child| child.path())
                        .filter(|path| {
                            globs
                                .iter()
                                .any(|glob| glob.matches_path_with(path, GLOB_OPTIONS))
                        })
                        .collect();
                    if !found.is_empty() {
                        matches
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .extend(found);
                    }
                    ignore::WalkState::Continue
                })
            });
        let mut matches = matches
            .into_inner()
            .unwrap_or_else(|error| error.into_inner());
        // Shallowest first, so a hidden directory covers what is inside it.
        matches.sort();
        let mut hidden: Vec<PathBuf> = Vec::new();
        for path in matches {
            if !hidden.iter().any(|outer| path.starts_with(outer))
                && self
                    .closest(&path)
                    .is_some_and(|(_, rule)| self.hides(rule, &path, access))
            {
                hidden.push(path);
            }
        }
        hidden
    }

    /// The directories approved this session, except those a `deny` rule
    /// covers: an approval never opens a `deny` path.
    fn approved(&self) -> impl Iterator<Item = &(PathBuf, Access)> {
        self.approved
            .iter()
            .filter(|(dir, _)| !matches!(self.closest(dir), Some((_, Rule::Deny))))
    }

    /// Every directory a `write` command may change: the `allow` paths
    /// (which include the workspace and the temp directories) and the
    /// directories approved at `write` or more.
    fn writable(&self) -> Vec<PathBuf> {
        let allowed = self.rules.iter().filter_map(|rule| match &rule.pattern {
            Pattern::Path(path) if rule.rule == Rule::Allow => Some(path.clone()),
            _ => None,
        });
        let approved = self
            .approved()
            .filter(|(_, level)| *level >= Access::Write)
            .map(|(dir, _)| dir.clone());
        allowed.chain(approved).collect()
    }
}

/// `*` and `?` stay within one path component; `**` spans them.
const GLOB_OPTIONS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

/// Scratch directories that every sandboxed command may write, even at
/// `read` access: heredocs, `mktemp`, `sort` and compilers need them. On
/// macOS this adds the per-user temp and cache directories, which clang and
/// `xcrun` write.
fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
    dirs.extend(platform_temp_dirs());
    dirs
}

#[cfg(target_os = "macos")]
fn platform_temp_dirs() -> Vec<PathBuf> {
    [
        libc::_CS_DARWIN_USER_TEMP_DIR,
        libc::_CS_DARWIN_USER_CACHE_DIR,
    ]
    .into_iter()
    .filter_map(darwin_dir)
    .collect()
}

#[cfg(not(target_os = "macos"))]
fn platform_temp_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// The directory `confstr` reports for `name`.
#[cfg(target_os = "macos")]
fn darwin_dir(name: libc::c_int) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: `buf` is writable for `buf.len()` bytes.
    let len = unsafe { libc::confstr(name, buf.as_mut_ptr().cast(), buf.len()) };
    if len == 0 || len > buf.len() {
        return None;
    }
    buf.truncate(len - 1);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

/// Canonicalize the longest existing ancestor of `path` and append the
/// rest, so a symlink cannot point a rule's path somewhere else.
fn real_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(current) {
            return missing.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(workspace: &Path, explicit: &[(&str, Rule, Layer)]) -> PathRules {
        let permissions = Permissions {
            roots: vec![workspace.canonicalize().unwrap()],
            ..Default::default()
        };
        let home = Path::new("/home/user");
        let explicit = explicit
            .iter()
            .map(|(pattern, rule, layer)| {
                PathRule::parse(pattern, *rule, *layer, workspace, home).unwrap()
            })
            .collect();
        permissions.rules(explicit)
    }

    #[test]
    fn approvals_cover_their_level_and_below() {
        let mut permissions = Permissions::default();
        permissions.allow("bash", Access::Write);
        assert!(permissions.allowed("bash", Access::Read));
        assert!(permissions.allowed("bash", Access::Write));
        assert!(!permissions.allowed("bash", Access::Full));
        assert!(!permissions.allowed("edit_file", Access::Read));

        permissions.allow("bash", Access::Read);
        assert!(permissions.allowed("bash", Access::Write));

        let dir = tempfile::tempdir().unwrap();
        permissions.allow_path(dir.path(), Access::Write);
        assert!(permissions.allowed_path(&dir.path().join("a/b"), Access::Write));
        assert!(!permissions.allowed_path(&dir.path().join("a"), Access::Full));
        assert!(!permissions.allowed_path(Path::new("/elsewhere"), Access::Read));

        permissions.clear_approvals();
        assert!(!permissions.allowed("bash", Access::Read));
        assert!(!permissions.allowed_path(&dir.path().join("a"), Access::Read));
    }

    #[test]
    fn each_rule_sets_the_access_a_path_needs() {
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        let rules = rules(
            ws,
            &[
                ("~/.aws", Rule::Ask, Layer::Global),
                ("~/.ssh", Rule::Deny, Layer::Global),
                ("~/dev/other", Rule::Allow, Layer::Global),
            ],
        );
        let need = |path: &str, intent| rules.need(Path::new(path), intent);
        assert_eq!(
            rules.need(&ws.join("src/a.rs"), Intent::Write),
            Some(Access::Write)
        );
        assert_eq!(
            need("/home/user/dev/other/a", Intent::Write),
            Some(Access::Write)
        );
        assert_eq!(need("/home/user/notes", Intent::Read), Some(Access::Read));
        assert_eq!(need("/home/user/notes", Intent::Write), Some(Access::Full));
        assert_eq!(
            need("/home/user/.aws/config", Intent::Read),
            Some(Access::Full)
        );
        assert_eq!(need("/home/user/.ssh/id", Intent::Read), None);
        assert_eq!(
            rules.need(&std::env::temp_dir().join("x"), Intent::Write),
            Some(Access::Write)
        );
    }

    #[test]
    fn the_closest_rule_wins() {
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        let rules = rules(
            ws,
            &[
                ("~/.ssh", Rule::Ask, Layer::Global),
                ("~/.ssh/known_hosts", Rule::Allow, Layer::Global),
                ("**/.env", Rule::Ask, Layer::Global),
                ("**/.env.*", Rule::Ask, Layer::Global),
                ("**/.env.example", Rule::Allow, Layer::Global),
                ("~/.aws", Rule::Ask, Layer::Global),
                ("~/.aws", Rule::Deny, Layer::Mode),
            ],
        );
        let rule = |path: &str| rules.rule(Path::new(path));
        assert_eq!(rule("/home/user/.ssh/known_hosts"), Some(Rule::Allow));
        assert_eq!(rule("/home/user/.ssh/id"), Some(Rule::Ask));
        assert_eq!(rules.rule(&ws.join(".env")), Some(Rule::Ask));
        assert_eq!(rules.rule(&ws.join("app/.env.local")), Some(Rule::Ask));
        assert_eq!(rules.rule(&ws.join(".env.example")), Some(Rule::Allow));
        assert_eq!(rules.rule(&ws.join("src/env.rs")), Some(Rule::Allow));
        assert_eq!(rule("/home/user/.aws/config"), Some(Rule::Deny));
        assert_eq!(rule("/elsewhere"), None);

        let scope = |path: &str| rules.scope(Path::new(path));
        assert_eq!(
            scope("/home/user/.ssh/id"),
            Some(PathBuf::from("/home/user/.ssh"))
        );
        assert_eq!(scope("/home/user/.aws/config"), None);
        assert_eq!(
            rules.scope(&ws.join(".env")),
            Some(ws.canonicalize().unwrap().join(".env"))
        );
        assert_eq!(scope("/elsewhere/file"), Some(PathBuf::from("/elsewhere")));
    }

    #[test]
    fn plans_are_writable_at_read_access_and_logins_need_full_access() {
        let artifacts = tempfile::tempdir().unwrap();
        let credentials = tempfile::tempdir().unwrap();
        let mut permissions = Permissions::with_credentials(credentials.path().to_path_buf());
        permissions.set_artifacts(artifacts.path().to_path_buf());
        let rules = permissions.rules(Vec::new());
        assert_eq!(
            rules.need(&artifacts.path().join("plan.md"), Intent::Write),
            Some(Access::Read)
        );
        assert_eq!(
            rules.need(&credentials.path().join("codex.json"), Intent::Read),
            Some(Access::Full)
        );
    }

    #[test]
    fn the_sandbox_hides_what_the_access_cannot_read() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let aws = real_path(&home.path().join(".aws"));
        let ssh = real_path(&home.path().join(".ssh"));
        let ws = workspace.path().canonicalize().unwrap();
        let explicit = [
            (aws.to_str().unwrap(), Rule::Ask, Layer::Global),
            (ssh.to_str().unwrap(), Rule::Deny, Layer::Global),
        ];
        let rules = rules(&ws, &explicit);

        let read = rules.sandbox(Access::Read).unwrap();
        assert_eq!(read.writable, Some(temp_dirs()));
        assert_eq!(read.hidden, vec![aws.clone(), ssh.clone()]);

        let write = rules.sandbox(Access::Write).unwrap();
        assert!(write.writable.unwrap().contains(&ws));

        let full = rules.sandbox(Access::Full).unwrap();
        assert_eq!(full.hidden, vec![ssh.clone()]);
        assert!(full.writable.is_none());

        assert!(self::rules(&ws, &[]).sandbox(Access::Full).is_none());
    }

    #[test]
    fn approvals_open_ask_paths_but_never_deny_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let aws = real_path(&home.path().join(".aws"));
        let ssh = real_path(&home.path().join(".ssh"));
        let keys = ssh.join("keys");
        let mut permissions = Permissions {
            roots: vec![workspace.path().canonicalize().unwrap()],
            ..Default::default()
        };
        permissions.allow_path(&aws, Access::Full);
        permissions.allow_path(&keys, Access::Full);
        let parse = |path: &Path, rule| {
            PathRule::parse(
                path.to_str().unwrap(),
                rule,
                Layer::Global,
                workspace.path(),
                home.path(),
            )
            .unwrap()
        };
        let rules = permissions.rules(vec![parse(&aws, Rule::Ask), parse(&ssh, Rule::Deny)]);

        let write = rules.sandbox(Access::Write).unwrap();
        assert_eq!(write.hidden, vec![ssh]);
        assert!(!write.visible.contains(&keys));
        assert!(!write.writable.unwrap().contains(&keys));
        assert!(!rules.may_read(&keys.join("id"), Access::Full));
    }

    #[test]
    fn glob_rules_hide_their_matches_in_the_project() {
        // Outside the temp directories, which are always "allow".
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        let workspace = tempfile::tempdir_in(target).unwrap();
        let ws = workspace.path().canonicalize().unwrap();
        std::fs::create_dir_all(ws.join("app")).unwrap();
        std::fs::create_dir_all(ws.join("node_modules/pkg")).unwrap();
        std::fs::write(ws.join(".gitignore"), ".env\nnode_modules\n").unwrap();
        for file in [".env", "app/.env", ".env.example", "node_modules/pkg/.env"] {
            std::fs::write(ws.join(file), "").unwrap();
        }
        let permissions = Permissions {
            project: Some(ws.clone()),
            ..Default::default()
        };
        let parse =
            |pattern, rule| PathRule::parse(pattern, rule, Layer::Global, &ws, &ws).unwrap();
        let rules = permissions.rules(vec![
            parse("**/.env*", Rule::Ask),
            parse("**/.env.example", Rule::Allow),
        ]);

        let mut hidden = rules.sandbox(Access::Write).unwrap().hidden;
        hidden.sort();
        assert_eq!(hidden, vec![ws.join(".env"), ws.join("app/.env")]);
        assert!(rules.sandbox(Access::Full).is_none());
    }
}
