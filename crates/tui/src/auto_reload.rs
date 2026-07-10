//! Filesystem-driven `/reload` trigger. Watches Lua config inputs
//! (init.lua, plugins, commands, tools, completers, dialogs, runtime
//! overrides) and pushes a debounced wake-up signal into the TUI run loop,
//! which then re-enters [`crate::app::TuiApp::reload_lua_config`].
//!
//! Owned by `TuiApp` so the watcher stays alive for the session and the
//! drop guard tears down the `notify::RecommendedWatcher` on shutdown.

use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{hash_map::DefaultHasher, BTreeMap};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver as StdReceiver, RecvTimeoutError, Sender as StdSender};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Debounce window: events arriving inside this interval are coalesced
/// into a single reload signal. Picked to swallow editor "save → swap
/// tempfile in place" bursts (vim's `:w` typically fires 2–4 events).
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Handle returned by [`spawn`]. Owns the watcher worker; dropping it
/// stops the OS subscription and asks the debouncer thread to exit.
pub struct AutoReloadHandle {
    tx: StdSender<AutoReloadMsg>,
    _worker: std::thread::JoinHandle<()>,
}

impl Drop for AutoReloadHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(AutoReloadMsg::Shutdown);
    }
}

pub(crate) type AutoReloadSetup = Option<(AutoReloadHandle, UnboundedReceiver<()>)>;
pub(crate) type AutoReloadSetupRx = tokio::sync::oneshot::Receiver<(u64, AutoReloadSetup)>;

pub(crate) struct AutoReloadController {
    pub(crate) handle: Option<AutoReloadHandle>,
    pub(crate) events: Option<UnboundedReceiver<()>>,
    pub(crate) setup: Option<AutoReloadSetupRx>,
    pub(crate) start_pending: bool,
    desired: Option<WatchPaths>,
    desired_revision: u64,
    observed_revision: u64,
    last_error: Option<String>,
}

impl AutoReloadController {
    pub(crate) fn new(enabled: bool, paths: WatchPaths) -> Self {
        Self {
            handle: None,
            events: None,
            setup: None,
            start_pending: enabled,
            desired: enabled.then_some(paths),
            desired_revision: u64::from(enabled),
            observed_revision: 0,
            last_error: None,
        }
    }

    pub(crate) fn set_desired(&mut self, enabled: bool, paths: WatchPaths) -> bool {
        let desired = enabled.then_some(paths);
        if self.desired == desired {
            return false;
        }
        self.desired_revision = self.desired_revision.wrapping_add(1);
        self.desired = desired;
        self.setup = None;
        self.events = None;
        self.handle = None;
        self.start_pending = enabled;
        self.last_error = None;
        if !enabled {
            self.observed_revision = self.desired_revision;
        }
        true
    }

    pub(crate) fn start_setup(&mut self) {
        self.start_pending = false;
        if self.handle.is_some() || self.setup.is_some() {
            return;
        }
        let Some(paths) = self.desired.clone() else {
            return;
        };
        let revision = self.desired_revision;
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send((revision, spawn(paths)));
        });
        self.setup = Some(rx);
    }

    pub(crate) fn apply_setup(&mut self, revision: u64, setup: AutoReloadSetup) -> bool {
        self.setup = None;
        if revision != self.desired_revision || self.desired.is_none() {
            return false;
        }
        self.observed_revision = revision;
        if let Some((handle, events)) = setup {
            self.handle = Some(handle);
            self.events = Some(events);
            self.last_error = None;
        } else {
            self.last_error = Some("no committed Lua config root could be watched".into());
        }
        true
    }

    pub(crate) fn status(&self) -> (u64, u64, Option<String>) {
        (
            self.desired_revision,
            self.observed_revision,
            self.last_error.clone(),
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum AutoReloadMsg {
    Event,
    Shutdown,
}

/// Roots the auto-reloader watches. This intentionally covers Lua config
/// development only; prompt inputs (`AGENTS.md`, `SKILL.md`, command-backed
/// skill metadata in `commands/*.md`, and `--system-prompt`) are refreshed by
/// manual `/reload` so instruction changes remain explicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchPaths {
    /// Every root that contributed to the committed Lua generation.
    pub roots: Vec<PathBuf>,
    /// `.smelt/` under cwd. When missing, the parent is watched
    /// non-recursively until the directory appears.
    pub project_config: Option<PathBuf>,
}

impl WatchPaths {
    /// Resolve every watchable root for the running session. Paths that
    /// don't exist on disk are still included - `notify` errors are
    /// logged and skipped per-path so a missing optional root never
    /// disables the entire watcher.
    pub fn discover(cwd: &Path) -> Self {
        let project_config = cwd.join(".smelt");
        Self::from_manifest(
            vec![smelt_core::config::config_dir(), project_config.clone()],
            Some(cwd),
        )
    }

    pub fn from_manifest(mut roots: Vec<PathBuf>, target_cwd: Option<&Path>) -> Self {
        let project_config = target_cwd.map(|cwd| cwd.join(".smelt"));
        if let Some(project_config) = project_config.as_ref() {
            roots.push(project_config.clone());
        }
        roots.sort();
        roots.dedup();
        Self {
            roots,
            project_config,
        }
    }

    fn relevant_roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }
}

/// The Lua config tree whose content can trigger an automatic reload. This is
/// separate from the OS subscriptions: when `.smelt/` does not exist yet we
/// subscribe to its parent non-recursively, but only paths inside these roots
/// are config.
#[derive(Clone, Debug)]
struct ConfigWatchSet {
    roots: Vec<PathBuf>,
}

impl ConfigWatchSet {
    fn from_paths(paths: &WatchPaths) -> Self {
        Self {
            roots: paths.relevant_roots(),
        }
    }

    #[cfg(test)]
    fn from_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// Cheap event-level prefilter. Passing this only means an event is worth
    /// checking after debounce; the snapshot decides whether config changed.
    fn event_may_affect_config(&self, event: &notify::Event) -> bool {
        if ignored_event_kind(&event.kind) {
            return false;
        }
        if event.paths.is_empty() {
            return true;
        }
        event.paths.iter().any(|p| self.path_may_affect_config(p))
    }

    fn path_may_affect_config(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| {
            if root.starts_with(path) {
                return true;
            }
            if !path.starts_with(root) {
                return false;
            }
            path == root || path.is_dir() || is_lua_path(path)
        })
    }

    #[cfg(test)]
    fn is_lua_config_path(&self, path: &Path) -> bool {
        is_lua_path(path) && self.roots.iter().any(|root| path.starts_with(root))
    }
}

/// Content fingerprint of every Lua config file. Notify events are hints only;
/// this snapshot is the source of truth for whether reloadable config changed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfigSnapshot {
    files: BTreeMap<PathBuf, FileSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileSnapshot {
    len: u64,
    hash: u64,
}

impl ConfigSnapshot {
    fn capture(watch_set: &ConfigWatchSet) -> Self {
        let mut files = BTreeMap::new();
        for root in &watch_set.roots {
            collect_lua_files(root, &mut files);
        }
        Self { files }
    }
}

/// Converts debounced filesystem hints into real reload decisions by rescanning
/// the watch set and comparing it to the last accepted snapshot.
#[derive(Clone, Debug)]
struct AutoReloadFilter {
    watch_set: ConfigWatchSet,
    snapshot: ConfigSnapshot,
    project_config: Option<PathBuf>,
    project_config_watched: bool,
}

impl AutoReloadFilter {
    fn new(watch_set: ConfigWatchSet, project_config_watched: bool) -> Self {
        let snapshot = ConfigSnapshot::capture(&watch_set);
        let project_config = watch_set
            .roots
            .iter()
            .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(".smelt"))
            .cloned();
        Self {
            watch_set,
            snapshot,
            project_config,
            project_config_watched,
        }
    }

    fn refresh_project_config_watch(&mut self, mut watch: impl FnMut(&Path) -> bool) {
        if self.project_config_watched {
            return;
        }
        let Some(path) = self.project_config.as_ref() else {
            return;
        };
        if path.is_dir() && watch(path) {
            self.project_config_watched = true;
        }
    }

    fn changed_since_last_scan(&mut self) -> bool {
        let next = ConfigSnapshot::capture(&self.watch_set);
        if next == self.snapshot {
            return false;
        }
        self.snapshot = next;
        true
    }
}

fn ignored_event_kind(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Access(_))
        || matches!(
            kind,
            EventKind::Modify(notify::event::ModifyKind::Metadata(_))
        )
}

fn is_lua_path(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("lua")
}

fn collect_lua_files(path: &Path, files: &mut BTreeMap<PathBuf, FileSnapshot>) {
    let Ok(file_type) = fs::symlink_metadata(path).map(|m| m.file_type()) else {
        return;
    };
    if file_type.is_file() {
        if is_lua_path(path) {
            if let Some(snapshot) = snapshot_file(path) {
                files.insert(path.to_path_buf(), snapshot);
            }
        }
        return;
    }
    if !file_type.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        collect_lua_files(&entry.path(), files);
    }
}

fn snapshot_file(path: &Path) -> Option<FileSnapshot> {
    let bytes = fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(FileSnapshot {
        len: bytes.len() as u64,
        hash: hasher.finish(),
    })
}

/// Spawn the watcher and debouncer. Returns the drop guard plus a
/// receiver that yields `()` each time a debounced filesystem change
/// warrants a reload.
pub fn spawn(paths: WatchPaths) -> Option<(AutoReloadHandle, UnboundedReceiver<()>)> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<AutoReloadMsg>();
    let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let watch_set = ConfigWatchSet::from_paths(&paths);
    let callback_watch_set = watch_set.clone();

    let callback_tx = raw_tx.clone();
    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !callback_watch_set.event_may_affect_config(&event) {
                return;
            }
            let _ = callback_tx.send(AutoReloadMsg::Event);
        },
        Config::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("auto-reload: failed to create watcher: {e}");
            return None;
        }
    };

    let mut subscribed_any = false;
    for path in &paths.roots {
        if paths.project_config.as_ref() == Some(path) {
            continue;
        }
        if try_watch(&mut watcher, path, RecursiveMode::Recursive) {
            subscribed_any = true;
        }
    }
    let mut project_config_watched = false;
    if let Some(p) = paths.project_config.as_ref() {
        if p.exists() {
            if try_watch(&mut watcher, p, RecursiveMode::Recursive) {
                subscribed_any = true;
                project_config_watched = true;
            }
        } else if let Some(parent) = p.parent() {
            if try_watch(&mut watcher, parent, RecursiveMode::NonRecursive) {
                subscribed_any = true;
            }
        }
    }

    if !subscribed_any {
        return None;
    }

    let filter = AutoReloadFilter::new(watch_set, project_config_watched);
    let worker = std::thread::Builder::new()
        .name("smelt-auto-reload".to_string())
        .spawn(move || debounce_loop(raw_rx, signal_tx, filter, watcher))
        .ok()?;

    Some((
        AutoReloadHandle {
            tx: raw_tx,
            _worker: worker,
        },
        signal_rx,
    ))
}

fn try_watch(watcher: &mut RecommendedWatcher, path: &Path, mode: RecursiveMode) -> bool {
    if !path.exists() {
        return false;
    }
    match watcher.watch(path, mode) {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// Keep only events that can plausibly affect Lua config behavior. Drops
/// metadata-only events (atime bumps from `cat`, permission probes from
/// editors) and prompt-input file events (`AGENTS.md`, `SKILL.md`, markdown
/// command skills, system-prompt files) so instruction changes remain explicit
/// through manual `/reload`.
#[cfg(test)]
fn relevant(event: &notify::Event, roots: &[PathBuf]) -> bool {
    ConfigWatchSet::from_roots(roots.to_vec()).event_may_affect_config(event)
}

#[cfg(test)]
fn relevant_path(path: &Path, roots: &[PathBuf]) -> bool {
    ConfigWatchSet::from_roots(roots.to_vec()).is_lua_config_path(path)
}

/// Coalesce a burst of raw events into a single signal. After the first
/// event arrives, wait for [`DEBOUNCE`] of silence before forwarding;
/// any event landing inside the window resets the timer.
fn debounce_loop(
    raw_rx: StdReceiver<AutoReloadMsg>,
    signal_tx: UnboundedSender<()>,
    filter: AutoReloadFilter,
    mut watcher: RecommendedWatcher,
) {
    debounce_loop_inner(raw_rx, signal_tx, filter, |path| {
        try_watch(&mut watcher, path, RecursiveMode::Recursive)
    });
}

fn debounce_loop_inner(
    raw_rx: StdReceiver<AutoReloadMsg>,
    signal_tx: UnboundedSender<()>,
    mut filter: AutoReloadFilter,
    mut watch_project_config: impl FnMut(&Path) -> bool,
) {
    loop {
        match raw_rx.recv() {
            Ok(AutoReloadMsg::Event) => {}
            Ok(AutoReloadMsg::Shutdown) | Err(_) => return,
        }
        loop {
            match raw_rx.recv_timeout(DEBOUNCE) {
                Ok(AutoReloadMsg::Event) => continue,
                Ok(AutoReloadMsg::Shutdown) => return,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        filter.refresh_project_config_watch(&mut watch_project_config);
        if !filter.changed_since_last_scan() {
            continue;
        }
        if signal_tx.send(()).is_err() {
            return;
        }
    }
}

#[cfg(any(test, feature = "harness"))]
pub mod test_support {
    pub struct WatcherSetupControl {
        started: tokio::sync::oneshot::Receiver<()>,
        release: tokio::sync::oneshot::Sender<()>,
    }

    pub struct ControlledWatcherSetup<T> {
        result: T,
        started: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    }

    pub fn controlled_watcher_setup<T>(
        result: T,
    ) -> (WatcherSetupControl, ControlledWatcherSetup<T>) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        (
            WatcherSetupControl {
                started: started_rx,
                release: release_tx,
            },
            ControlledWatcherSetup {
                result,
                started: started_tx,
                release: release_rx,
            },
        )
    }

    impl WatcherSetupControl {
        pub async fn wait_started(self) -> tokio::sync::oneshot::Sender<()> {
            let _ = self.started.await;
            self.release
        }
    }

    impl<T> ControlledWatcherSetup<T> {
        pub async fn complete(self) -> T {
            let _ = self.started.send(());
            let _ = self.release.await;
            self.result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{DataChange, ModifyKind};
    use std::path::Path;

    fn roots() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/home/me/.config/smelt"),
            PathBuf::from("/repo/.smelt"),
        ]
    }

    #[test]
    fn relevant_path_accepts_lua_under_config_roots() {
        let roots = roots();
        assert!(relevant_path(
            Path::new("/home/me/.config/smelt/plugins/foo.lua"),
            &roots
        ));
        assert!(relevant_path(
            Path::new("/repo/.smelt/commands/bar.lua"),
            &roots
        ));
    }

    #[test]
    fn relevant_path_rejects_prompt_inputs_and_unrelated_paths() {
        let roots = roots();
        assert!(!relevant_path(
            Path::new("/repo/.smelt/commands/bar.md"),
            &roots
        ));
        assert!(!relevant_path(Path::new("/proj/AGENTS.md"), &roots));
        assert!(!relevant_path(
            Path::new("/repo/.smelt/skills/x/SKILL.md"),
            &roots
        ));
        assert!(!relevant_path(Path::new("/repo/.smelt/foo.txt"), &roots));
        assert!(!relevant_path(Path::new("/repo/.smelt/notes.json"), &roots));
        assert!(!relevant_path(Path::new("/repo/src/plugin.lua"), &roots));
    }

    #[test]
    fn relevant_event_ignores_access_and_metadata_only_changes() {
        let access = notify::Event {
            kind: EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write,
            )),
            paths: vec![PathBuf::from("init.lua")],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(!relevant(&access, &roots()));

        let metadata = notify::Event {
            kind: EventKind::Modify(ModifyKind::Metadata(notify::event::MetadataKind::Any)),
            paths: vec![PathBuf::from("AGENTS.md")],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(!relevant(&metadata, &roots()));
    }

    #[test]
    fn relevant_event_accepts_rescan_and_relevant_path() {
        let rescan = notify::Event {
            kind: EventKind::Other,
            paths: Vec::new(),
            attrs: notify::event::EventAttributes::default(),
        };
        let roots = roots();
        assert!(relevant(&rescan, &roots));

        let data_change = notify::Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths: vec![PathBuf::from("/repo/.smelt/plugins/init.lua")],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(relevant(&data_change, &roots));
    }

    #[test]
    fn relevant_event_ignores_prompt_input_file_paths() {
        let agents = notify::Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths: vec![PathBuf::from("/repo/.smelt/AGENTS.md")],
            attrs: notify::event::EventAttributes::default(),
        };
        let skill = notify::Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths: vec![PathBuf::from("/repo/.smelt/skills/example/SKILL.md")],
            attrs: notify::event::EventAttributes::default(),
        };

        assert!(!relevant(&agents, &roots()));
        assert!(!relevant(&skill, &roots()));
    }

    #[test]
    fn snapshot_filter_rejects_replayed_unchanged_lua_events() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.lua");
        std::fs::write(&init, "one\n").unwrap();
        let watch_set = ConfigWatchSet::from_roots(vec![dir.path().to_path_buf()]);
        let mut filter = AutoReloadFilter::new(watch_set, false);

        assert!(!filter.changed_since_last_scan());

        std::fs::write(&init, "two\n").unwrap();
        assert!(filter.changed_since_last_scan());
        assert!(!filter.changed_since_last_scan());
    }

    #[test]
    fn snapshot_filter_handles_pathless_rescan_by_comparing_state() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.lua");
        std::fs::write(&init, "before\n").unwrap();
        let roots = vec![dir.path().to_path_buf()];
        let mut filter = AutoReloadFilter::new(ConfigWatchSet::from_roots(roots.clone()), false);
        let rescan = notify::Event {
            kind: EventKind::Other,
            paths: Vec::new(),
            attrs: notify::event::EventAttributes::default(),
        };

        assert!(relevant(&rescan, &roots));
        assert!(!filter.changed_since_last_scan());

        std::fs::write(&init, "after\n").unwrap();
        assert!(filter.changed_since_last_scan());
    }

    #[test]
    fn snapshot_filter_detects_new_project_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project_config = dir.path().join(".smelt");
        let watch_set = ConfigWatchSet::from_roots(vec![project_config.clone()]);
        let mut filter = AutoReloadFilter::new(watch_set.clone(), false);
        let mkdir = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::Folder),
            paths: vec![project_config.clone()],
            attrs: notify::event::EventAttributes::default(),
        };

        assert!(watch_set.event_may_affect_config(&mkdir));
        assert!(!filter.changed_since_last_scan());

        std::fs::create_dir_all(&project_config).unwrap();
        let mut watched = false;
        filter.refresh_project_config_watch(|path| {
            watched = path == project_config;
            true
        });
        assert!(watched);
        assert!(filter.project_config_watched);
        std::fs::write(project_config.join("init.lua"), "created = true\n").unwrap();
        assert!(filter.changed_since_last_scan());
    }

    #[tokio::test]
    async fn watcher_does_not_replay_changes_from_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.lua");
        std::fs::write(&init, "before = true\n").unwrap();

        let (_handle, mut rx) = spawn(WatchPaths {
            roots: vec![dir.path().to_path_buf()],
            project_config: None,
        })
        .expect("watcher starts");

        assert!(tokio::time::timeout(DEBOUNCE * 2, rx.recv()).await.is_err());

        std::fs::write(&init, "after = true\n").unwrap();
        assert!(tokio::time::timeout(DEBOUNCE * 8, rx.recv())
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn debounce_loop_coalesces_bursts() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.lua");
        std::fs::write(&init, "before\n").unwrap();
        let filter = AutoReloadFilter::new(
            ConfigWatchSet::from_roots(vec![dir.path().to_path_buf()]),
            true,
        );
        let (raw_tx, raw_rx) = std::sync::mpsc::channel();
        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = std::thread::spawn(move || {
            debounce_loop_inner(raw_rx, signal_tx, filter, |_| false);
        });

        std::fs::write(&init, "after\n").unwrap();
        raw_tx.send(AutoReloadMsg::Event).unwrap();
        raw_tx.send(AutoReloadMsg::Event).unwrap();
        assert!(tokio::time::timeout(DEBOUNCE / 2, signal_rx.recv())
            .await
            .is_err());
        assert!(tokio::time::timeout(DEBOUNCE * 2, signal_rx.recv())
            .await
            .unwrap()
            .is_some());
        assert!(signal_rx.try_recv().is_err());

        raw_tx.send(AutoReloadMsg::Shutdown).unwrap();
        task.join().unwrap();
    }

    #[test]
    fn stale_setup_cannot_replace_newer_watcher_desired_paths() {
        let old_dir = tempfile::tempdir().unwrap();
        let new_dir = tempfile::tempdir().unwrap();
        let old_paths = WatchPaths {
            roots: vec![old_dir.path().to_path_buf()],
            project_config: None,
        };
        let new_paths = WatchPaths {
            roots: vec![new_dir.path().to_path_buf()],
            project_config: None,
        };
        let mut controller = AutoReloadController::new(true, old_paths.clone());
        let old_revision = controller.desired_revision;
        let old_setup = spawn(old_paths);

        assert!(controller.set_desired(true, new_paths));
        assert!(!controller.apply_setup(old_revision, old_setup));
        assert!(controller.handle.is_none());
        assert_eq!(controller.observed_revision, 0);
    }

    #[test]
    fn equal_watcher_desired_paths_do_not_restart_setup() {
        let dir = tempfile::tempdir().unwrap();
        let paths = WatchPaths {
            roots: vec![dir.path().to_path_buf()],
            project_config: None,
        };
        let mut controller = AutoReloadController::new(true, paths.clone());
        let revision = controller.desired_revision;

        assert!(!controller.set_desired(true, paths));
        assert_eq!(controller.desired_revision, revision);
    }

    #[test]
    fn successful_watcher_revision_clears_the_previous_setup_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = WatchPaths {
            roots: vec![dir.path().to_path_buf()],
            project_config: None,
        };
        let mut controller = AutoReloadController::new(true, paths.clone());
        assert!(controller.apply_setup(controller.desired_revision, None));
        assert!(controller.last_error.is_some());

        controller.set_desired(false, paths.clone());
        controller.set_desired(true, paths.clone());
        let revision = controller.desired_revision;
        assert!(controller.apply_setup(revision, spawn(paths)));
        assert!(controller.last_error.is_none());
    }

    #[tokio::test]
    async fn controlled_watcher_setup_waits_for_explicit_release() {
        let (control, setup) = test_support::controlled_watcher_setup(7_u64);
        let task = tokio::spawn(setup.complete());
        let release = control.wait_started().await;

        assert!(!task.is_finished());
        release.send(()).unwrap();
        assert_eq!(task.await.unwrap(), 7);
    }
}
