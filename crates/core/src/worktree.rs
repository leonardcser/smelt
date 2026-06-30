use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct WorktreeSpec<'a> {
    pub name: Option<&'a str>,
    pub base: Option<&'a str>,
    pub root: Option<&'a Path>,
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub name: String,
    pub branch: String,
    pub path: PathBuf,
    pub base: String,
    pub created: bool,
}

#[derive(Debug, Clone)]
pub struct ManagedWorktreeInfo {
    pub name: String,
    pub branch: String,
    pub path: PathBuf,
    pub base: String,
    pub current: bool,
}

#[derive(Debug, Clone)]
pub struct ManagedWorktreeContext {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
    pub base_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub project_name: String,
    pub active_root: PathBuf,
    pub branch: String,
    pub managed_worktree: bool,
    pub worktree_name: Option<String>,
    pub base_path: Option<PathBuf>,
    pub allowed_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct GitWorktree {
    path: PathBuf,
    branch: Option<String>,
}

#[derive(Debug, Clone)]
enum ResolvedWorktreeRoot {
    RepoLocal(PathBuf),
    External(PathBuf),
}

const ADJECTIVES: &[&str] = &[
    "amber", "ancient", "arctic", "autumn", "bold", "brave", "bright", "calm", "cedar", "clear",
    "clever", "cobalt", "cosmic", "crimson", "daring", "dawn", "deep", "eager", "ember", "fabled",
    "fast", "fierce", "forest", "gentle", "golden", "grand", "green", "hidden", "honest", "icy",
    "iron", "jade", "keen", "kind", "lively", "lunar", "misty", "modern", "nimble", "noble",
    "opal", "orange", "quiet", "rapid", "red", "river", "royal", "ruby", "sage", "silent",
    "silver", "solar", "steady", "stone", "swift", "tidal", "true", "velvet", "vivid", "warm",
    "wild", "winter", "wise",
];

const NOUNS: &[&str] = &[
    "anchor", "badger", "beacon", "bison", "bridge", "brook", "canyon", "cedar", "comet", "coral",
    "crane", "delta", "ember", "falcon", "field", "fjord", "forest", "harbor", "heron", "island",
    "jaguar", "lantern", "maple", "meadow", "meteor", "otter", "owl", "panda", "pine", "prairie",
    "quartz", "raven", "reef", "ridge", "river", "rocket", "sable", "salmon", "shadow", "sparrow",
    "spring", "summit", "sunset", "thicket", "tiger", "valley", "violet", "voyage", "willow",
    "wolf", "zephyr",
];

pub fn sanitize_name(input: &str) -> Option<String> {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.trim().chars().flat_map(char::to_lowercase) {
        let dash = ch.is_whitespace() || matches!(ch, '-' | '_' | '.' | '/');
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if dash && !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 64 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() || out == "." || out == ".." {
        None
    } else {
        Some(out)
    }
}

pub fn generated_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = nanos ^ ((std::process::id() as u64) << 32);
    let a = ADJECTIVES[(seed as usize) % ADJECTIVES.len()];
    let b = ADJECTIVES[((seed >> 11) as usize) % ADJECTIVES.len()];
    let n = NOUNS[((seed >> 22) as usize) % NOUNS.len()];
    format!("{a}-{b}-{n}")
}

pub fn enter_or_create(cwd: &Path, spec: WorktreeSpec<'_>) -> Result<WorktreeInfo, String> {
    let root = engine::paths::git_root(cwd).ok_or_else(|| "not in a git repository".to_string())?;
    let requested = match spec.name {
        Some(name) => {
            Some(sanitize_name(name).ok_or_else(|| format!("invalid worktree name: {name:?}"))?)
        }
        None => None,
    };
    let base_name = requested.unwrap_or_else(generated_name);
    let default_base = default_base_ref(&root);
    let base = spec.base.unwrap_or(&default_base).trim();
    let base = if base.is_empty() {
        default_base.as_str()
    } else {
        base
    };

    let resolved_root = resolve_worktree_root(spec.root)?;
    ensure_worktrees_excluded(&root, &resolved_root);
    let dir = worktree_parent_dir(&root, &resolved_root);
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    for attempt in 0..1000 {
        let name = if attempt == 0 {
            base_name.clone()
        } else {
            format!("{base_name}-{attempt}")
        };
        let path = dir.join(&name);
        let branch = name.clone();

        if path.exists() {
            if is_git_worktree(&path) {
                return Ok(WorktreeInfo {
                    name,
                    branch: current_branch(&path).unwrap_or(branch),
                    path,
                    base: base.to_string(),
                    created: false,
                });
            }
            if spec.name.is_some() {
                return Err(format!(
                    "{} already exists and is not a git worktree",
                    path.display()
                ));
            }
            continue;
        }

        let branch_exists = git_success(
            &root,
            ["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        );
        if spec.name.is_none() && branch_exists {
            continue;
        }
        let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
        if !branch_exists {
            args.push("-b".into());
            args.push(branch.clone());
        }
        args.push(path.display().to_string());
        args.push(if branch_exists {
            branch.clone()
        } else {
            base.to_string()
        });

        match git_output(&root, args.iter().map(String::as_str)) {
            Ok(()) => {
                return Ok(WorktreeInfo {
                    name,
                    branch,
                    path,
                    base: base.to_string(),
                    created: true,
                });
            }
            Err(_) if spec.name.is_none() && branch_exists => continue,
            Err(e) => return Err(e),
        }
    }

    Err(format!(
        "could not find an unused worktree name for {base_name}"
    ))
}

pub fn list_managed(cwd: &Path, root: Option<&Path>) -> Result<Vec<ManagedWorktreeInfo>, String> {
    let active_root = worktree_root(cwd)?;
    let active_root = std::fs::canonicalize(&active_root).unwrap_or(active_root);
    let base = default_base_ref(&active_root);
    let mut out = Vec::new();

    for wt in git_worktrees(&active_root)? {
        let path = std::fs::canonicalize(&wt.path).unwrap_or(wt.path);
        if !is_managed_worktree_path(&path, root) {
            continue;
        }
        let branch = wt
            .branch
            .or_else(|| current_branch(&path))
            .unwrap_or_else(|| "HEAD".to_string());
        let name = path_name(&path).unwrap_or_else(|| branch.clone());
        out.push(ManagedWorktreeInfo {
            name,
            branch,
            current: path == active_root,
            path,
            base: base.clone(),
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn managed_context(cwd: &Path, root: Option<&Path>) -> Option<ManagedWorktreeContext> {
    let path = worktree_root(cwd).ok()?;
    if !is_managed_worktree_path(&path, root) {
        return None;
    }

    let branch = current_branch(&path).unwrap_or_else(|| "HEAD".into());
    let base = default_base_ref(&path);
    let base_path = git_worktrees(&path)
        .ok()
        .and_then(|worktrees| find_branch_worktree(&worktrees, &base))
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    let path = std::fs::canonicalize(&path).unwrap_or(path);

    Some(ManagedWorktreeContext {
        path,
        branch,
        base,
        base_path,
    })
}

pub fn project_context(cwd: &Path, root: Option<&Path>) -> ProjectContext {
    let git_root = worktree_root(cwd).ok();
    let active_root = git_root
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .unwrap_or_else(|| std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()));
    let branch = current_branch(&active_root).unwrap_or_default();
    let base = default_base_ref(&active_root);
    let worktrees = git_worktrees(&active_root).unwrap_or_default();
    let base_path = find_branch_worktree(&worktrees, &base)
        .or_else(|| common_dir_repo_root(&active_root))
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    let managed_worktree = is_managed_worktree_path(&active_root, root);
    let project_root = base_path.as_deref().unwrap_or(active_root.as_path());
    let project_name = path_name(project_root)
        .or_else(|| path_name(&active_root))
        .unwrap_or_else(|| active_root.display().to_string());

    let mut allowed_roots = Vec::new();
    for wt in worktrees {
        push_unique_path(&mut allowed_roots, wt.path);
    }
    push_unique_path(&mut allowed_roots, active_root.clone());
    if let Some(base_path) = base_path.clone() {
        push_unique_path(&mut allowed_roots, base_path);
    }

    ProjectContext {
        project_name,
        active_root,
        branch: branch.clone(),
        managed_worktree,
        worktree_name: managed_worktree
            .then(|| branch.clone())
            .filter(|name| !name.is_empty()),
        base_path,
        allowed_roots,
    }
}

fn path_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    let normalized = std::fs::canonicalize(&path).unwrap_or(path);
    if !paths.iter().any(|existing| {
        std::fs::canonicalize(existing).unwrap_or_else(|_| existing.clone()) == normalized
    }) {
        paths.push(normalized);
    }
}

fn is_git_worktree(path: &Path) -> bool {
    git_success(path, ["rev-parse", "--is-inside-work-tree"])
}

fn current_branch(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch == "HEAD" || branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

fn git_success<const N: usize>(cwd: &Path, args: [&str; N]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_output<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    git_command(cwd, args, std::iter::empty::<(&str, &str)>()).map(|_| ())
}

fn git_stdout<'a>(cwd: &Path, args: impl IntoIterator<Item = &'a str>) -> Result<String, String> {
    git_command(cwd, args, std::iter::empty::<(&str, &str)>())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_command<'a, 'b>(
    cwd: &Path,
    args: impl IntoIterator<Item = &'a str>,
    env: impl IntoIterator<Item = (&'b str, &'b str)>,
) -> Result<std::process::Output, String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    for (k, v) in env {
        command.env(k, v);
    }
    let output = command.output().map_err(|e| format!("git: {e}"))?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(if stderr.is_empty() { stdout } else { stderr })
}

fn worktree_root(cwd: &Path) -> Result<PathBuf, String> {
    git_stdout(cwd, ["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn default_base_ref(root: &Path) -> String {
    if git_success(root, ["rev-parse", "--verify", "refs/heads/main"]) {
        "main".into()
    } else if git_success(root, ["rev-parse", "--verify", "refs/heads/master"]) {
        "master".into()
    } else {
        "HEAD".into()
    }
}

fn resolve_worktree_root(root: Option<&Path>) -> Result<ResolvedWorktreeRoot, String> {
    let raw = root
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new(".worktrees"));
    let expanded = crate::path::expand(raw)
        .map_err(|e| format!("invalid worktree_root {:?}: {e}", raw.display().to_string()))?;
    if expanded.is_absolute() {
        if expanded.parent().is_none() {
            return Err(format!(
                "invalid worktree_root {:?}: external root may not be filesystem root",
                raw.display().to_string()
            ));
        }
        return Ok(ResolvedWorktreeRoot::External(expanded));
    }
    if expanded == Path::new(".") {
        return Err(format!(
            "invalid worktree_root {:?}: relative root may not be repository root",
            raw.display().to_string()
        ));
    }
    if expanded
        .components()
        .next()
        .is_some_and(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "invalid worktree_root {:?}: relative root may not escape the repository",
            raw.display().to_string()
        ));
    }
    Ok(ResolvedWorktreeRoot::RepoLocal(expanded))
}

fn worktree_parent_dir(repo_root: &Path, root: &ResolvedWorktreeRoot) -> PathBuf {
    match root {
        ResolvedWorktreeRoot::RepoLocal(path) => repo_root.join(path),
        ResolvedWorktreeRoot::External(path) => path.join(repo_bucket(repo_root)),
    }
}

fn repo_bucket(repo_root: &Path) -> String {
    let canonical = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let name = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(sanitize_name)
        .unwrap_or_else(|| "repo".to_string());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    format!("{name}-{}", &hash[..12])
}

fn is_managed_worktree_path(path: &Path, root: Option<&Path>) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    match resolve_worktree_root(root) {
        Ok(ResolvedWorktreeRoot::External(root)) => {
            parent
                .parent()
                .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
                == Some(std::fs::canonicalize(&root).unwrap_or(root))
        }
        Ok(ResolvedWorktreeRoot::RepoLocal(root)) => parent_ends_with(parent, &root),
        Err(_) => false,
    }
}

fn parent_ends_with(parent: &Path, suffix: &Path) -> bool {
    let parent_components: Vec<_> = parent.components().collect();
    let suffix_components: Vec<_> = suffix.components().collect();
    !suffix_components.is_empty() && parent_components.ends_with(&suffix_components)
}

fn git_worktrees(cwd: &Path) -> Result<Vec<GitWorktree>, String> {
    let out = git_stdout(cwd, ["worktree", "list", "--porcelain"])?;
    let mut worktrees = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;

    let push = |worktrees: &mut Vec<GitWorktree>,
                path: &mut Option<PathBuf>,
                branch: &mut Option<String>| {
        if let Some(path) = path.take() {
            worktrees.push(GitWorktree {
                path,
                branch: branch.take(),
            });
        }
    };

    for line in out.lines() {
        if line.trim().is_empty() {
            push(&mut worktrees, &mut path, &mut branch);
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            push(&mut worktrees, &mut path, &mut branch);
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            branch = Some(rest.to_string());
        }
    }
    push(&mut worktrees, &mut path, &mut branch);
    Ok(worktrees)
}

fn common_dir_repo_root(cwd: &Path) -> Option<PathBuf> {
    let raw = git_stdout(cwd, ["rev-parse", "--git-common-dir"]).ok()?;
    let common = {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    };
    let common = std::fs::canonicalize(&common).unwrap_or(common);
    if common.file_name().and_then(|name| name.to_str()) == Some(".git") {
        common.parent().map(Path::to_path_buf)
    } else {
        common
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    }
}

fn find_branch_worktree(worktrees: &[GitWorktree], branch: &str) -> Option<PathBuf> {
    worktrees
        .iter()
        .find(|wt| wt.branch.as_deref() == Some(branch))
        .map(|wt| wt.path.clone())
}

fn ensure_worktrees_excluded(root: &Path, worktree_root: &ResolvedWorktreeRoot) {
    let ResolvedWorktreeRoot::RepoLocal(worktree_root) = worktree_root else {
        return;
    };
    let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(root)
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return;
    }
    let git_dir = {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            root.join(p)
        }
    };
    let info = git_dir.join("info");
    let exclude = info.join("exclude");
    let _ = fs::create_dir_all(&info);
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    let pattern = format!("{}/", worktree_root.display());
    if existing.lines().any(|line| line.trim() == pattern) {
        return;
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&pattern);
    next.push('\n');
    let _ = fs::write(exclude, next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["branch", "-M", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        fs::write(dir.path().join("README.md"), "hello\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn sanitize_lowercases_and_dash_separates() {
        assert_eq!(
            sanitize_name("My New Feature"),
            Some("my-new-feature".into())
        );
        assert_eq!(sanitize_name("../Bad/Name!!"), Some("bad-name".into()));
    }

    #[test]
    fn sanitize_rejects_empty_names() {
        assert_eq!(sanitize_name("!!!"), None);
        assert_eq!(sanitize_name("   "), None);
    }

    #[test]
    fn enter_or_create_creates_safe_named_worktree() {
        let repo = repo();
        let info = enter_or_create(
            repo.path(),
            WorktreeSpec {
                name: Some("My Feature"),
                base: None,
                root: None,
            },
        )
        .unwrap();
        assert_eq!(info.name, "my-feature");
        assert_eq!(info.branch, "my-feature");
        assert!(info.created);
        assert!(info.path.ends_with(".worktrees/my-feature"));
        assert!(info.path.join("README.md").exists());
        assert!(fs::read_to_string(repo.path().join(".git/info/exclude"))
            .unwrap()
            .contains(".worktrees/"));
    }

    #[test]
    fn enter_or_create_rejects_invalid_explicit_name() {
        let repo = repo();
        let err = enter_or_create(
            repo.path(),
            WorktreeSpec {
                name: Some("!!!"),
                base: None,
                root: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("invalid worktree name"));
    }

    #[test]
    fn worktree_root_rejects_relative_escape() {
        assert!(resolve_worktree_root(Some(Path::new("..")))
            .unwrap_err()
            .contains("may not escape"));
        assert!(resolve_worktree_root(Some(Path::new("../worktrees")))
            .unwrap_err()
            .contains("may not escape"));
    }

    #[test]
    fn worktree_root_normalizes_safe_relative_parent_components() {
        let root = resolve_worktree_root(Some(Path::new("foo/../worktrees"))).unwrap();
        assert!(
            matches!(root, ResolvedWorktreeRoot::RepoLocal(ref p) if p == Path::new("worktrees"))
        );
    }

    #[test]
    fn worktree_root_expands_env_vars() {
        std::env::set_var("SMELT_WORKTREE_ROOT_TEST", "/tmp/smelt-worktrees");
        let root =
            resolve_worktree_root(Some(Path::new("$SMELT_WORKTREE_ROOT_TEST/nested"))).unwrap();
        assert!(
            matches!(root, ResolvedWorktreeRoot::External(ref p) if p == Path::new("/tmp/smelt-worktrees/nested"))
        );
    }

    #[test]
    fn worktree_root_errors_for_missing_env_vars() {
        std::env::remove_var("SMELT_WORKTREE_ROOT_TEST_MISSING");
        let err = resolve_worktree_root(Some(Path::new("$SMELT_WORKTREE_ROOT_TEST_MISSING")))
            .unwrap_err();
        assert!(err.contains("SMELT_WORKTREE_ROOT_TEST_MISSING"));
    }

    #[test]
    fn worktree_root_rejects_filesystem_root() {
        assert!(resolve_worktree_root(Some(Path::new("/")))
            .unwrap_err()
            .contains("filesystem root"));
    }

    #[test]
    fn enter_or_create_uses_custom_relative_root() {
        let repo = repo();
        let custom_root = Path::new(".agent/worktrees");
        let info = enter_or_create(
            repo.path(),
            WorktreeSpec {
                name: Some("Feature"),
                base: None,
                root: Some(custom_root),
            },
        )
        .unwrap();

        assert!(info.path.ends_with(".agent/worktrees/feature"));
        assert!(managed_context(&info.path, Some(custom_root)).is_some());
        assert!(fs::read_to_string(repo.path().join(".git/info/exclude"))
            .unwrap()
            .contains(".agent/worktrees/"));
    }

    #[test]
    fn enter_or_create_uses_repo_bucket_for_absolute_root() {
        let repo = repo();
        let external = tempfile::tempdir().unwrap();
        let info = enter_or_create(
            repo.path(),
            WorktreeSpec {
                name: Some("Feature"),
                base: None,
                root: Some(external.path()),
            },
        )
        .unwrap();

        assert!(info.path.starts_with(external.path()));
        assert_eq!(info.path.parent().unwrap().parent(), Some(external.path()));
        assert!(info.path.ends_with("feature"));
        assert!(managed_context(&info.path, Some(external.path())).is_some());
    }

    #[test]
    fn managed_context_detects_managed_worktree() {
        let repo = repo();
        let info = enter_or_create(
            repo.path(),
            WorktreeSpec {
                name: Some("Feature"),
                base: Some("main"),
                root: None,
            },
        )
        .unwrap();

        let ctx = managed_context(&info.path, None).unwrap();

        assert_eq!(ctx.branch, "feature");
        assert_eq!(ctx.base, "main");
        let repo_path = std::fs::canonicalize(repo.path()).unwrap();
        assert_eq!(ctx.base_path.as_deref(), Some(repo_path.as_path()));
        assert!(ctx.path.ends_with(".worktrees/feature"));
        assert!(managed_context(repo.path(), None).is_none());
    }

    #[test]
    fn project_context_for_non_git_directory_has_no_branch() {
        let dir = tempfile::tempdir().unwrap();

        let ctx = project_context(dir.path(), None);

        assert_eq!(
            ctx.project_name,
            dir.path().file_name().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(ctx.branch, "");
        assert!(!ctx.managed_worktree);
        assert_eq!(ctx.worktree_name, None);
    }

    #[test]
    fn managed_project_context_uses_base_project_name() {
        let repo = repo();
        let info = enter_or_create(
            repo.path(),
            WorktreeSpec {
                name: Some("Feature"),
                base: Some("main"),
                root: None,
            },
        )
        .unwrap();

        let ctx = project_context(&info.path, None);

        assert_eq!(
            ctx.project_name,
            repo.path().file_name().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(ctx.branch, "feature");
        assert!(ctx.managed_worktree);
        assert_eq!(ctx.worktree_name.as_deref(), Some("feature"));
        assert!(ctx
            .allowed_roots
            .iter()
            .any(|path| path == &std::fs::canonicalize(repo.path()).unwrap()));
        assert!(ctx
            .allowed_roots
            .iter()
            .any(|path| path == &std::fs::canonicalize(&info.path).unwrap()));
    }

    #[test]
    fn project_context_allows_non_managed_git_worktree_family() {
        let repo = repo();
        let sibling = tempfile::tempdir().unwrap();
        let worktree_path = sibling.path().join("feature-tree");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree_path.to_str().unwrap(),
            ],
        );

        let ctx = project_context(&worktree_path, None);

        assert_eq!(
            ctx.project_name,
            repo.path().file_name().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(ctx.branch, "feature");
        assert!(!ctx.managed_worktree);
        assert_eq!(ctx.worktree_name, None);
        assert!(ctx
            .allowed_roots
            .iter()
            .any(|path| path == &std::fs::canonicalize(repo.path()).unwrap()));
        assert!(ctx
            .allowed_roots
            .iter()
            .any(|path| path == &std::fs::canonicalize(&worktree_path).unwrap()));
    }
}
