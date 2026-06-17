use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;
use notify::{Config as NotifyConfig, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const RESCAN_DEBOUNCE: Duration = Duration::from_millis(150);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(35);
const GIT_LS_FILES_TIMEOUT: Duration = Duration::from_secs(5);
const PARTIAL_PUBLISH_START: usize = 2_000;

const NOISY_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".jj",
    "node_modules",
    "bower_components",
    "jspm_packages",
    "target",
    "build",
    "dist",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".angular",
    ".astro",
    ".parcel-cache",
    ".turbo",
    ".vite",
    "coverage",
    ".nyc_output",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".hypothesis",
    ".venv",
    "venv",
    "env",
    ".gradle",
    ".idea",
    ".metadata",
    ".settings",
    ".classpath",
    ".project",
    ".stack-work",
    "_build",
    "deps",
    "CMakeFiles",
    "cmake-build-debug",
    "cmake-build-release",
    "DerivedData",
    "Pods",
];

#[derive(Debug)]
pub struct WorkspaceFiles {
    projects: HashMap<PathBuf, ProjectSearch>,
    roots: HashMap<PathBuf, PathBuf>,
    git_roots: HashSet<PathBuf>,
    search_worker: SearchWorker,
}

#[derive(Debug)]
struct ProjectSearch {
    root: PathBuf,
    state: SharedProjectState,
    worker_tx: mpsc::Sender<WorkerMsg>,
    _watcher: Option<RecommendedWatcher>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectKind {
    Git,
    Filesystem,
}

#[derive(Debug)]
enum WorkerMsg {
    Rescan,
    Shutdown,
}

struct SearchWorker {
    tx: mpsc::Sender<SearchJob>,
    rx: mpsc::Receiver<SearchCompletion>,
    next_id: u64,
    pending: Option<PendingSearch>,
    completed: Option<SearchCompletion>,
}

#[derive(Clone, Debug)]
struct PendingSearch {
    id: u64,
    key: SearchKey,
}

#[derive(Clone, Debug)]
struct SearchJob {
    id: u64,
    key: SearchKey,
    snapshot: ProjectSnapshot,
}

#[derive(Clone, Debug)]
struct SearchCompletion {
    id: u64,
    key: SearchKey,
    response: SearchResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchKey {
    root: PathBuf,
    query: String,
    limit: usize,
    offset: usize,
    include_dirs: bool,
    generation: u64,
}

impl SearchKey {
    fn same_request(&self, other: &Self) -> bool {
        self.root == other.root
            && self.query == other.query
            && self.limit == other.limit
            && self.offset == other.offset
            && self.include_dirs == other.include_dirs
    }
}

impl fmt::Debug for SearchWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchWorker")
            .field("next_id", &self.next_id)
            .field("pending", &self.pending)
            .field(
                "completed",
                &self.completed.as_ref().map(|search| search.id),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for ProjectSearch {
    fn drop(&mut self) {
        let _ = self.worker_tx.send(WorkerMsg::Shutdown);
    }
}

type SharedProjectState = Arc<RwLock<ProjectState>>;

#[derive(Debug)]
struct ProjectState {
    entries: Arc<Vec<FileEntry>>,
    files: usize,
    dirs: usize,
    scanned: usize,
    scanning: bool,
    watcher_ready: bool,
    warmup_complete: bool,
    last_error: Option<String>,
    generation: u64,
    accepted: HashMap<String, u64>,
    accept_tick: u64,
}

#[derive(Clone, Debug)]
struct FileEntry {
    path: String,
    lower_path: String,
    lower_file_name: String,
    padded_path: String,
    kind: ItemKind,
}

#[derive(Clone, Debug)]
struct ProjectSnapshot {
    entries: Arc<Vec<FileEntry>>,
    files: usize,
    dirs: usize,
    scanned: usize,
    scanning: bool,
    watcher_ready: bool,
    warmup_complete: bool,
    last_error: Option<String>,
    generation: u64,
    accepted: HashMap<String, u64>,
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub cwd: PathBuf,
    pub limit: usize,
    pub offset: usize,
    pub include_dirs: bool,
}

#[derive(Clone, Debug)]
pub struct AcceptRequest {
    pub cwd: PathBuf,
    pub path: String,
}

#[derive(Clone, Debug, Default)]
pub struct SearchResponse {
    pub root: PathBuf,
    pub items: Vec<Item>,
    pub total_matched: usize,
    pub total_files: usize,
    pub total_dirs: usize,
    pub scanned: usize,
    pub scanning: bool,
    pub searching: bool,
    pub ready: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceFilesStatus {
    pub root: PathBuf,
    pub initialized: bool,
    pub files: usize,
    pub scanned: usize,
    pub scanning: bool,
    pub watcher_ready: bool,
    pub warmup_complete: bool,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub label: String,
    pub path: String,
    pub insert_text: String,
    pub kind: ItemKind,
    pub score: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    File,
    Dir,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::File => "file",
            ItemKind::Dir => "dir",
        }
    }
}

impl ProjectState {
    fn new_scanning() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            files: 0,
            dirs: 0,
            scanned: 0,
            scanning: true,
            watcher_ready: false,
            warmup_complete: false,
            last_error: None,
            generation: 0,
            accepted: HashMap::new(),
            accept_tick: 0,
        }
    }
}

impl SearchWorker {
    fn new() -> Self {
        let (tx, rx) = spawn_search_worker();
        Self {
            tx,
            rx,
            next_id: 0,
            pending: None,
            completed: None,
        }
    }

    fn request(&mut self, key: SearchKey, snapshot: ProjectSnapshot) -> SearchResponse {
        self.drain_completed();

        if let Some(completed) = &self.completed {
            if completed.key == key {
                return completed.response.clone();
            }
        }

        let has_pending = self
            .pending
            .as_ref()
            .map(|pending| pending.key == key)
            .unwrap_or(false);
        if !has_pending {
            self.next_id = self.next_id.saturating_add(1);
            let id = self.next_id;
            let job = SearchJob {
                id,
                key: key.clone(),
                snapshot: snapshot.clone(),
            };
            if self.tx.send(job).is_ok() {
                self.pending = Some(PendingSearch {
                    id,
                    key: key.clone(),
                });
            }
        }

        if let Some(completed) = &self.completed {
            if completed.key.same_request(&key) {
                let mut response = completed.response.clone();
                response.scanning = snapshot.scanning;
                response.searching = true;
                response.ready = false;
                return response;
            }
        }

        loading_search_response(&key.root, &snapshot, true)
    }

    fn drain_completed(&mut self) {
        while let Ok(completed) = self.rx.try_recv() {
            if self
                .pending
                .as_ref()
                .map(|pending| pending.id == completed.id)
                .unwrap_or(false)
            {
                self.pending = None;
            }
            self.completed = Some(completed);
        }
    }
}

impl Default for WorkspaceFiles {
    fn default() -> Self {
        Self::new(PathBuf::new())
    }
}

impl WorkspaceFiles {
    pub fn new(_state_dir: PathBuf) -> Self {
        Self {
            projects: HashMap::new(),
            roots: HashMap::new(),
            git_roots: HashSet::new(),
            search_worker: SearchWorker::new(),
        }
    }

    pub fn warmup(&mut self, cwd: &Path) -> Result<(), String> {
        let root = self.workspace_root(cwd);
        self.ensure_project(&root)
    }

    pub fn search(&mut self, request: SearchRequest) -> Result<SearchResponse, String> {
        let _perf = smelt_perf::perf::begin("workspace_files:search");
        let (root, snapshot) = self.snapshot_for_request(&request)?;
        Ok(search_snapshot_response(&root, &snapshot, &request))
    }

    pub fn search_interactive(&mut self, request: SearchRequest) -> Result<SearchResponse, String> {
        let _perf = smelt_perf::perf::begin("workspace_files:search");
        let (root, snapshot) = self.snapshot_for_request(&request)?;
        smelt_perf::perf::record_value("workspace_files:entries", snapshot.entries.len() as u64);
        let key = SearchKey {
            root,
            query: request.query,
            limit: request.limit,
            offset: request.offset,
            include_dirs: request.include_dirs,
            generation: snapshot.generation,
        };
        Ok(self.search_worker.request(key, snapshot))
    }

    pub fn accept(&mut self, request: AcceptRequest) -> Result<(), String> {
        let root = self.workspace_root(&request.cwd);
        self.ensure_project(&root)?;
        let project = self
            .projects
            .get(&root)
            .ok_or_else(|| "workspace files project was not initialized".to_string())?;
        let relative = safe_relative_path(&request.path)?;
        let path = slash_path(&relative).ok_or_else(|| "file accept path is empty".to_string())?;
        let mut state = project.state.write().map_err(|e| e.to_string())?;
        state.accept_tick = state.accept_tick.saturating_add(1);
        let tick = state.accept_tick;
        state.accepted.insert(path, tick);
        state.generation = state.generation.saturating_add(1);
        Ok(())
    }

    pub fn status(&mut self, cwd: &Path) -> Result<WorkspaceFilesStatus, String> {
        let root = self.workspace_root(cwd);
        self.ensure_project(&root)?;
        let project = self
            .projects
            .get(&root)
            .ok_or_else(|| "workspace files project was not initialized".to_string())?;
        let snapshot = project.snapshot()?;
        Ok(WorkspaceFilesStatus {
            root: project.root.clone(),
            initialized: true,
            files: snapshot.files,
            scanned: snapshot.scanned,
            scanning: snapshot.scanning,
            watcher_ready: snapshot.watcher_ready,
            warmup_complete: snapshot.warmup_complete,
        })
    }

    pub fn rescan(&mut self, cwd: &Path) -> Result<(), String> {
        let root = self.workspace_root(cwd);
        self.ensure_project(&root)?;
        let project = self
            .projects
            .get(&root)
            .ok_or_else(|| "workspace files project was not initialized".to_string())?;
        if let Ok(mut state) = project.state.write() {
            state.scanning = true;
            state.last_error = None;
        }
        project
            .worker_tx
            .send(WorkerMsg::Rescan)
            .map_err(|e| e.to_string())
    }

    fn snapshot_for_request(
        &mut self,
        request: &SearchRequest,
    ) -> Result<(PathBuf, ProjectSnapshot), String> {
        let root = {
            let _perf = smelt_perf::perf::begin("workspace_files:root");
            self.workspace_root(&request.cwd)
        };
        self.ensure_project(&root)?;
        let project = self
            .projects
            .get(&root)
            .ok_or_else(|| "workspace files project was not initialized".to_string())?;
        let snapshot = {
            let _perf = smelt_perf::perf::begin("workspace_files:snapshot");
            project.snapshot()?
        };
        Ok((project.root.clone(), snapshot))
    }

    fn ensure_project(&mut self, root: &Path) -> Result<(), String> {
        if self.projects.contains_key(root) {
            return Ok(());
        }

        let state = Arc::new(RwLock::new(ProjectState::new_scanning()));
        let kind = if self.git_roots.contains(root) {
            ProjectKind::Git
        } else {
            ProjectKind::Filesystem
        };
        let worker_tx = spawn_project_worker(root.to_path_buf(), kind, Arc::clone(&state));
        let watcher = start_watcher(root, kind, worker_tx.clone(), Arc::clone(&state));
        self.projects.insert(
            root.to_path_buf(),
            ProjectSearch {
                root: root.to_path_buf(),
                state,
                worker_tx,
                _watcher: watcher,
            },
        );
        Ok(())
    }

    fn workspace_root(&mut self, cwd: &Path) -> PathBuf {
        let cwd = normalize_cwd(cwd);
        if let Some(root) = self.roots.get(&cwd) {
            return root.clone();
        }
        let root = if let Some(root) = engine::paths::git_root(&cwd) {
            self.git_roots.insert(root.clone());
            root
        } else {
            cwd.clone()
        };
        self.roots.insert(cwd, root.clone());
        root
    }
}

impl ProjectSearch {
    fn snapshot(&self) -> Result<ProjectSnapshot, String> {
        let state = self.state.read().map_err(|e| e.to_string())?;
        Ok(ProjectSnapshot {
            entries: Arc::clone(&state.entries),
            files: state.files,
            dirs: state.dirs,
            scanned: state.scanned,
            scanning: state.scanning,
            watcher_ready: state.watcher_ready,
            warmup_complete: state.warmup_complete,
            last_error: state.last_error.clone(),
            generation: state.generation,
            accepted: state.accepted.clone(),
        })
    }
}

fn search_snapshot_response(
    root: &Path,
    snapshot: &ProjectSnapshot,
    request: &SearchRequest,
) -> SearchResponse {
    smelt_perf::perf::record_value("workspace_files:entries", snapshot.entries.len() as u64);
    if let Some(item) = exact_path_item(root, &request.query, request.include_dirs) {
        return exact_path_response(root, snapshot, request, item);
    }

    let ranked = {
        let _perf = smelt_perf::perf::begin("workspace_files:rank");
        rank_entries_window(
            &request.query,
            &snapshot.entries,
            request.include_dirs,
            &snapshot.accepted,
            request.offset,
            request.limit,
        )
    };
    smelt_perf::perf::record_value("workspace_files:matches", ranked.total as u64);
    let items = {
        let _perf = smelt_perf::perf::begin("workspace_files:items");
        ranked
            .entries
            .into_iter()
            .map(|(index, score)| {
                let entry = &snapshot.entries[index];
                search_item(entry.kind, entry.path.clone(), score)
            })
            .collect::<Vec<_>>()
    };

    let message = if items.is_empty() {
        snapshot.last_error.clone().or_else(|| {
            Some(if snapshot.scanning {
                "indexing workspace…".to_string()
            } else {
                "no matches".to_string()
            })
        })
    } else {
        snapshot.last_error.clone()
    };

    SearchResponse {
        root: root.to_path_buf(),
        items,
        total_matched: ranked.total,
        total_files: snapshot.files,
        total_dirs: snapshot.dirs,
        scanned: snapshot.scanned,
        scanning: snapshot.scanning,
        searching: false,
        ready: snapshot.warmup_complete && !snapshot.scanning,
        message,
    }
}

fn exact_path_response(
    root: &Path,
    snapshot: &ProjectSnapshot,
    request: &SearchRequest,
    item: Item,
) -> SearchResponse {
    let items = if request.offset == 0 && request.limit > 0 {
        vec![item]
    } else {
        Vec::new()
    };
    let message = if items.is_empty() {
        snapshot.last_error.clone().or_else(|| {
            Some(if snapshot.scanning {
                "indexing workspace…".to_string()
            } else {
                "no matches".to_string()
            })
        })
    } else {
        snapshot.last_error.clone()
    };

    SearchResponse {
        root: root.to_path_buf(),
        items,
        total_matched: 1,
        total_files: snapshot.files,
        total_dirs: snapshot.dirs,
        scanned: snapshot.scanned,
        scanning: snapshot.scanning,
        searching: false,
        ready: snapshot.warmup_complete && !snapshot.scanning,
        message,
    }
}

fn loading_search_response(
    root: &Path,
    snapshot: &ProjectSnapshot,
    searching: bool,
) -> SearchResponse {
    SearchResponse {
        root: root.to_path_buf(),
        items: Vec::new(),
        total_matched: 0,
        total_files: snapshot.files,
        total_dirs: snapshot.dirs,
        scanned: snapshot.scanned,
        scanning: snapshot.scanning,
        searching,
        ready: false,
        message: Some(if searching {
            "searching files…".to_string()
        } else if snapshot.scanning {
            "indexing workspace…".to_string()
        } else {
            "no matches".to_string()
        }),
    }
}

fn spawn_search_worker() -> (mpsc::Sender<SearchJob>, mpsc::Receiver<SearchCompletion>) {
    let (job_tx, job_rx) = mpsc::channel::<SearchJob>();
    let (completion_tx, completion_rx) = mpsc::channel::<SearchCompletion>();
    std::thread::spawn(move || {
        while let Ok(mut job) = job_rx.recv() {
            loop {
                match job_rx.recv_timeout(SEARCH_DEBOUNCE) {
                    Ok(next) => job = next,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            while let Ok(next) = job_rx.try_recv() {
                job = next;
            }
            let request = SearchRequest {
                query: job.key.query.clone(),
                cwd: job.key.root.clone(),
                limit: job.key.limit,
                offset: job.key.offset,
                include_dirs: job.key.include_dirs,
            };
            let response = search_snapshot_response(&job.key.root, &job.snapshot, &request);
            if completion_tx
                .send(SearchCompletion {
                    id: job.id,
                    key: job.key,
                    response,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (job_tx, completion_rx)
}

fn spawn_project_worker(
    root: PathBuf,
    kind: ProjectKind,
    state: SharedProjectState,
) -> mpsc::Sender<WorkerMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        scan_project(&root, kind, &state);
        while let Ok(WorkerMsg::Rescan) = rx.recv() {
            loop {
                match rx.recv_timeout(RESCAN_DEBOUNCE) {
                    Ok(WorkerMsg::Rescan) => {}
                    Ok(WorkerMsg::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                }
            }
            scan_project(&root, kind, &state);
        }
    });
    tx
}

fn start_watcher(
    root: &Path,
    kind: ProjectKind,
    tx: mpsc::Sender<WorkerMsg>,
    state: SharedProjectState,
) -> Option<RecommendedWatcher> {
    let watch_root = root.to_path_buf();
    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if relevant_event(&watch_root, kind, &event) {
                let _ = tx.send(WorkerMsg::Rescan);
            }
        },
        NotifyConfig::default(),
    ) {
        Ok(watcher) => watcher,
        Err(_) => return None,
    };

    match watcher.watch(root, RecursiveMode::Recursive) {
        Ok(()) => {
            if let Ok(mut state) = state.write() {
                state.watcher_ready = true;
            }
            Some(watcher)
        }
        Err(_) => None,
    }
}

fn scan_project(root: &Path, kind: ProjectKind, state: &SharedProjectState) {
    if let Ok(mut state) = state.write() {
        state.scanning = true;
        state.scanned = 0;
        state.last_error = None;
    }

    let scan = match kind {
        ProjectKind::Git => scan_git_project(root),
        ProjectKind::Filesystem => scan_filesystem_project(root, state),
    };
    publish_scan(
        state,
        ScanPublish {
            entries: scan.entries,
            files: scan.files,
            dirs: scan.dirs,
            scanned: scan.scanned,
            scanning: false,
            warmup_complete: true,
            error: scan.error,
        },
    );
}

struct ScanResult {
    entries: Vec<FileEntry>,
    files: usize,
    dirs: usize,
    scanned: usize,
    error: Option<String>,
}

impl ScanResult {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            files: 0,
            dirs: 0,
            scanned: 0,
            error: None,
        }
    }

    fn finish(mut self) -> Self {
        self.entries.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| kind_rank(a.kind).cmp(&kind_rank(b.kind)))
        });
        self
    }
}

fn scan_git_project(root: &Path) -> ScanResult {
    let tracked = match git_ls_files(root, &["--cached", "--recurse-submodules"])
        .or_else(|_| git_ls_files(root, &["--cached"]))
    {
        Ok(paths) => paths,
        Err(err) => {
            let mut scan = ScanResult::new();
            scan.error = Some(format!("git ls-files failed: {err}"));
            return scan.finish();
        }
    };
    let (untracked, error) = match git_ls_files(root, &["--others", "--exclude-standard"]) {
        Ok(paths) => (paths, None),
        Err(err) => (
            Vec::new(),
            Some(format!("git ls-files --others failed: {err}")),
        ),
    };

    let mut scan = ScanResult::new();
    scan.error = error;
    let mut seen = HashSet::new();
    for path in tracked {
        add_existing_file(root, &mut scan, &mut seen, path);
    }
    for path in untracked {
        if !has_noisy_parent(&path) {
            add_existing_file(root, &mut scan, &mut seen, path);
        }
    }
    scan.finish()
}

fn git_ls_files(root: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let stdout = git_ls_files_stdout(root, args)?;
    Ok(stdout
        .split(|b| *b == 0)
        .filter(|raw| !raw.is_empty())
        .filter_map(|raw| normalize_git_path(&String::from_utf8_lossy(raw)))
        .collect())
}

fn git_ls_files_stdout(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let (stdout_path, stdout_file) = create_git_stdout_file()?;
    let spawn = std::process::Command::new("git")
        .arg("ls-files")
        .args(args)
        .arg("-z")
        .current_dir(root)
        .stdout(std::process::Stdio::from(stdout_file))
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(err) => {
            let _ = std::fs::remove_file(&stdout_path);
            return Err(err.to_string());
        }
    };

    let deadline = Instant::now() + GIT_LS_FILES_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&stdout_path);
                return Err(format!(
                    "timed out after {}s",
                    GIT_LS_FILES_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&stdout_path);
                return Err(err.to_string());
            }
        }
    };

    let stdout = std::fs::read(&stdout_path).map_err(|err| err.to_string());
    let _ = std::fs::remove_file(&stdout_path);
    if !status.success() {
        return Err(status.to_string());
    }
    stdout
}

fn create_git_stdout_file() -> Result<(PathBuf, std::fs::File), String> {
    for attempt in 0..16 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "smelt-git-ls-files-{}-{nanos}-{attempt}.out",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    Err("could not create git ls-files output file".to_string())
}

fn add_existing_file(root: &Path, scan: &mut ScanResult, seen: &mut HashSet<String>, path: String) {
    let full_path = root.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if full_path.is_file() {
        add_parent_dirs(scan, seen, &path);
        add_scan_entry(scan, seen, path, ItemKind::File);
    }
}

fn scan_filesystem_project(root: &Path, state: &SharedProjectState) -> ScanResult {
    let mut scan = ScanResult::new();
    let mut seen = HashSet::new();
    let mut next_publish = PARTIAL_PUBLISH_START;

    let mut builder = WalkBuilder::new(root);
    let scan_root = root.to_path_buf();
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    builder
        .standard_filters(false)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .follow_links(true)
        .filter_entry(move |entry| {
            if entry.path() == scan_root {
                return true;
            }
            let Some(path) = relative_slash_path(&scan_root, entry.path()) else {
                return true;
            };
            let is_noisy_dir = entry
                .file_type()
                .map(|file_type| file_type.is_dir() && is_noisy_relative_path(&path))
                .unwrap_or(false);
            !is_noisy_dir
                && !is_symlink_escape(&canonical_root, entry.path(), entry.path_is_symlink())
        });

    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
                if scan.error.is_none() {
                    scan.error = Some(err.to_string());
                }
                continue;
            }
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if entry.path() == root {
            continue;
        }
        let Some(path) = relative_slash_path(root, entry.path()) else {
            continue;
        };
        if file_type.is_dir() {
            add_scan_entry(&mut scan, &mut seen, path, ItemKind::Dir);
        } else if file_type.is_file() {
            add_scan_entry(&mut scan, &mut seen, path, ItemKind::File);
        }

        if scan.scanned >= next_publish {
            publish_scan(
                state,
                ScanPublish {
                    entries: scan.entries.clone(),
                    files: scan.files,
                    dirs: scan.dirs,
                    scanned: scan.scanned,
                    scanning: true,
                    warmup_complete: false,
                    error: None,
                },
            );
            next_publish = next_publish.saturating_mul(2).max(scan.scanned + 1);
        }
    }

    scan.finish()
}

fn add_parent_dirs(scan: &mut ScanResult, seen: &mut HashSet<String>, path: &str) {
    for (idx, ch) in path.char_indices() {
        if ch == '/' {
            add_scan_entry(scan, seen, path[..idx].to_string(), ItemKind::Dir);
        }
    }
}

fn add_scan_entry(scan: &mut ScanResult, seen: &mut HashSet<String>, path: String, kind: ItemKind) {
    if path.is_empty() || !seen.insert(format!("{}:{path}", kind.as_str())) {
        return;
    }
    match kind {
        ItemKind::File => scan.files += 1,
        ItemKind::Dir => scan.dirs += 1,
    }
    scan.scanned += 1;
    scan.entries.push(FileEntry::new(path, kind));
}

fn normalize_git_path(path: &str) -> Option<String> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return None;
    }
    slash_path(Path::new(path))
}

struct ScanPublish {
    entries: Vec<FileEntry>,
    files: usize,
    dirs: usize,
    scanned: usize,
    scanning: bool,
    warmup_complete: bool,
    error: Option<String>,
}

fn publish_scan(state: &SharedProjectState, update: ScanPublish) {
    if let Ok(mut state) = state.write() {
        state.entries = Arc::new(update.entries);
        state.files = update.files;
        state.dirs = update.dirs;
        state.scanned = update.scanned;
        state.scanning = update.scanning;
        state.warmup_complete |= update.warmup_complete;
        state.last_error = update.error;
        state.generation = state.generation.saturating_add(1);
    }
}

#[derive(Debug)]
struct RankedEntries {
    total: usize,
    entries: Vec<(usize, i32)>,
}

#[cfg(test)]
fn rank_entries(
    query: &str,
    entries: &[FileEntry],
    include_dirs: bool,
    accepted: &HashMap<String, u64>,
) -> Vec<(usize, i32)> {
    rank_entries_window(query, entries, include_dirs, accepted, 0, usize::MAX).entries
}

fn rank_entries_window(
    query: &str,
    entries: &[FileEntry],
    include_dirs: bool,
    accepted: &HashMap<String, u64>,
    offset: usize,
    limit: usize,
) -> RankedEntries {
    let query = query.trim();
    let mut ranked = if query.is_empty() {
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| include_dirs || entry.kind == ItemKind::File)
            .map(|(index, entry)| (index, empty_query_score(entry, accepted)))
            .collect::<Vec<_>>()
    } else {
        let config = neo_frizbee::Config {
            sort: false,
            ..Default::default()
        };
        let query_lower = query.to_lowercase();
        let _perf = smelt_perf::perf::begin("workspace_files:fuzzy_match");
        if include_dirs {
            neo_frizbee::match_list(query, entries, &config)
                .into_iter()
                .map(|m| {
                    let index = m.index as usize;
                    let entry = &entries[index];
                    (
                        index,
                        adjusted_score(entry, m.score as i32, &query_lower, accepted),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            let candidates = entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| (entry.kind == ItemKind::File).then_some(index))
                .collect::<Vec<_>>();
            let haystacks = candidates
                .iter()
                .map(|index| entries[*index].padded_path.as_str())
                .collect::<Vec<_>>();
            neo_frizbee::match_list(query, &haystacks, &config)
                .into_iter()
                .map(|m| {
                    let index = candidates[m.index as usize];
                    let entry = &entries[index];
                    (
                        index,
                        adjusted_score(entry, m.score as i32, &query_lower, accepted),
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    let total = ranked.len();
    {
        let _perf = smelt_perf::perf::begin("workspace_files:sort");
        trim_ranked_window(&mut ranked, entries, query.is_empty(), offset, limit);
    }
    RankedEntries {
        total,
        entries: ranked,
    }
}

fn trim_ranked_window(
    ranked: &mut Vec<(usize, i32)>,
    entries: &[FileEntry],
    empty_query: bool,
    offset: usize,
    limit: usize,
) {
    let top = offset.saturating_add(limit).min(ranked.len());
    if top == 0 {
        ranked.clear();
        return;
    }
    if top < ranked.len() {
        ranked.select_nth_unstable_by(top, |a, b| compare_ranked(a, b, entries, empty_query));
        ranked.truncate(top);
    }
    ranked.sort_by(|a, b| compare_ranked(a, b, entries, empty_query));
    if offset >= ranked.len() {
        ranked.clear();
    } else if offset > 0 {
        ranked.drain(..offset);
    }
    ranked.truncate(limit);
}

fn compare_ranked(
    a: &(usize, i32),
    b: &(usize, i32),
    entries: &[FileEntry],
    empty_query: bool,
) -> Ordering {
    let a_entry = &entries[a.0];
    let b_entry = &entries[b.0];
    let ordering = b.1.cmp(&a.1);
    if empty_query {
        ordering
            .then_with(|| a_entry.path.cmp(&b_entry.path))
            .then_with(|| kind_rank(a_entry.kind).cmp(&kind_rank(b_entry.kind)))
    } else {
        ordering
            .then_with(|| a_entry.path.len().cmp(&b_entry.path.len()))
            .then_with(|| a_entry.path.cmp(&b_entry.path))
            .then_with(|| kind_rank(a_entry.kind).cmp(&kind_rank(b_entry.kind)))
    }
}

fn adjusted_score(
    entry: &FileEntry,
    base_score: i32,
    query_lower: &str,
    accepted: &HashMap<String, u64>,
) -> i32 {
    let mut score = base_score;

    if entry.lower_path == query_lower {
        score += 50_000;
    }
    if entry.lower_file_name == query_lower {
        score += 30_000;
    }
    if entry.lower_path.starts_with(query_lower) {
        score += 15_000;
    }
    if entry.lower_file_name.starts_with(query_lower) {
        score += 10_000;
    }
    if entry
        .lower_path
        .split('/')
        .any(|component| component.starts_with(query_lower))
    {
        score += 5_000;
    }
    if let Some(rank) = accepted.get(&entry.path) {
        score += (*rank).min(50_000) as i32;
    }
    if entry.kind == ItemKind::Dir {
        score -= 50;
    }
    score -= (entry.path.len() as i32).min(2_000);
    score
}

fn empty_query_score(entry: &FileEntry, accepted: &HashMap<String, u64>) -> i32 {
    let mut score = accepted
        .get(&entry.path)
        .map(|rank| (*rank).min(50_000) as i32)
        .unwrap_or_default();
    if entry.kind == ItemKind::Dir {
        score -= 1;
    }
    score
}

fn search_item(kind: ItemKind, path: String, score: i32) -> Item {
    let insert_text = match kind {
        ItemKind::File => path.clone(),
        ItemKind::Dir => format!("{}/", path.trim_end_matches('/')),
    };
    let label = insert_text.clone();
    Item {
        id: format!("{}:{path}", kind.as_str()),
        label,
        path,
        insert_text,
        kind,
        score,
    }
}

fn exact_path_item(root: &Path, query: &str, include_dirs: bool) -> Option<Item> {
    let query = query.trim();
    if query.is_empty() || query.contains("://") {
        return None;
    }
    let relative = safe_relative_path(query).ok()?;
    let path = slash_path(&relative)?;
    let full_path = root.join(&relative);
    let kind = if full_path.is_file() {
        ItemKind::File
    } else if include_dirs && full_path.is_dir() {
        ItemKind::Dir
    } else {
        return None;
    };
    Some(search_item(kind, path, i32::MAX))
}

impl FileEntry {
    fn new(path: String, kind: ItemKind) -> Self {
        let lower_path = path.to_lowercase();
        let lower_file_name = path.rsplit('/').next().unwrap_or(&path).to_lowercase();
        let padded_path = crate::fuzzy::pad_for_simd(&path);
        Self {
            path,
            lower_path,
            lower_file_name,
            padded_path,
            kind,
        }
    }
}

impl neo_frizbee::Matchable for FileEntry {
    fn match_str(&self) -> Option<&str> {
        Some(&self.padded_path)
    }
}

fn safe_relative_path(path: &str) -> Result<PathBuf, String> {
    let trimmed = path.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("file accept path is empty".to_string());
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err("file accept path must be relative to the workspace".to_string());
    }

    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err("file accept path must not contain `..`".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("file accept path must be relative to the workspace".to_string());
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err("file accept path is empty".to_string());
    }
    Ok(relative)
}

fn relative_slash_path(root: &Path, path: &Path) -> Option<String> {
    slash_path(path.strip_prefix(root).ok()?)
}

fn slash_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn kind_rank(kind: ItemKind) -> u8 {
    match kind {
        ItemKind::Dir => 0,
        ItemKind::File => 1,
    }
}

fn relevant_event(root: &Path, kind: ProjectKind, event: &notify::Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    if let EventKind::Modify(notify::event::ModifyKind::Metadata(_)) = event.kind {
        return false;
    }
    match kind {
        ProjectKind::Git => true,
        ProjectKind::Filesystem => {
            event.paths.is_empty()
                || event.paths.iter().any(|path| {
                    relative_slash_path(root, path)
                        .map(|path| !is_noisy_relative_path(&path))
                        .unwrap_or(true)
                })
        }
    }
}

fn is_noisy_relative_path(path: &str) -> bool {
    path.split('/').any(is_noisy_component)
}

fn has_noisy_parent(path: &str) -> bool {
    path.rsplit_once('/')
        .map(|(parent, _)| is_noisy_relative_path(parent))
        .unwrap_or(false)
}

fn is_noisy_component(component: &str) -> bool {
    NOISY_DIRS.contains(&component) || component.starts_with("bazel-")
}

fn is_symlink_escape(canonical_root: &Path, path: &Path, path_is_symlink: bool) -> bool {
    if !path_is_symlink {
        return false;
    }
    std::fs::canonicalize(path)
        .map(|target| !target.starts_with(canonical_root))
        .unwrap_or(true)
}

fn normalize_cwd(cwd: &Path) -> PathBuf {
    let cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(cwd)
    };
    std::fs::canonicalize(&cwd).unwrap_or(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn init_git(dir: &Path) -> bool {
        run_git(dir, &["init", "--quiet"])
    }

    #[test]
    fn safe_relative_path_rejects_workspace_escape() {
        assert_eq!(
            safe_relative_path("src/main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(
            safe_relative_path("./src/main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(safe_relative_path("src/").unwrap(), PathBuf::from("src"));
        assert!(safe_relative_path("/tmp/main.rs").is_err());
        assert!(safe_relative_path("../main.rs").is_err());
        assert!(safe_relative_path("src/../main.rs").is_err());
        assert!(safe_relative_path("").is_err());
    }

    #[test]
    fn search_item_uses_trailing_slash_for_directory_insert_text() {
        let item = search_item(ItemKind::Dir, "src".to_string(), 1);
        assert_eq!(item.path, "src");
        assert_eq!(item.label, "src/");
        assert_eq!(item.insert_text, "src/");
    }

    #[test]
    fn rank_entries_finds_fuzzy_file_matches() {
        let entries = vec![
            FileEntry::new("src/lib.rs".to_string(), ItemKind::File),
            FileEntry::new("docs/readme.md".to_string(), ItemKind::File),
            FileEntry::new("crates/core/src/lua/api/fs.rs".to_string(), ItemKind::File),
        ];
        let ranked = rank_entries("fs", &entries, true, &HashMap::new());
        assert_eq!(entries[ranked[0].0].path, "crates/core/src/lua/api/fs.rs");
    }

    #[test]
    fn new_project_state_reports_scanning_before_worker_publishes() {
        let state = ProjectState::new_scanning();
        assert!(state.scanning);
        assert!(!state.warmup_complete);
    }

    #[test]
    fn noisy_path_filter_is_relative_to_workspace() {
        assert!(is_noisy_relative_path("target/debug/app"));
        assert!(is_noisy_relative_path("src/node_modules/pkg/index.js"));
        assert!(is_noisy_relative_path("web/.next/server/app.js"));
        assert!(is_noisy_relative_path("bazel-smelt/bin/app"));
        assert!(is_noisy_relative_path("ios/DerivedData/build.log"));
        assert!(!is_noisy_relative_path("src/main.rs"));
        assert!(!is_noisy_relative_path("workspace-target/src/main.rs"));
    }

    #[test]
    fn watcher_filters_noisy_paths_only_for_filesystem_projects() {
        let root = PathBuf::from("/workspace");
        let noisy_event = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![root.join("target/kept.txt")],
            attrs: Default::default(),
        };
        assert!(relevant_event(&root, ProjectKind::Git, &noisy_event));
        assert!(!relevant_event(
            &root,
            ProjectKind::Filesystem,
            &noisy_event
        ));

        let source_event = notify::Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![root.join("src/main.rs")],
            attrs: Default::default(),
        };
        assert!(relevant_event(
            &root,
            ProjectKind::Filesystem,
            &source_event
        ));
    }

    #[test]
    fn warmup_starts_background_scan() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("main.rs"), "").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        files.warmup(project.path()).unwrap();
        assert_eq!(files.projects.len(), 1);
        wait_until_ready(&mut files, project.path());
    }

    #[test]
    fn scanner_finds_files_outside_git_repo() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        let response = files
            .search(SearchRequest {
                query: "main".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();

        assert!(
            response.items.iter().any(|item| item.path == "src/main.rs"),
            "response: {response:#?}"
        );
    }

    #[test]
    fn scanner_includes_hidden_project_files() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".github/workflows")).unwrap();
        std::fs::write(project.path().join(".github/workflows/ci.yml"), "").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        let response = files
            .search(SearchRequest {
                query: "ci".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();

        assert!(
            response
                .items
                .iter()
                .any(|item| item.path == ".github/workflows/ci.yml"),
            "response: {response:#?}"
        );
    }

    #[test]
    fn git_scan_includes_tracked_files_inside_ignored_directories() {
        let project = tempfile::tempdir().unwrap();
        if !init_git(project.path()) {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".gitignore"), "tasks/\n").unwrap();
        std::fs::create_dir_all(project.path().join("roles/developer/tasks")).unwrap();
        std::fs::write(
            project.path().join("roles/developer/tasks/main.yml"),
            "---\n",
        )
        .unwrap();
        assert!(run_git(
            project.path(),
            &["add", "-f", "roles/developer/tasks/main.yml"]
        ));

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        for query in ["tasks", "main"] {
            let response = files
                .search(SearchRequest {
                    query: query.to_string(),
                    cwd: project.path().to_path_buf(),
                    limit: 20,
                    offset: 0,
                    include_dirs: false,
                })
                .unwrap();

            assert!(
                response
                    .items
                    .iter()
                    .any(|item| item.path == "roles/developer/tasks/main.yml"),
                "query {query:?} response: {response:#?}"
            );
        }
    }

    #[test]
    fn git_scan_includes_untracked_nonignored_files() {
        let project = tempfile::tempdir().unwrap();
        if !init_git(project.path()) {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join("src/new_file.rs"), "fn main() {}\n").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        let response = files
            .search(SearchRequest {
                query: "new_file".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();

        assert!(
            response
                .items
                .iter()
                .any(|item| item.path == "src/new_file.rs"),
            "response: {response:#?}"
        );
    }

    #[test]
    fn git_scan_prunes_untracked_noisy_dirs_but_keeps_tracked_files() {
        let project = tempfile::tempdir().unwrap();
        if !init_git(project.path()) {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("target/debug")).unwrap();
        std::fs::write(project.path().join("target/debug/generated.log"), "noise\n").unwrap();
        std::fs::write(project.path().join("target/kept.txt"), "tracked\n").unwrap();
        assert!(run_git(project.path(), &["add", "-f", "target/kept.txt"]));

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        let noisy = files
            .search(SearchRequest {
                query: "generated".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();
        assert!(noisy.items.is_empty(), "response: {noisy:#?}");

        let tracked = files
            .search(SearchRequest {
                query: "kept".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();
        assert!(
            tracked
                .items
                .iter()
                .any(|item| item.path == "target/kept.txt"),
            "response: {tracked:#?}"
        );
    }

    #[test]
    fn ignored_untracked_files_require_exact_path() {
        let project = tempfile::tempdir().unwrap();
        if !init_git(project.path()) {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(project.path().join("ignored")).unwrap();
        std::fs::write(project.path().join("ignored/secret.txt"), "secret\n").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        let fuzzy = files
            .search(SearchRequest {
                query: "secret".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();
        assert!(fuzzy.items.is_empty(), "response: {fuzzy:#?}");

        let exact = files
            .search(SearchRequest {
                query: "ignored/secret.txt".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();
        assert_eq!(exact.items[0].path, "ignored/secret.txt");
    }

    #[test]
    fn non_git_scan_ignores_gitignore_files() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".gitignore"), "tasks/\n").unwrap();
        std::fs::create_dir_all(project.path().join("roles/developer/tasks")).unwrap();
        std::fs::write(
            project.path().join("roles/developer/tasks/main.yml"),
            "---\n",
        )
        .unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        let response = files
            .search(SearchRequest {
                query: "main".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();

        assert!(
            response
                .items
                .iter()
                .any(|item| item.path == "roles/developer/tasks/main.yml"),
            "response: {response:#?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scanner_follows_workspace_symlinked_directories() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("shared/tasks")).unwrap();
        std::fs::write(project.path().join("shared/tasks/main.yml"), "---\n").unwrap();
        std::fs::create_dir_all(project.path().join("roles/developer")).unwrap();
        std::os::unix::fs::symlink(
            project.path().join("shared/tasks"),
            project.path().join("roles/developer/tasks"),
        )
        .unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        for query in ["tasks", "main"] {
            let response = files
                .search(SearchRequest {
                    query: query.to_string(),
                    cwd: project.path().to_path_buf(),
                    limit: 20,
                    offset: 0,
                    include_dirs: false,
                })
                .unwrap();

            assert!(
                response
                    .items
                    .iter()
                    .any(|item| item.path == "roles/developer/tasks/main.yml"),
                "query {query:?} response: {response:#?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn scanner_skips_symlinked_directories_outside_workspace() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("leaked.yml"), "---\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join("outside")).unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        let response = files
            .search(SearchRequest {
                query: "leaked".to_string(),
                cwd: project.path().to_path_buf(),
                limit: 20,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();

        assert!(response.items.is_empty(), "response: {response:#?}");
    }

    #[test]
    fn search_finds_files_after_background_scan() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(project.path().join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        std::fs::write(project.path().join("README.md"), "workspace\n").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut last = None;

        let item = loop {
            let response = files
                .search(SearchRequest {
                    query: "main".to_string(),
                    cwd: project.path().to_path_buf(),
                    limit: 20,
                    offset: 0,
                    include_dirs: true,
                })
                .unwrap();
            if let Some(item) = response
                .items
                .iter()
                .find(|item| item.path == "src/main.rs")
                .cloned()
            {
                break item;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for indexed file; last response: {last:#?}");
            }
            last = Some(response);
            std::thread::sleep(Duration::from_millis(25));
        };

        assert_eq!(item.kind, ItemKind::File);
        assert_eq!(item.label, "src/main.rs");
        assert_eq!(item.insert_text, "src/main.rs");

        let status = files.status(project.path()).unwrap();
        assert!(status.initialized);
        assert!(status.files >= 3, "status: {status:#?}");
        assert!(status.warmup_complete, "status: {status:#?}");
    }

    #[test]
    fn search_interactive_returns_pending_then_result() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());

        let request = SearchRequest {
            query: "main".to_string(),
            cwd: project.path().to_path_buf(),
            limit: 20,
            offset: 0,
            include_dirs: true,
        };
        let first = files.search_interactive(request.clone()).unwrap();
        assert!(first.searching, "first response: {first:#?}");
        assert!(first.items.is_empty(), "first response: {first:#?}");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let response = files.search_interactive(request.clone()).unwrap();
            if !response.searching {
                assert!(
                    response.items.iter().any(|item| item.path == "src/main.rs"),
                    "response: {response:#?}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for interactive search"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn accept_promotes_recent_paths() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("alpha.rs"), "").unwrap();
        std::fs::write(project.path().join("zeta.rs"), "").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        files
            .accept(AcceptRequest {
                cwd: project.path().to_path_buf(),
                path: "zeta.rs".to_string(),
            })
            .unwrap();

        let response = files
            .search(SearchRequest {
                query: String::new(),
                cwd: project.path().to_path_buf(),
                limit: 10,
                offset: 0,
                include_dirs: false,
            })
            .unwrap();
        assert_eq!(response.items[0].path, "zeta.rs");
    }

    #[test]
    fn rescan_finds_new_files() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("old.rs"), "").unwrap();

        let mut files = WorkspaceFiles::new(state.path().to_path_buf());
        wait_until_ready(&mut files, project.path());
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join("src/new_file.rs"), "").unwrap();
        files.rescan(project.path()).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let response = files
                .search(SearchRequest {
                    query: "new_file".to_string(),
                    cwd: project.path().to_path_buf(),
                    limit: 10,
                    offset: 0,
                    include_dirs: false,
                })
                .unwrap();
            if response
                .items
                .iter()
                .any(|item| item.path == "src/new_file.rs")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for rescan: {response:#?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    #[ignore = "manual in-memory workspace file fuzzy-search benchmark; prefer `cargo xtask bench-file-search`"]
    fn workspace_file_search_benchmark_suite() {
        let runs = file_search_bench_runs();
        let entries_count = file_search_bench_entries();
        let include_dirs = file_search_bench_include_dirs();
        let queries = file_search_bench_queries();
        let entries = synthetic_file_entries(entries_count);

        eprintln!(
            "FILE_SEARCH_BENCH runs={runs} entries={} include_dirs={} queries={}",
            entries.len(),
            include_dirs,
            queries.join(",")
        );
        eprintln!(
            "| query          | matched | first path                                           |   total ms |    rank ms |   items ms |"
        );
        eprintln!(
            "|----------------|---------|------------------------------------------------------|------------|------------|------------|"
        );

        for query in queries {
            let _warmup = run_file_search_bench_sample(&entries, &query, include_dirs);
            let mut samples = Vec::with_capacity(runs);
            for run in 0..runs {
                let sample = run_file_search_bench_sample(&entries, &query, include_dirs);
                eprintln!(
                    "FILE_SEARCH_BENCH_SAMPLE query={} run={} entries={} matched={} first_path={} total_ms={:.3} rank_ms={:.3} items_ms={:.3}",
                    query,
                    run + 1,
                    entries.len(),
                    sample.matched,
                    sample.first_path,
                    sample.total_ms,
                    sample.rank_ms,
                    sample.items_ms,
                );
                samples.push(sample);
            }
            print_file_search_bench_summary(&query, entries.len(), include_dirs, &samples);
        }
    }

    #[derive(Clone, Debug)]
    struct FileSearchBenchSample {
        matched: usize,
        first_path: String,
        total_ms: f64,
        rank_ms: f64,
        items_ms: f64,
    }

    #[derive(Clone, Copy, Debug)]
    struct BenchStats {
        mean: f64,
        stddev: f64,
        min: f64,
        max: f64,
    }

    impl BenchStats {
        fn from(values: &[f64]) -> Self {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let variance = if values.len() > 1 {
                values
                    .iter()
                    .map(|value| {
                        let delta = value - mean;
                        delta * delta
                    })
                    .sum::<f64>()
                    / (values.len() - 1) as f64
            } else {
                0.0
            };
            let min = values.iter().copied().fold(f64::INFINITY, f64::min);
            let max = values.iter().copied().fold(0.0, f64::max);
            Self {
                mean,
                stddev: variance.sqrt(),
                min,
                max,
            }
        }

        fn display(self) -> String {
            format!("{:.2}±{:.2}", self.mean, self.stddev)
        }
    }

    fn file_search_bench_runs() -> usize {
        env_usize("SMELT_FILE_SEARCH_BENCH_RUNS", 10)
    }

    fn file_search_bench_entries() -> usize {
        env_usize("SMELT_FILE_SEARCH_BENCH_ENTRIES", 500_000)
    }

    fn file_search_bench_include_dirs() -> bool {
        std::env::var("SMELT_FILE_SEARCH_BENCH_INCLUDE_DIRS")
            .ok()
            .as_deref()
            != Some("0")
    }

    fn file_search_bench_queries() -> Vec<String> {
        std::env::var("SMELT_FILE_SEARCH_BENCH_QUERIES")
            .ok()
            .filter(|queries| !queries.trim().is_empty())
            .map(|queries| {
                queries
                    .split(',')
                    .map(str::trim)
                    .filter(|query| !query.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|queries| !queries.is_empty())
            .unwrap_or_else(|| {
                [
                    "main",
                    "widget",
                    "config",
                    "controller",
                    "bench",
                    "zzz_nomatch",
                ]
                .into_iter()
                .map(str::to_string)
                .collect()
            })
    }

    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default)
    }

    fn synthetic_file_entries(count: usize) -> Vec<FileEntry> {
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let kind = if i % 25 == 0 {
                ItemKind::Dir
            } else {
                ItemKind::File
            };
            entries.push(FileEntry::new(synthetic_path(i, kind), kind));
        }
        entries
    }

    fn synthetic_path(i: usize, kind: ItemKind) -> String {
        if kind == ItemKind::Dir {
            return match i % 6 {
                0 => format!("crates/crate_{:04}/src/dir_{i:06}", i % 1_000),
                1 => format!("packages/app_{:04}/src/components/dir_{i:06}", i % 2_000),
                2 => format!("services/api_{:04}/controllers/dir_{i:06}", i % 1_500),
                3 => format!("docs/guide/section_{:04}/dir_{i:06}", i % 800),
                4 => format!("tests/fixtures/project_{:04}/dir_{i:06}", i % 1_200),
                _ => format!("home/library/cache/project_{:04}/dir_{i:06}", i % 5_000),
            };
        }
        match i % 8 {
            0 => format!("crates/crate_{:04}/src/main_{i:06}.rs", i % 1_000),
            1 => format!("crates/core/src/workspace/module_{i:06}.rs"),
            2 => format!(
                "packages/app_{:04}/src/components/Widget{i:06}.tsx",
                i % 2_000
            ),
            3 => format!(
                "packages/app_{:04}/src/config/environment_{i:06}.ts",
                i % 2_000
            ),
            4 => format!(
                "services/api_{:04}/controllers/UserController{i:06}.go",
                i % 1_500
            ),
            5 => format!("docs/guide/section_{:04}/benchmark_{i:06}.md", i % 800),
            6 => format!(
                "tests/fixtures/project_{:04}/snapshot_{i:06}.json",
                i % 1_200
            ),
            _ => format!("home/library/cache/tool/cache_entry_{i:06}.dat"),
        }
    }

    fn run_file_search_bench_sample(
        entries: &[FileEntry],
        query: &str,
        include_dirs: bool,
    ) -> FileSearchBenchSample {
        let total_start = std::time::Instant::now();
        let rank_start = std::time::Instant::now();
        let ranked = rank_entries_window(query, entries, include_dirs, &HashMap::new(), 0, 200);
        let rank_ms = elapsed_ms(rank_start.elapsed());
        let matched = ranked.total;

        let items_start = std::time::Instant::now();
        let items = ranked
            .entries
            .iter()
            .map(|(index, score)| {
                let entry = &entries[*index];
                search_item(entry.kind, entry.path.clone(), *score)
            })
            .collect::<Vec<_>>();
        std::hint::black_box(&items);
        let items_ms = elapsed_ms(items_start.elapsed());
        let total_ms = elapsed_ms(total_start.elapsed());
        let first_path = items
            .first()
            .map(|item| item.path.clone())
            .unwrap_or_else(|| "-".to_string());

        FileSearchBenchSample {
            matched,
            first_path,
            total_ms,
            rank_ms,
            items_ms,
        }
    }

    fn elapsed_ms(elapsed: std::time::Duration) -> f64 {
        elapsed.as_secs_f64() * 1_000.0
    }

    fn print_file_search_bench_summary(
        query: &str,
        entries: usize,
        include_dirs: bool,
        samples: &[FileSearchBenchSample],
    ) {
        let total = BenchStats::from(
            &samples
                .iter()
                .map(|sample| sample.total_ms)
                .collect::<Vec<_>>(),
        );
        let rank = BenchStats::from(
            &samples
                .iter()
                .map(|sample| sample.rank_ms)
                .collect::<Vec<_>>(),
        );
        let items = BenchStats::from(
            &samples
                .iter()
                .map(|sample| sample.items_ms)
                .collect::<Vec<_>>(),
        );
        let sample = &samples[0];
        eprintln!(
            "| {:<14} | {:>7} | {:<52} | {:>10} | {:>10} | {:>10} |",
            query,
            sample.matched,
            truncate(&sample.first_path, 52),
            total.display(),
            rank.display(),
            items.display(),
        );
        eprintln!(
            "FILE_SEARCH_BENCH_SUMMARY query={} runs={} entries={} include_dirs={} matched={} first_path={} total_mean_ms={:.3} total_stddev_ms={:.3} total_min_ms={:.3} total_max_ms={:.3} rank_mean_ms={:.3} rank_stddev_ms={:.3} items_mean_ms={:.3} items_stddev_ms={:.3}",
            query,
            samples.len(),
            entries,
            include_dirs,
            sample.matched,
            sample.first_path,
            total.mean,
            total.stddev,
            total.min,
            total.max,
            rank.mean,
            rank.stddev,
            items.mean,
            items.stddev,
        );
    }

    fn truncate(s: &str, max_len: usize) -> String {
        if s.chars().count() <= max_len {
            return s.to_string();
        }
        let head = s
            .chars()
            .take(max_len.saturating_sub(1))
            .collect::<String>();
        format!("{head}…")
    }

    fn wait_until_ready(files: &mut WorkspaceFiles, cwd: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = files.status(cwd).unwrap();
            if status.warmup_complete && !status.scanning {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for workspace file scan: {status:#?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
