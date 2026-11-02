//! Watches a Git repository for changes that can alter its working diff:
//! tracked and non-ignored files, plus the index, HEAD and refs. Reads,
//! untracked ignored paths, lock files and other Git internals are dropped, so loading
//! a diff, which only reads the repository, never wakes the watcher.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{Match, WalkBuilder};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Directories of one Git work tree. Linked worktrees keep their index and
/// HEAD in `git_dir` and share refs through `common_dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    global_ignore: Option<PathBuf>,
}

impl Repository {
    pub fn discover(cwd: &Path) -> Result<Self, String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-common-dir",
            ])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("git: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        let mut lines = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| PathBuf::from(crate::git::path_bytes(line.to_vec())));
        match (lines.next(), lines.next(), lines.next()) {
            (Some(root), Some(git_dir), Some(common_dir)) => Ok(Self {
                root,
                git_dir,
                common_dir,
                global_ignore: ignore::gitignore::gitconfig_excludes_path(),
            }),
            _ => Err("not inside a Git work tree".into()),
        }
    }

    fn origin(&self, path: &Path) -> Origin {
        if self.global_ignore.as_deref() == Some(path) {
            return Origin::Git;
        }
        if path.starts_with(&self.git_dir) || path.starts_with(&self.common_dir) {
            let name = path.file_name().unwrap_or_default();
            if name.as_encoded_bytes().ends_with(b".lock") {
                return Origin::Skip;
            }
            let parent = path.parent();
            let state = (parent == Some(&self.git_dir) && (name == "index" || name == "HEAD"))
                || (parent == Some(&self.common_dir) && name == "packed-refs")
                || path.starts_with(self.common_dir.join("refs"))
                || path == self.common_dir.join("info/exclude")
                || path == self.common_dir.join("info");
            return if state { Origin::Git } else { Origin::Skip };
        }
        match path.strip_prefix(&self.root) {
            Ok(relative) if !relative.components().any(|c| c.as_os_str() == ".git") => Origin::Tree,
            _ => Origin::Skip,
        }
    }
}

enum Origin {
    /// Index, HEAD or a ref.
    Git,
    /// A path in the work tree.
    Tree,
    Skip,
}

/// Stops the watcher thread on drop.
pub struct RepositoryWatcher {
    tx: mpsc::SyncSender<Message>,
    stopped: Arc<AtomicBool>,
}

impl Drop for RepositoryWatcher {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.tx.try_send(Message::Shutdown);
    }
}

enum Message {
    Event(notify::Event),
    Shutdown,
}

/// Install watches before returning, then coalesce changes off-thread into
/// invalidation notices. Ignored directories cost no watches unless they
/// contain tracked files.
pub fn watch(
    repository: Repository,
    on_change: impl Fn() + Send + 'static,
) -> Result<RepositoryWatcher, String> {
    let (tx, rx) = mpsc::sync_channel(256);
    let events = tx.clone();
    let overflow = Arc::new(AtomicBool::new(false));
    let lost = Arc::clone(&overflow);
    let stopped = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::clone(&stopped);
    let watcher = RecommendedWatcher::new(
        move |result: notify::Result<notify::Event>| {
            let event = match result {
                Ok(event) if matches!(event.kind, EventKind::Access(_)) => return,
                Ok(event) => event,
                Err(_) => notify::Event::new(EventKind::Any),
            };
            if matches!(
                events.try_send(Message::Event(event)),
                Err(mpsc::TrySendError::Full(_))
            ) {
                lost.store(true, Ordering::Release);
            }
        },
        Config::default(),
    )
    .map_err(|error| error.to_string())?;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("smelt-git-watcher".into())
        .spawn(move || {
            let mut state = match WatchState::new(repository, watcher) {
                Ok(state) => state,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            while let Ok(Message::Event(event)) = rx.recv() {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                let mut changed = state.relevant(&event);
                // Bound both memory and callback rate during large edit bursts.
                let deadline = std::time::Instant::now() + Duration::from_millis(150);
                while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
                    match rx.recv_timeout(left) {
                        Ok(Message::Event(event)) => changed |= state.relevant(&event),
                        Ok(Message::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                    }
                }
                if overflow.swap(false, Ordering::AcqRel) {
                    state.rebuild_needed = true;
                    changed = true;
                }
                if state.rebuild_needed {
                    // Retry a transient failure on the next batch of changes.
                    state.rebuild_needed = state.rebuild().is_err();
                }
                if changed && !shutdown.load(Ordering::Acquire) {
                    on_change();
                }
            }
        })
        .map_err(|error| error.to_string())?;
    ready_rx
        .recv()
        .map_err(|_| "Git watcher stopped during setup".to_string())??;
    Ok(RepositoryWatcher { tx, stopped })
}

struct WatchState {
    repository: Repository,
    watcher: RecommendedWatcher,
    ignores: Ignores,
    tracked: HashSet<PathBuf>,
    watched: HashSet<PathBuf>,
    rebuild_needed: bool,
}

impl WatchState {
    fn new(repository: Repository, mut watcher: RecommendedWatcher) -> Result<Self, String> {
        for dir in [&repository.git_dir, &repository.common_dir] {
            watcher
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(|error| error.to_string())?;
        }
        if let Some(parent) = repository
            .global_ignore
            .as_ref()
            .and_then(|path| path.parent())
            .filter(|path| path.exists())
        {
            watcher
                .watch(parent, RecursiveMode::NonRecursive)
                .map_err(|error| error.to_string())?;
        }
        let mut state = Self {
            ignores: Ignores::new(&repository),
            repository,
            watcher,
            tracked: HashSet::new(),
            watched: HashSet::new(),
            rebuild_needed: false,
        };
        state.rebuild()?;
        Ok(state)
    }

    fn rebuild(&mut self) -> Result<(), String> {
        for (name, mode) in [
            ("refs", RecursiveMode::Recursive),
            ("info", RecursiveMode::NonRecursive),
        ] {
            let path = self.repository.common_dir.join(name);
            if path.exists() {
                self.watcher
                    .watch(&path, mode)
                    .map_err(|error| error.to_string())?;
            }
        }
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.repository.root)
            .args(["ls-files", "--cached", "-z"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        self.tracked = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                self.repository
                    .root
                    .join(crate::git::path_bytes(path.to_vec()))
            })
            .collect();
        self.ignores = Ignores::new(&self.repository);
        let mut directories = HashSet::new();
        for path in &self.tracked {
            let mut parent = path.parent();
            while let Some(dir) = parent.filter(|dir| dir.starts_with(&self.repository.root)) {
                if dir.is_dir() {
                    directories.insert(dir.to_path_buf());
                }
                parent = dir.parent();
            }
        }
        let mut builder = WalkBuilder::new(&self.repository.root);
        builder
            .standard_filters(false)
            .hidden(false)
            .parents(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .follow_links(false)
            .filter_entry(|entry| {
                entry.file_type().is_some_and(|kind| kind.is_dir()) && entry.file_name() != ".git"
            });
        for entry in builder.build() {
            let entry = entry.map_err(|error| error.to_string())?;
            directories.insert(entry.into_path());
        }
        for dir in &directories {
            if !self.watched.contains(dir) {
                self.watcher
                    .watch(dir, RecursiveMode::NonRecursive)
                    .map_err(|error| error.to_string())?;
            }
            self.ignores.load(dir);
        }
        for dir in self.watched.difference(&directories) {
            let _ = self.watcher.unwatch(dir);
        }
        self.watched = directories;
        Ok(())
    }

    fn relevant(&mut self, event: &notify::Event) -> bool {
        if matches!(event.kind, EventKind::Access(_)) {
            return false;
        }
        // The backend lost track of changes, so newly created directories
        // may be unwatched as well.
        if event.need_rescan() || event.paths.is_empty() {
            self.rebuild_needed = true;
            return true;
        }
        let mut relevant = false;
        for path in &event.paths {
            if matches!(
                event.kind,
                EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
            ) && self.watched.contains(path)
            {
                let removed: Vec<_> = self
                    .watched
                    .iter()
                    .filter(|dir| dir.starts_with(path))
                    .cloned()
                    .collect();
                for dir in removed {
                    let _ = self.watcher.unwatch(&dir);
                    self.watched.remove(&dir);
                }
                self.rebuild_needed = true;
            }
            match self.repository.origin(path) {
                Origin::Skip => {}
                Origin::Git => {
                    self.rebuild_needed = true;
                    relevant = true;
                }
                Origin::Tree if path.file_name().is_some_and(|name| name == ".gitignore") => {
                    // Rules changed: reload them and watch directories they
                    // no longer ignore.
                    self.rebuild_needed = true;
                    relevant = true;
                }
                Origin::Tree => {
                    let is_dir = path.is_dir();
                    if !self.tracked.contains(path) && self.ignores.ignored(path, is_dir) {
                        continue;
                    }
                    if self.watched.contains(path)
                        || (is_dir
                            && matches!(
                                event.kind,
                                EventKind::Create(_)
                                    | EventKind::Modify(notify::event::ModifyKind::Name(_))
                            ))
                    {
                        self.rebuild_needed = true;
                    }
                    relevant = true;
                }
            }
        }
        relevant
    }
}

/// Gitignore rules for classifying single event paths. Per-directory files
/// are consulted from the deepest directory up, then `info/exclude` and the
/// global excludes file.
struct Ignores {
    root: PathBuf,
    dirs: HashMap<PathBuf, Gitignore>,
    repository_wide: [Gitignore; 2],
}

impl Ignores {
    fn new(repository: &Repository) -> Self {
        let mut exclude = GitignoreBuilder::new(&repository.root);
        exclude.add(repository.common_dir.join("info/exclude"));
        let exclude = exclude.build().unwrap_or_else(|_| Gitignore::empty());
        let (global, _) = GitignoreBuilder::new(&repository.root).build_global();
        Self {
            root: repository.root.clone(),
            dirs: HashMap::new(),
            repository_wide: [exclude, global],
        }
    }

    fn load(&mut self, dir: &Path) {
        let (rules, _) = Gitignore::new(dir.join(".gitignore"));
        if rules.is_empty() {
            self.dirs.remove(dir);
        } else {
            self.dirs.insert(dir.to_path_buf(), rules);
        }
    }

    fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        let decide = |rules: &Gitignore, base: &Path| {
            let relative = path.strip_prefix(base).ok()?;
            match rules.matched_path_or_any_parents(relative, is_dir) {
                Match::Ignore(_) => Some(true),
                Match::Whitelist(_) => Some(false),
                Match::None => None,
            }
        };
        let mut dir = path.parent();
        while let Some(current) = dir.filter(|dir| dir.starts_with(&self.root)) {
            if let Some(decision) = self
                .dirs
                .get(current)
                .and_then(|rules| decide(rules, current))
            {
                return decision;
            }
            dir = current.parent();
        }
        self.repository_wide
            .iter()
            .find_map(|rules| decide(rules, &self.root))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn repository() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "one\n").unwrap();
        let repository = Repository::discover(&root.join("src")).unwrap();
        (dir, repository)
    }

    struct Probe {
        rx: mpsc::Receiver<()>,
        _watcher: RepositoryWatcher,
    }

    impl Probe {
        fn new(repository: Repository) -> Self {
            let (tx, rx) = mpsc::channel();
            let watcher = watch(repository, move || {
                let _ = tx.send(());
            })
            .unwrap();
            Self {
                rx,
                _watcher: watcher,
            }
        }

        fn changed(&self) -> bool {
            self.rx.recv_timeout(Duration::from_secs(3)).is_ok()
        }

        fn quiet(&self) -> bool {
            self.rx.recv_timeout(Duration::from_millis(400)).is_err()
        }
    }

    #[test]
    fn discover_reports_work_tree_and_git_dirs() {
        let (_dir, repository) = repository();
        assert_eq!(repository.git_dir, repository.root.join(".git"));
        assert_eq!(repository.common_dir, repository.root.join(".git"));
        assert!(Repository::discover(Path::new("/")).is_err());
    }

    #[test]
    fn reports_tracked_and_new_files() {
        let (_dir, repository) = repository();
        let root = repository.root.clone();
        let probe = Probe::new(repository);
        std::fs::write(root.join("src/lib.rs"), "two\n").unwrap();
        assert!(probe.changed());
        std::fs::create_dir(root.join("src/nested")).unwrap();
        assert!(probe.changed());
        std::fs::write(root.join("src/nested/new.rs"), "new\n").unwrap();
        assert!(probe.changed());
    }

    #[test]
    fn ignores_reads_ignored_paths_and_git_internals() {
        let (_dir, repository) = repository();
        let root = repository.root.clone();
        let probe = Probe::new(repository);
        std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
        std::fs::write(root.join("target/debug/out"), "x").unwrap();
        std::fs::write(root.join("build.log"), "x").unwrap();
        std::fs::write(root.join(".git/FETCH_HEAD"), "x").unwrap();
        std::fs::write(root.join(".git/index.lock"), "x").unwrap();
        assert!(probe.quiet());
    }

    #[test]
    fn reports_index_and_refs() {
        let (_dir, repository) = repository();
        let root = repository.root.clone();
        let probe = Probe::new(repository);
        git(&root, &["add", "src/lib.rs"]);
        assert!(probe.changed());
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        );
        assert!(probe.changed());
    }

    #[test]
    fn gitignore_edits_take_effect() {
        let (_dir, repository) = repository();
        let root = repository.root.clone();
        let probe = Probe::new(repository);
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        assert!(probe.changed());
        std::fs::write(root.join("target/debug/out"), "x").unwrap();
        assert!(probe.changed());
    }

    #[test]
    fn setup_reports_missing_worktree() {
        let (dir, repository) = repository();
        drop(dir);
        assert!(watch(repository, || {}).is_err());
    }

    #[test]
    fn exclude_edits_and_recreated_directories_are_watched() {
        let (_dir, repository) = repository();
        let root = repository.root.clone();
        let probe = Probe::new(repository);
        std::fs::write(root.join(".git/info/exclude"), "src/\n").unwrap();
        assert!(probe.changed());
        std::fs::write(root.join("src/lib.rs"), "ignored\n").unwrap();
        assert!(probe.quiet());
        std::fs::write(root.join(".git/info/exclude"), "").unwrap();
        assert!(probe.changed());
        std::fs::remove_dir_all(root.join("src")).unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        assert!(probe.changed());
        std::fs::write(root.join("src/lib.rs"), "recreated\n").unwrap();
        assert!(probe.changed());
    }

    #[test]
    fn tracked_files_override_ignore_rules() {
        let (_dir, repository) = repository();
        let root = repository.root.clone();
        std::fs::write(root.join("target/debug/tracked.rs"), "one\n").unwrap();
        let probe = Probe::new(repository);
        git(&root, &["add", "-f", "target/debug/tracked.rs"]);
        assert!(probe.changed());
        std::fs::write(root.join("target/debug/tracked.rs"), "two\n").unwrap();
        assert!(probe.changed());
        std::fs::write(root.join("target/debug/out"), "ignored\n").unwrap();
        assert!(probe.quiet());
    }

    #[test]
    fn linked_worktree_watches_its_own_index_and_shared_refs() {
        let (dir, repository) = repository();
        git(&repository.root, &["add", "."]);
        git(
            &repository.root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ],
        );
        let linked = tempfile::tempdir().unwrap();
        let path = linked.path().join("linked");
        git(
            dir.path(),
            &["worktree", "add", "-qb", "linked", path.to_str().unwrap()],
        );
        let repository = Repository::discover(&path).unwrap();
        assert_ne!(repository.git_dir, repository.common_dir);
        let probe = Probe::new(repository);
        std::fs::write(path.join("src/lib.rs"), "two\n").unwrap();
        assert!(probe.changed());
        git(&path, &["add", "src/lib.rs"]);
        assert!(probe.changed());
    }
}
