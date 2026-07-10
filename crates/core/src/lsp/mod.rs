mod daemon;
mod operations;
mod semantic;

use semantic::*;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::{oneshot, watch, Mutex};
use tokio::time::{timeout, Duration};

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LspServerConfig {
    pub cmd: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub language_id: Option<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(default = "default_init_timeout_ms")]
    pub init_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_startup_wait_ms")]
    pub startup_wait_ms: u64,
    #[serde(default)]
    pub initialization_options: Value,
    #[serde(default)]
    pub settings: Value,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct LspConfig {
    #[serde(default)]
    pub servers: HashMap<String, LspServerConfig>,
}

#[derive(Clone)]
struct ServerEntry {
    config: LspServerConfig,
    clients: HashMap<PathBuf, Arc<LspClientSlot>>,
}

struct LspClientSlot {
    name: String,
    root: PathBuf,
    config: LspServerConfig,
    state: Mutex<LspClientState>,
    stderr_tail: StderrTail,
}

enum LspClientState {
    Starting {
        started: Instant,
    },
    Ready {
        client: Arc<LspClient>,
        init_ms: u128,
    },
    Failed {
        started: Instant,
        error: String,
    },
}

#[derive(Default)]
pub struct LspManager {
    servers: StdMutex<HashMap<String, ServerEntry>>,
    controller: StdMutex<LspControllerState>,
}

#[derive(Default)]
struct LspControllerState {
    desired: LspConfig,
    desired_revision: u64,
    observed_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LspControllerStatus {
    pub desired_revision: u64,
    pub observed_revision: u64,
}

pub struct LspConfigure {
    manager: Arc<LspManager>,
    revision: u64,
    desired: LspConfig,
}

struct LspConfigureCompletion {
    manager: Arc<LspManager>,
    revision: u64,
    removed_clients: Vec<Arc<LspClientSlot>>,
}

type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
type DiagnosticsByUri = Arc<DiagnosticsStore>;
type StderrTail = Arc<Mutex<VecDeque<String>>>;

struct DiagnosticsStore {
    state: Mutex<DiagnosticsState>,
    updates: watch::Sender<u64>,
}

#[derive(Default)]
struct DiagnosticsState {
    generation: u64,
    by_uri: HashMap<String, DiagnosticEntry>,
}

struct DiagnosticEntry {
    generation: u64,
    published_at: Instant,
    value: Value,
}

const DIAGNOSTICS_SETTLE: Duration = Duration::from_millis(500);
const WORKSPACE_PROGRESS_SETTLE: Duration = Duration::from_millis(100);

#[derive(Clone)]
struct WorkspaceProgressTracker {
    updates: watch::Sender<WorkspaceProgressState>,
}

#[derive(Clone)]
struct WorkspaceProgressState {
    initialized_at: Instant,
    seen: bool,
    server_quiescent: Option<bool>,
    active: HashMap<String, String>,
    last_update: Instant,
}

struct OpenDocument {
    version: i32,
    text: String,
}

struct SyncedDocument {
    text: String,
    changed: bool,
}

struct LspClient {
    name: String,
    root: PathBuf,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingRequests,
    diagnostics: DiagnosticsByUri,
    workspace_progress: WorkspaceProgressTracker,
    documents: Mutex<HashMap<String, OpenDocument>>,
    request_timeout_ms: u64,
    init_timeout_ms: u64,
    initialization_options: Value,
    config: LspServerConfig,
    next_id: AtomicU64,
    _child: Mutex<Child>,
}

impl DiagnosticsStore {
    fn new() -> Self {
        let (updates, _) = watch::channel(0);
        Self {
            state: Mutex::new(DiagnosticsState::default()),
            updates,
        }
    }

    async fn generation(&self, uri: &str) -> u64 {
        self.state
            .lock()
            .await
            .by_uri
            .get(uri)
            .map_or(0, |entry| entry.generation)
    }

    async fn current(&self, uri: &str) -> Option<Value> {
        self.state
            .lock()
            .await
            .by_uri
            .get(uri)
            .map(|entry| entry.value.clone())
    }

    async fn publish(&self, uri: String, value: Value) {
        let generation = {
            let mut state = self.state.lock().await;
            state.generation += 1;
            let generation = state.generation;
            state.by_uri.insert(
                uri,
                DiagnosticEntry {
                    generation,
                    published_at: Instant::now(),
                    value,
                },
            );
            generation
        };
        self.updates.send_replace(generation);
    }

    async fn wait_for_update(&self, uri: &str, after: u64, wait: Duration) -> Value {
        let mut updates = self.updates.subscribe();
        let deadline = Instant::now() + wait;
        loop {
            let settle_deadline = self
                .state
                .lock()
                .await
                .by_uri
                .get(uri)
                .filter(|entry| entry.generation > after)
                .map(|entry| entry.published_at + DIAGNOSTICS_SETTLE);
            let now = Instant::now();
            if now >= deadline || settle_deadline.is_some_and(|settle| now >= settle) {
                return self.current(uri).await.unwrap_or_else(|| json!([]));
            }
            let next_deadline = settle_deadline.map_or(deadline, |settle| settle.min(deadline));
            let _ = timeout(next_deadline - now, updates.changed()).await;
        }
    }

    async fn snapshot(&self) -> serde_json::Map<String, Value> {
        self.state
            .lock()
            .await
            .by_uri
            .iter()
            .map(|(uri, entry)| (uri.clone(), entry.value.clone()))
            .collect()
    }
}

impl WorkspaceProgressTracker {
    fn new() -> Self {
        let now = Instant::now();
        let (updates, _) = watch::channel(WorkspaceProgressState {
            initialized_at: now,
            seen: false,
            server_quiescent: None,
            active: HashMap::new(),
            last_update: now,
        });
        Self { updates }
    }

    fn mark_initialized(&self) {
        self.updates.send_modify(|state| {
            let now = Instant::now();
            state.initialized_at = now;
            state.last_update = now;
        });
    }

    fn record(&self, message: &Value) {
        let Some(params) = message.get("params") else {
            return;
        };
        if message.get("method").and_then(Value::as_str) == Some("experimental/serverStatus") {
            let Some(quiescent) = params.get("quiescent").and_then(Value::as_bool) else {
                return;
            };
            self.updates.send_modify(|state| {
                state.server_quiescent = Some(quiescent);
                state.last_update = Instant::now();
            });
            return;
        }
        let Some(token) = params.get("token").and_then(progress_token) else {
            return;
        };
        let Some(value) = params.get("value") else {
            return;
        };
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") => {
                let Some(title) = value.get("title").and_then(Value::as_str) else {
                    return;
                };
                if !progress_blocks_workspace(title) {
                    return;
                }
                self.updates.send_modify(|state| {
                    state.seen = true;
                    state.active.insert(token, title.to_string());
                    state.last_update = Instant::now();
                });
            }
            Some("end") => {
                self.updates.send_if_modified(|state| {
                    if state.active.remove(&token).is_none() {
                        return false;
                    }
                    state.last_update = Instant::now();
                    true
                });
            }
            _ => {}
        }
    }

    async fn wait_until_ready(
        &self,
        startup_timeout: Duration,
        progress_start_grace: Duration,
    ) -> Result<(), String> {
        let mut updates = self.updates.subscribe();
        loop {
            let state = updates.borrow().clone();
            let now = Instant::now();
            if state.server_quiescent == Some(true) {
                return Ok(());
            }

            let loading = state.server_quiescent == Some(false) || !state.active.is_empty();
            if loading {
                let startup_deadline = state.initialized_at + startup_timeout;
                if now >= startup_deadline {
                    let mut active = state.active.values().cloned().collect::<Vec<_>>();
                    if state.server_quiescent == Some(false) && active.is_empty() {
                        active.push("server workspace status".into());
                    }
                    active.sort();
                    active.dedup();
                    return Err(format!(
                        "LSP workspace is still loading after {}ms: {}",
                        startup_timeout.as_millis(),
                        active.join(", ")
                    ));
                }
                let _ = timeout(startup_deadline - now, updates.changed()).await;
                continue;
            }

            if !state.seen {
                let grace_deadline = state.initialized_at + progress_start_grace;
                if now >= grace_deadline {
                    return Ok(());
                }
                if timeout(grace_deadline - now, updates.changed())
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                continue;
            }

            let settle_deadline = state.last_update + WORKSPACE_PROGRESS_SETTLE;
            if now >= settle_deadline {
                return Ok(());
            }
            let _ = timeout(settle_deadline - now, updates.changed()).await;
        }
    }

    fn status(&self, progress_start_grace: Duration) -> Vec<String> {
        let state = self.updates.borrow();
        if state.server_quiescent == Some(true) {
            return vec!["  workspace: ready".into()];
        }
        if state.server_quiescent == Some(false) || !state.active.is_empty() {
            let mut lines = vec!["  workspace: loading".into()];
            let mut active = state.active.values().cloned().collect::<Vec<_>>();
            active.sort();
            active.dedup();
            if !active.is_empty() {
                lines.push(format!("  progress: {}", active.join(", ")));
            }
            return lines;
        }
        if !state.seen {
            if state.initialized_at.elapsed() < progress_start_grace {
                return vec!["  workspace: detecting load progress".into()];
            }
            return Vec::new();
        }
        vec!["  workspace: ready".into()]
    }
}

fn progress_token(value: &Value) -> Option<String> {
    match value {
        Value::String(token) => Some(token.clone()),
        Value::Number(token) => Some(token.to_string()),
        _ => None,
    }
}

fn progress_blocks_workspace(title: &str) -> bool {
    let title = title.to_ascii_lowercase();
    ["index", "load", "project", "scan", "workspace", "initializ"]
        .iter()
        .any(|word| title.contains(word))
}

fn default_init_timeout_ms() -> u64 {
    120_000
}

fn default_request_timeout_ms() -> u64 {
    30_000
}

fn default_startup_wait_ms() -> u64 {
    5_000
}

pub async fn run_daemon(socket: PathBuf) -> Result<(), String> {
    daemon::run(socket).await
}

impl LspManager {
    fn reconcile_config(&self, config: LspConfig) -> Vec<Arc<LspClientSlot>> {
        let mut removed_clients = Vec::new();
        let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        servers.retain(|name, entry| {
            let keep = config
                .servers
                .get(name)
                .is_some_and(|new_config| new_config == &entry.config);
            if !keep {
                removed_clients.extend(entry.clients.values().cloned());
            }
            keep
        });
        for (name, config) in config.servers {
            servers.entry(name).or_insert_with(|| ServerEntry {
                config,
                clients: HashMap::new(),
            });
        }
        removed_clients
    }

    fn reserve_configure(&self, desired: &LspConfig) -> Option<u64> {
        let mut controller = self
            .controller
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if &controller.desired == desired {
            return None;
        }
        controller.desired_revision = controller.desired_revision.wrapping_add(1);
        controller.desired = desired.clone();
        Some(controller.desired_revision)
    }

    pub fn prepare_configure(self: &Arc<Self>, desired: LspConfig) -> Option<LspConfigure> {
        self.reserve_configure(&desired)
            .map(|revision| LspConfigure {
                manager: Arc::clone(self),
                revision,
                desired,
            })
    }

    pub async fn configure(&self, desired: LspConfig) {
        let Some(revision) = self.reserve_configure(&desired) else {
            return;
        };
        self.apply_configure(revision, desired).await;
    }

    pub fn configure_detached(self: &Arc<Self>, desired: LspConfig) {
        let Some(completion) = self
            .prepare_configure(desired)
            .and_then(LspConfigure::install)
        else {
            return;
        };
        if completion.removed_clients.is_empty() {
            completion.observe();
        } else {
            tokio::spawn(completion.finish());
        }
    }

    fn install_configure(
        &self,
        revision: u64,
        desired: LspConfig,
    ) -> Option<Vec<Arc<LspClientSlot>>> {
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if controller.desired_revision != revision {
            return None;
        }
        let removed_clients = self.reconcile_config(desired);
        drop(controller);
        Some(removed_clients)
    }

    async fn finish_configure(&self, revision: u64, removed_clients: Vec<Arc<LspClientSlot>>) {
        for slot in removed_clients {
            slot.shutdown().await;
        }
        let mut controller = self
            .controller
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if controller.desired_revision == revision {
            controller.observed_revision = revision;
        }
    }

    async fn apply_configure(&self, revision: u64, desired: LspConfig) {
        if let Some(removed_clients) = self.install_configure(revision, desired) {
            self.finish_configure(revision, removed_clients).await;
        }
    }

    pub fn controller_status(&self) -> LspControllerStatus {
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        LspControllerStatus {
            desired_revision: controller.desired_revision,
            observed_revision: controller.observed_revision,
        }
    }

    pub fn config_snapshot(&self) -> LspConfig {
        let servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        LspConfig {
            servers: servers
                .iter()
                .map(|(name, entry)| (name.clone(), entry.config.clone()))
                .collect(),
        }
    }

    pub async fn call(&self, operation: &str, args: Value) -> Result<Value, String> {
        let config = self.config_snapshot();
        if config.servers.is_empty() {
            return self.dispatch_local(operation, args).await;
        }
        daemon::call(config, operation, args).await
    }

    pub async fn shutdown_all(&self) {
        let slots = {
            let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
            let slots = servers
                .values_mut()
                .flat_map(|entry| entry.clients.drain().map(|(_, slot)| slot))
                .collect::<Vec<_>>();
            servers.clear();
            slots
        };
        for slot in slots {
            slot.shutdown().await;
        }
    }

    pub async fn status(&self, file_path: Option<&str>) -> String {
        let file_error = if let Some(file_path) = file_path {
            self.initialized_client_for_file(file_path).await.err()
        } else {
            None
        };
        let entries: Vec<_> = {
            let servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
            let mut entries: Vec<_> = servers
                .iter()
                .map(|(name, entry)| {
                    let mut clients: Vec<_> = entry.clients.values().cloned().collect();
                    clients.sort_by(|a, b| a.root.cmp(&b.root));
                    (name.clone(), entry.config.clone(), clients)
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        };

        let mut lines = vec!["LSP backend ready.".to_string()];
        if entries.is_empty() {
            lines.push("No language servers configured.".into());
            return lines.join("\n");
        }
        lines.push(format!(
            "Configured servers: {}",
            entries
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if let Some(error) = file_error {
            lines.push(format!("Requested file error: {error}"));
        }
        for (name, config, clients) in entries {
            lines.push(format!("{name}: {}", config.cmd.join(" ")));
            if clients.is_empty() {
                lines.push("  state: configured".into());
                continue;
            }
            for slot in clients {
                lines.push(slot.status().await);
            }
        }
        lines.join("\n")
    }

    pub async fn document_symbols(&self, file_path: &str) -> Result<Value, String> {
        let client = self.client_for_file(file_path).await?;
        client.sync_document(file_path).await?;
        request_document_symbols(&client, file_path).await
    }

    pub(crate) async fn outline(&self, options: OutlineOptions<'_>) -> Result<Value, String> {
        let raw_symbols = self.document_symbols(options.file_path).await?;
        if !raw_symbols.is_array() {
            return Ok(raw_symbols);
        }
        let symbols = normalize_document_symbols(&raw_symbols);
        let filter = OutlineFilter {
            symbol: options.symbol,
            kind: options.kind,
            name_contains: options.name_contains,
            max_depth: options.max_depth,
        };
        let limit = bounded_limit(options.max_symbols, 200, 500);
        let (total, compact_symbols) = limited_outline_symbols(&symbols, filter, limit);
        let shown = count_compact_outline_symbols(&compact_symbols);
        Ok(json!({
            "file_path": display_path(&absolute_path_string(options.file_path)),
            "filters": {
                "symbol": options.symbol,
                "kind": options.kind,
                "name_contains": options.name_contains,
                "max_depth": options.max_depth,
            },
            "total": total,
            "limit": limit,
            "truncated": total > shown,
            "shown": shown,
            "omitted": total.saturating_sub(shown),
            "symbols": compact_symbols,
        }))
    }

    pub async fn workspace_symbols(
        &self,
        query: &str,
        kind: Option<&str>,
        path_glob: Option<&str>,
        limit: usize,
        exact: bool,
    ) -> Result<Value, String> {
        let clients = self.workspace_clients(path_glob).await?;
        let mut errors = Vec::new();
        let mut symbols = Vec::new();
        let mut handles = Vec::new();
        for (server, client) in clients {
            let query = query.to_string();
            handles.push(tokio::spawn(async move {
                let result = client.request_workspace_symbols(&query).await;
                (server, result)
            }));
        }
        for handle in handles {
            let Ok((server, result)) = handle.await else {
                errors.push(json!({
                    "code": "lsp_workspace_symbol_task_failed",
                    "message": "workspace symbol request task failed",
                }));
                continue;
            };
            match result {
                Ok(value) => {
                    collect_workspace_symbols(&value, &server, kind, path_glob, &mut symbols)
                }
                Err(err) => errors.push(json!({
                    "code": "lsp_workspace_symbol_request_failed",
                    "message": err,
                    "server": server,
                })),
            }
        }
        rank_workspace_symbols(&mut symbols, query, exact);
        let total = symbols.len();
        let limit = bounded_limit(limit, 20, 100);
        let truncated = total > limit;
        symbols.truncate(limit);
        Ok(json!({
            "query": query,
            "kind": kind,
            "path_glob": path_glob,
            "exact": exact,
            "limit": limit,
            "truncated": truncated,
            "total": total,
            "shown": symbols.len(),
            "omitted": total.saturating_sub(symbols.len()),
            "symbols": symbols,
            "errors": errors,
        }))
    }

    pub async fn inspect_symbol(
        &self,
        file_path: &str,
        line: u64,
        column: u64,
        depth: u64,
    ) -> Result<Value, String> {
        let client = self.client_for_file(file_path).await?;
        let text = client.sync_document(file_path).await?.text;
        let params = text_position_params(file_path, &text, line, column)?;
        let hover_params = params.clone();
        let definition_params = params.clone();
        let type_definition_params = params.clone();
        let implementation_params = params;
        let symbols_client = client.clone();
        let references_client = client.clone();
        let references_text = text.clone();
        let (hover, definitions, type_definitions, implementations, raw_symbols, references) = tokio::join!(
            async {
                normalize_hover(
                    optional_lsp_request(&client, "textDocument/hover", hover_params).await,
                )
            },
            async {
                optional_lsp_locations(&client, "textDocument/definition", definition_params).await
            },
            async {
                optional_lsp_locations(
                    &client,
                    "textDocument/typeDefinition",
                    type_definition_params,
                )
                .await
            },
            async {
                optional_lsp_locations(
                    &client,
                    "textDocument/implementation",
                    implementation_params,
                )
                .await
            },
            async {
                request_document_symbols(&symbols_client, file_path)
                    .await
                    .ok()
            },
            async {
                if depth == 0 {
                    return Value::Null;
                }
                let mut reference_params =
                    match text_position_params(file_path, &references_text, line, column) {
                        Ok(params) => params,
                        Err(err) => return json!({ "error": err }),
                    };
                reference_params["context"] = json!({ "includeDeclaration": false });
                match references_client
                    .request("textDocument/references", reference_params)
                    .await
                {
                    Ok(raw_refs) => location_summary(
                        &raw_refs,
                        LocationSummaryOptions {
                            limit: 30,
                            symbol: Some(SymbolPosition {
                                file_path,
                                line,
                                column,
                            }),
                        },
                    ),
                    Err(err) => json!({ "error": err }),
                }
            }
        );
        let symbols = raw_symbols
            .as_ref()
            .map(normalize_document_symbols)
            .unwrap_or_default();
        let enclosing_symbol = enclosing_symbol_at(&symbols, line, column);
        let outline_context = outline_path_at_position(&symbols, line, column);

        Ok(json!({
            "position": {
                "file_path": display_path(&absolute_path_string(file_path)),
                "line": line,
                "column": column,
            },
            "enclosing_symbol": enclosing_symbol,
            "hover": hover,
            "definitions": definitions,
            "type_definitions": type_definitions,
            "implementations": implementations,
            "references": references,
            "outline_context": outline_context,
        }))
    }

    pub async fn definition(
        &self,
        file_path: &str,
        line: u64,
        column: u64,
    ) -> Result<Value, String> {
        let client = self.client_for_file(file_path).await?;
        let text = client.sync_document(file_path).await?.text;
        let raw = client
            .request(
                "textDocument/definition",
                text_position_params(file_path, &text, line, column)?,
            )
            .await?;
        Ok(location_summary(
            &raw,
            LocationSummaryOptions {
                limit: 50,
                symbol: Some(SymbolPosition {
                    file_path,
                    line,
                    column,
                }),
            },
        ))
    }

    pub(crate) async fn references(
        &self,
        file_path: &str,
        line: u64,
        column: u64,
        options: ReferenceOptions,
    ) -> Result<Value, String> {
        let client = self.client_for_file(file_path).await?;
        let text = client.sync_document(file_path).await?.text;
        let mut params = text_position_params(file_path, &text, line, column)?;
        params["context"] = json!({ "includeDeclaration": options.include_declaration });
        let raw_refs = client.request("textDocument/references", params).await?;
        if options.raw {
            return Ok(raw_location_summary(raw_refs, options.limit));
        }
        Ok(location_summary(
            &raw_refs,
            LocationSummaryOptions {
                limit: options.limit,
                symbol: Some(SymbolPosition {
                    file_path,
                    line,
                    column,
                }),
            },
        ))
    }

    pub async fn diagnostics(&self, file_path: Option<&str>) -> Result<Value, String> {
        if let Some(path) = file_path {
            let client = self.client_for_file(path).await?;
            let uri = file_uri(path)?;
            let generation = client.diagnostics.generation(&uri).await;
            let synced = client.sync_document(path).await?;
            if !synced.changed {
                return Ok(client
                    .diagnostics
                    .current(&uri)
                    .await
                    .unwrap_or_else(|| json!([])));
            }
            return Ok(client
                .diagnostics
                .wait_for_update(
                    &uri,
                    generation,
                    Duration::from_millis(client.config.startup_wait_ms),
                )
                .await);
        }

        let slots: Vec<_> = {
            let servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
            servers
                .values()
                .flat_map(|entry| entry.clients.values().cloned())
                .collect()
        };
        let mut out = serde_json::Map::new();
        for slot in slots {
            let client = match slot.ready_now().await {
                Some(client) => client,
                None => continue,
            };
            out.extend(client.diagnostics.snapshot().await);
        }
        Ok(Value::Object(out))
    }

    pub async fn rename(
        &self,
        file_path: &str,
        line: u64,
        column: u64,
        new_name: &str,
        apply: bool,
    ) -> Result<Value, String> {
        let client = self.client_for_file(file_path).await?;
        let text = client.sync_document(file_path).await?.text;
        let mut params = text_position_params(file_path, &text, line, column)?;
        params["newName"] = json!(new_name);
        let edit = client.request("textDocument/rename", params).await?;
        let summary = workspace_edit_summary(&edit);
        if apply {
            apply_workspace_edit(&edit)?;
        }
        Ok(json!({ "applied": apply, "summary": summary, "edit": edit }))
    }

    async fn workspace_clients(
        &self,
        path_glob: Option<&str>,
    ) -> Result<Vec<(String, Arc<LspClient>)>, String> {
        let (existing_slots, candidate_names, hinted): (Vec<_>, Vec<String>, bool) = {
            let servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
            if servers.is_empty() {
                return Err("no LSP servers configured".into());
            }
            let mut existing = servers
                .iter()
                .flat_map(|(name, entry)| {
                    entry
                        .clients
                        .values()
                        .cloned()
                        .map(|slot| (name.clone(), slot))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            existing.sort_by(|a, b| a.0.cmp(&b.0));
            let hinted = path_glob.and_then(extension_from_pattern).is_some();
            let candidate_names = workspace_candidate_servers(&servers, path_glob);
            (existing, candidate_names, hinted)
        };

        let mut ready_clients = Vec::new();
        for (name, slot) in &existing_slots {
            if !candidate_names.contains(name) {
                continue;
            }
            if let Some(client) = slot.ready_now().await {
                ready_clients.push((name.clone(), client));
            }
        }
        if !ready_clients.is_empty() {
            return Ok(ready_clients);
        }
        if !hinted && candidate_names.len() > 1 {
            return Err(json!({
                "error": "find_symbol needs a path_glob when multiple LSP servers are configured and none are ready",
                "servers": candidate_names,
            })
            .to_string());
        }

        let slots: Vec<_> = {
            let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let mut slots = Vec::new();
            for name in candidate_names {
                let Some(entry) = servers.get_mut(&name) else {
                    continue;
                };
                let root = find_root(&cwd, &entry.config.root_markers);
                let slot = entry
                    .clients
                    .entry(root.clone())
                    .or_insert_with(|| {
                        LspClientSlot::start(name.clone(), entry.config.clone(), root)
                    })
                    .clone();
                slots.push((name, slot));
            }
            slots.sort_by(|a, b| a.0.cmp(&b.0));
            slots
        };

        let mut clients = Vec::new();
        let mut errors = Vec::new();
        for (name, slot) in slots {
            match slot.wait_ready().await {
                Ok(client) => clients.push((name, client)),
                Err(err) => errors.push(json!({ "server": name, "status": err })),
            }
        }
        if clients.is_empty() {
            return Err(
                json!({ "error": "no LSP server became ready", "servers": errors }).to_string(),
            );
        }
        Ok(clients)
    }

    async fn client_for_file(&self, file_path: &str) -> Result<Arc<LspClient>, String> {
        let client = self.initialized_client_for_file(file_path).await?;
        client.wait_for_workspace_ready().await?;
        Ok(client)
    }

    async fn initialized_client_for_file(&self, file_path: &str) -> Result<Arc<LspClient>, String> {
        let file = PathBuf::from(file_path);
        let (server_name, slot) = {
            let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
            let server_name = pick_server(&servers, &file)
                .ok_or_else(|| format!("no LSP server configured for {}", file.display()))?;
            let entry = servers
                .get_mut(&server_name)
                .ok_or_else(|| format!("server disappeared: {server_name}"))?;
            let root = find_root(&file, &entry.config.root_markers);
            let slot = entry
                .clients
                .entry(root.clone())
                .or_insert_with(|| {
                    LspClientSlot::start(server_name.clone(), entry.config.clone(), root)
                })
                .clone();
            (server_name, slot)
        };
        slot.wait_ready().await.map_err(|mut value| {
            value["server"] = json!(server_name);
            value.to_string()
        })
    }
}

impl LspConfigure {
    fn install(self) -> Option<LspConfigureCompletion> {
        let removed_clients = self
            .manager
            .install_configure(self.revision, self.desired)?;
        Some(LspConfigureCompletion {
            manager: self.manager,
            revision: self.revision,
            removed_clients,
        })
    }

    pub async fn apply(self) {
        if let Some(completion) = self.install() {
            completion.finish().await;
        }
    }
}

impl LspConfigureCompletion {
    fn observe(self) {
        let mut controller = self
            .manager
            .controller
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if controller.desired_revision == self.revision {
            controller.observed_revision = self.revision;
        }
    }

    async fn finish(self) {
        self.manager
            .finish_configure(self.revision, self.removed_clients)
            .await;
    }
}

pub(crate) struct OutlineOptions<'a> {
    pub(crate) file_path: &'a str,
    pub(crate) max_symbols: usize,
    pub(crate) symbol: Option<&'a str>,
    pub(crate) kind: Option<&'a str>,
    pub(crate) name_contains: Option<&'a str>,
    pub(crate) max_depth: Option<usize>,
}

fn limited_outline_symbols(
    symbols: &[NormalizedSymbol],
    filter: OutlineFilter<'_>,
    limit: usize,
) -> (usize, Vec<CompactOutlineSymbol>) {
    let mut remaining = usize::MAX;
    let all_symbols = compact_outline_symbols_filtered(symbols, &mut remaining, filter);
    let total = count_compact_outline_symbols(&all_symbols);
    if total <= limit {
        return (total, all_symbols);
    }
    let mut remaining = limit;
    let symbols = compact_outline_symbols_filtered(symbols, &mut remaining, filter);
    (total, symbols)
}

pub(crate) struct ReferenceOptions {
    pub(crate) include_declaration: bool,
    pub(crate) limit: usize,
    pub(crate) raw: bool,
}

struct SymbolPosition<'a> {
    file_path: &'a str,
    line: u64,
    column: u64,
}

struct LocationSummaryOptions<'a> {
    limit: usize,
    symbol: Option<SymbolPosition<'a>>,
}

fn raw_location_summary(locations: Value, requested_limit: usize) -> Value {
    let mut locations = match locations {
        Value::Array(locations) => locations,
        Value::Null => Vec::new(),
        location => vec![location],
    };
    let total = locations.len();
    let limit = bounded_limit(requested_limit, 50, 200);
    locations.truncate(limit);
    json!({
        "raw": true,
        "total": total,
        "limit": limit,
        "truncated": total > locations.len(),
        "shown": locations.len(),
        "omitted": total.saturating_sub(locations.len()),
        "locations": locations,
    })
}

fn location_summary(locations: &Value, options: LocationSummaryOptions<'_>) -> Value {
    let mut locations = normalize_locations(locations);
    let total = locations.len();
    let limit = bounded_limit(options.limit, 50, 200);
    locations.truncate(limit);
    add_location_previews(&mut locations);
    let locations = locations
        .into_iter()
        .map(|loc| loc.to_json())
        .collect::<Vec<_>>();
    let mut out = json!({
        "total": total,
        "limit": limit,
        "truncated": total > locations.len(),
        "shown": locations.len(),
        "omitted": total.saturating_sub(locations.len()),
        "locations": locations,
    });
    if let Some(symbol) = options.symbol {
        out["symbol"] = json!({
            "file_path": display_path(&absolute_path_string(symbol.file_path)),
            "line": symbol.line,
            "column": symbol.column,
        });
    }
    out
}

impl LspClientSlot {
    fn start(name: String, config: LspServerConfig, root: PathBuf) -> Arc<Self> {
        let slot = Arc::new(Self {
            name,
            root,
            config,
            state: Mutex::new(LspClientState::Starting {
                started: Instant::now(),
            }),
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
        });
        let slot_for_task = slot.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let result = LspClient::start(
                slot_for_task.name.clone(),
                slot_for_task.config.clone(),
                slot_for_task.root.clone(),
                slot_for_task.stderr_tail.clone(),
            )
            .await;
            let mut state = slot_for_task.state.lock().await;
            *state = match result {
                Ok(client) => LspClientState::Ready {
                    client,
                    init_ms: started.elapsed().as_millis(),
                },
                Err(error) => LspClientState::Failed { started, error },
            };
        });
        slot
    }

    async fn ready_now(&self) -> Option<Arc<LspClient>> {
        match &*self.state.lock().await {
            LspClientState::Ready { client, .. } => Some(client.clone()),
            _ => None,
        }
    }

    async fn wait_ready(&self) -> Result<Arc<LspClient>, Value> {
        let wait = Duration::from_millis(self.config.startup_wait_ms);
        let deadline = Instant::now() + wait;
        loop {
            enum Snapshot {
                Ready(Arc<LspClient>),
                Failed { elapsed_ms: u128, error: String },
                Starting { elapsed_ms: u128 },
                Waiting,
            }
            let snapshot = {
                let state = self.state.lock().await;
                match &*state {
                    LspClientState::Ready { client, .. } => Snapshot::Ready(client.clone()),
                    LspClientState::Failed { started, error } => Snapshot::Failed {
                        elapsed_ms: started.elapsed().as_millis(),
                        error: error.clone(),
                    },
                    LspClientState::Starting { started } if Instant::now() >= deadline => {
                        Snapshot::Starting {
                            elapsed_ms: started.elapsed().as_millis(),
                        }
                    }
                    LspClientState::Starting { .. } => Snapshot::Waiting,
                }
            };
            match snapshot {
                Snapshot::Ready(client) => return Ok(client),
                Snapshot::Failed { elapsed_ms, error } => {
                    return Err(json!({
                        "state": "failed",
                        "root": self.root.display().to_string(),
                        "elapsed_ms": elapsed_ms,
                        "error": error,
                        "stderr_tail": self.stderr_tail().await,
                    }));
                }
                Snapshot::Starting { elapsed_ms } => {
                    return Err(json!({
                        "state": "starting",
                        "root": self.root.display().to_string(),
                        "elapsed_ms": elapsed_ms,
                        "message": "language server is still initializing; retry shortly or use grep/read_file meanwhile",
                        "stderr_tail": self.stderr_tail().await,
                    }));
                }
                Snapshot::Waiting => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    async fn status(&self) -> String {
        let mut lines = vec![format!("  root: {}", self.root.display())];
        {
            let state = self.state.lock().await;
            match &*state {
                LspClientState::Starting { started } => {
                    lines.push("  state: starting".into());
                    lines.push(format!("  elapsed: {}ms", started.elapsed().as_millis()));
                }
                LspClientState::Ready { client, init_ms } => {
                    lines.push("  state: ready".into());
                    lines.push(format!("  initialized_in: {init_ms}ms"));
                    lines.extend(
                        client
                            .workspace_progress
                            .status(Duration::from_millis(client.config.startup_wait_ms)),
                    );
                }
                LspClientState::Failed { started, error } => {
                    lines.push("  state: failed".into());
                    lines.push(format!("  elapsed: {}ms", started.elapsed().as_millis()));
                    lines.push(format!("  error: {error}"));
                }
            }
        }
        let stderr = self.stderr_tail().await;
        if !stderr.is_empty() {
            lines.push("  stderr:".into());
            for line in stderr {
                lines.push(format!("    {line}"));
            }
        }
        lines.join("\n")
    }

    async fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().await.iter().cloned().collect()
    }

    async fn shutdown(&self) {
        let client = match &*self.state.lock().await {
            LspClientState::Ready { client, .. } => Some(client.clone()),
            _ => None,
        };
        if let Some(client) = client {
            client.shutdown().await;
        }
    }
}

impl LspClient {
    async fn start(
        name: String,
        config: LspServerConfig,
        root: PathBuf,
        stderr_tail: StderrTail,
    ) -> Result<Arc<Self>, String> {
        let Some(program) = config.cmd.first() else {
            return Err(format!("LSP server `{name}` has empty cmd"));
        };
        let mut cmd = Command::new(program);
        cmd.args(config.cmd.iter().skip(1));
        cmd.current_dir(&root);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        crate::process::without_controlling_terminal(cmd.as_std_mut());
        cmd.kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to start {name}: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or("language server stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("language server stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("language server stderr unavailable")?;
        let workspace_progress = WorkspaceProgressTracker::new();
        let client = Arc::new(Self {
            name,
            root,
            stdin: Arc::new(Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            diagnostics: Arc::new(DiagnosticsStore::new()),
            workspace_progress: workspace_progress.clone(),
            documents: Mutex::new(HashMap::new()),
            request_timeout_ms: config.request_timeout_ms,
            init_timeout_ms: config.init_timeout_ms,
            initialization_options: config.initialization_options.clone(),
            config: config.clone(),
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        });
        spawn_stderr_reader(stderr, stderr_tail);
        spawn_reader(
            stdout,
            client.stdin.clone(),
            client.pending.clone(),
            client.diagnostics.clone(),
            workspace_progress,
            config.settings.clone(),
        );
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<(), String> {
        let root_uri = path_to_uri(&self.root)?;
        let result = self
            .request_with_timeout(
                "initialize",
                json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "capabilities": {
                        "textDocument": {
                            "synchronization": { "didOpen": true, "didChange": true },
                            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                            "definition": {},
                            "typeDefinition": {},
                            "implementation": {},
                            "hover": { "contentFormat": ["markdown", "plaintext"] },
                            "references": {},
                            "rename": { "prepareSupport": false },
                            "publishDiagnostics": {}
                        },
                        "workspace": {
                            "symbol": {},
                            "workspaceEdit": { "documentChanges": true }
                        },
                        "window": { "workDoneProgress": true },
                        "experimental": { "serverStatusNotification": true }
                    },
                    "initializationOptions": self.initialization_options.clone()
                }),
                Duration::from_millis(self.init_timeout_ms),
            )
            .await?;
        self.workspace_progress.mark_initialized();
        self.notify("initialized", json!({})).await?;
        Ok(result).map(|_| ())
    }

    async fn sync_document(&self, file_path: &str) -> Result<SyncedDocument, String> {
        let uri = file_uri(file_path)?;
        let text = tokio::fs::read_to_string(file_path)
            .await
            .map_err(|e| format!("read {}: {e}", file_path))?;
        let (version, previous_text) = {
            let mut documents = self.documents.lock().await;
            match documents.get_mut(&uri) {
                Some(document) if document.text == text => {
                    return Ok(SyncedDocument {
                        text,
                        changed: false,
                    });
                }
                Some(document) => {
                    document.version += 1;
                    let previous_text = std::mem::replace(&mut document.text, text.clone());
                    (document.version, Some(previous_text))
                }
                None => {
                    documents.insert(
                        uri.clone(),
                        OpenDocument {
                            version: 1,
                            text: text.clone(),
                        },
                    );
                    (1, None)
                }
            }
        };

        let result = if version == 1 {
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id(file_path, &self.config),
                        "version": version,
                        "text": text
                    }
                }),
            )
            .await
        } else {
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }]
                }),
            )
            .await
        };

        if let Err(err) = result {
            let mut documents = self.documents.lock().await;
            if version == 1 {
                documents.remove(&uri);
            } else if let (Some(document), Some(previous_text)) =
                (documents.get_mut(&uri), previous_text)
            {
                if document.version == version {
                    document.version -= 1;
                    document.text = previous_text;
                }
            }
            return Err(err);
        }
        Ok(SyncedDocument {
            text,
            changed: true,
        })
    }

    async fn wait_for_workspace_ready(&self) -> Result<(), String> {
        self.workspace_progress
            .wait_until_ready(
                Duration::from_millis(self.init_timeout_ms),
                Duration::from_millis(self.config.startup_wait_ms),
            )
            .await
    }

    async fn request_workspace_symbols(&self, query: &str) -> Result<Value, String> {
        self.wait_for_workspace_ready().await?;
        self.request("workspace/symbol", json!({ "query": query }))
            .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_with_timeout(
            method,
            params,
            Duration::from_millis(self.request_timeout_ms),
        )
        .await
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        request_timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if let Err(err) = self
            .write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }
        match timeout(request_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("LSP server `{}` closed request channel", self.name)),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("LSP request timed out: {method}"))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.write(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn shutdown(&self) {
        let _ = self
            .request_with_timeout("shutdown", Value::Null, Duration::from_secs(2))
            .await;
        let _ = self.notify("exit", Value::Null).await;
        let mut child = self._child.lock().await;
        match timeout(Duration::from_secs(2), child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                crate::process::kill_child_process_group_sigkill(&child);
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
    }

    async fn write(&self, msg: Value) -> Result<(), String> {
        write_message(&self.stdin, msg).await
    }
}

async fn write_message(stdin: &Arc<Mutex<ChildStdin>>, msg: Value) -> Result<(), String> {
    let body = serde_json::to_vec(&msg).map_err(|e| e.to_string())?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(header.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin.write_all(&body).await.map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())
}

fn spawn_stderr_reader(stderr: ChildStderr, stderr_tail: StderrTail) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        loop {
            let mut line = String::new();
            let Ok(n) = reader.read_line(&mut line).await else {
                return;
            };
            if n == 0 {
                return;
            }
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if line.is_empty() {
                continue;
            }
            let mut tail = stderr_tail.lock().await;
            tail.push_back(line);
            while tail.len() > 40 {
                tail.pop_front();
            }
        }
    });
}

fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingRequests,
    diagnostics: DiagnosticsByUri,
    workspace_progress: WorkspaceProgressTracker,
    settings: Value,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut content_len = None;
            loop {
                let mut line = String::new();
                let Ok(n) = reader.read_line(&mut line).await else {
                    fail_pending(&pending, "LSP server stdout closed".into()).await;
                    return;
                };
                if n == 0 {
                    fail_pending(&pending, "LSP server stdout closed".into()).await;
                    return;
                }
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    break;
                }
                if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                    content_len = value.trim().parse::<usize>().ok();
                }
            }
            let Some(len) = content_len else { continue };
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).await.is_err() {
                fail_pending(&pending, "LSP server stdout closed".into()).await;
                return;
            }
            let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                continue;
            };
            if let Some(id) = msg.get("id").cloned() {
                if msg.get("method").is_some() {
                    let response = json!({ "jsonrpc": "2.0", "id": id, "result": server_request_result(&msg, &settings) });
                    let _ = write_message(&stdin, response).await;
                    continue;
                }
                let Some(id) = id.as_u64() else { continue };
                let tx = pending.lock().await.remove(&id);
                if let Some(tx) = tx {
                    let result = if let Some(error) = msg.get("error") {
                        Err(error.to_string())
                    } else {
                        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(result);
                }
            } else if matches!(
                msg.get("method").and_then(Value::as_str),
                Some("$/progress" | "experimental/serverStatus")
            ) {
                workspace_progress.record(&msg);
            } else if msg.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                if let Some(params) = msg.get("params") {
                    if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                        let value = params
                            .get("diagnostics")
                            .cloned()
                            .unwrap_or_else(|| json!([]));
                        diagnostics.publish(uri.to_string(), value).await;
                    }
                }
            }
        }
    });
}

async fn fail_pending(pending: &PendingRequests, error: String) {
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(error.clone()));
    }
}

fn server_request_result(msg: &Value, settings: &Value) -> Value {
    match msg.get("method").and_then(Value::as_str) {
        Some("workspace/configuration") => {
            let len = msg
                .get("params")
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Value::Array((0..len).map(|_| settings.clone()).collect())
        }
        Some("client/registerCapability")
        | Some("client/unregisterCapability")
        | Some("window/workDoneProgress/create") => Value::Null,
        _ => Value::Null,
    }
}

fn workspace_candidate_servers(
    servers: &HashMap<String, ServerEntry>,
    path_glob: Option<&str>,
) -> Vec<String> {
    let hinted_ext = path_glob.and_then(extension_from_pattern);
    let mut names = servers
        .iter()
        .filter(|(name, entry)| {
            hinted_ext.is_none_or(|ext| server_matches_extension(name, &entry.config, ext))
        })
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if names.is_empty() && servers.len() == 1 {
        names.extend(servers.keys().cloned());
    }
    names.sort();
    names
}

fn extension_from_pattern(pattern: &str) -> Option<&str> {
    let trimmed = pattern.trim_end_matches(['*', '/', '.']);
    Path::new(trimmed)
        .extension()?
        .to_str()
        .filter(|ext| !ext.is_empty())
}

fn server_matches_extension(name: &str, config: &LspServerConfig, ext: &str) -> bool {
    let default_language = default_language_id_for_extension(ext);
    config.extensions.iter().any(|item| item == ext)
        || name == ext
        || default_language.is_some_and(|language| name == language)
        || config
            .language_id
            .as_deref()
            .is_some_and(|language_id| language_id == ext || default_language == Some(language_id))
}

fn pick_server(servers: &HashMap<String, ServerEntry>, file: &Path) -> Option<String> {
    let ext = file.extension().and_then(|s| s.to_str()).unwrap_or("");
    let mut matches = servers
        .iter()
        .filter(|(name, entry)| server_matches_extension(name, &entry.config, ext))
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    matches.sort();
    matches.into_iter().next().or_else(|| {
        if servers.len() == 1 {
            servers.keys().next().cloned()
        } else {
            None
        }
    })
}

fn find_root(file: &Path, markers: &[String]) -> PathBuf {
    let start = if file.is_dir() {
        file.to_path_buf()
    } else {
        file.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };
    let mut dirs = Vec::new();
    let mut dir = start;
    loop {
        dirs.push(dir.clone());
        if !dir.pop() {
            break;
        }
    }

    for vcs_marker in [".git", ".hg", ".svn"] {
        if markers.iter().any(|marker| marker == vcs_marker) {
            if let Some(root) = dirs.iter().find(|dir| dir.join(vcs_marker).exists()) {
                return root.clone();
            }
        }
    }
    dirs.into_iter()
        .find(|dir| markers.iter().any(|marker| dir.join(marker).exists()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn text_document(file_path: &str) -> Result<Value, String> {
    Ok(json!({ "uri": file_uri(file_path)? }))
}

fn text_position_params(
    file_path: &str,
    content: &str,
    line: u64,
    column: u64,
) -> Result<Value, String> {
    Ok(json!({
        "textDocument": text_document(file_path)?,
        "position": lsp_position(content, line, column)?
    }))
}

fn lsp_position(content: &str, line: u64, column: u64) -> Result<Value, String> {
    let line_index = line.saturating_sub(1) as usize;
    let char_column = column.saturating_sub(1) as usize;
    let line_text = content
        .split_inclusive('\n')
        .nth(line_index)
        .map(|segment| segment.strip_suffix('\n').unwrap_or(segment))
        .ok_or_else(|| format!("line {line} is out of range"))?;
    Ok(json!({ "line": line_index, "character": char_column_to_utf16(line_text, char_column) }))
}

fn language_id(file_path: &str, config: &LspServerConfig) -> String {
    if let Some(language_id) = config
        .language_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        return language_id.to_string();
    }
    let ext = Path::new(file_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    default_language_id_for_extension(ext)
        .map(str::to_string)
        .unwrap_or_else(|| ext.to_string())
}

fn default_language_id_for_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rust"),
        "ts" => Some("typescript"),
        "tsx" => Some("typescriptreact"),
        "js" => Some("javascript"),
        "jsx" => Some("javascriptreact"),
        "py" => Some("python"),
        "go" => Some("go"),
        "lua" => Some("lua"),
        _ => None,
    }
}

fn file_uri(path: &str) -> Result<String, String> {
    path_to_uri(&PathBuf::from(path))
}

fn path_to_uri(path: &Path) -> Result<String, String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let url = url::Url::from_file_path(abs).map_err(|_| "could not convert path to file URI")?;
    Ok(url.to_string())
}

fn uri_to_path(uri: &str) -> Result<PathBuf, String> {
    let url = url::Url::parse(uri).map_err(|e| e.to_string())?;
    url.to_file_path()
        .map_err(|_| format!("not a file URI: {uri}"))
}

fn workspace_edit_summary(edit: &Value) -> Value {
    let mut files = Vec::new();
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            files.push(json!({ "uri": uri, "edits": edits.as_array().map_or(0, Vec::len) }));
        }
    }
    if let Some(changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            let uri = change
                .get("textDocument")
                .and_then(|d| d.get("uri"))
                .and_then(Value::as_str)
                .or_else(|| change.get("uri").and_then(Value::as_str));
            if let Some(uri) = uri {
                files.push(json!({ "uri": uri, "edits": change.get("edits").and_then(Value::as_array).map_or(0, Vec::len) }));
            }
        }
    }
    json!({ "files": files, "file_count": files.len() })
}

fn apply_workspace_edit(edit: &Value) -> Result<(), String> {
    let mut by_uri: HashMap<String, Vec<Value>> = HashMap::new();
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            let edits = edits
                .as_array()
                .ok_or("LSP workspace edit contains non-array text edits")?;
            by_uri.entry(uri.clone()).or_default().extend(edits.clone());
        }
    }
    if let Some(changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            let uri = change
                .get("textDocument")
                .and_then(|d| d.get("uri"))
                .and_then(Value::as_str)
                .ok_or("LSP workspace edit contains unsupported document change")?;
            let edits = change
                .get("edits")
                .and_then(Value::as_array)
                .ok_or("LSP workspace edit contains unsupported document change")?;
            by_uri
                .entry(uri.to_string())
                .or_default()
                .extend(edits.clone());
        }
    }
    for (uri, edits) in by_uri {
        let path = uri_to_path(&uri)?;
        let path_str = path.to_string_lossy().to_string();
        let _lock = crate::fs::try_flock(&path_str)?;
        let content = crate::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let new_content = apply_text_edits(&content, &edits)?;
        crate::fs::write(&path, new_content.as_bytes())
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn apply_text_edits(content: &str, edits: &[Value]) -> Result<String, String> {
    let mut ranges = Vec::new();
    for edit in edits {
        let range = edit.get("range").ok_or("LSP edit missing range")?;
        let start = range.get("start").ok_or("LSP edit missing start")?;
        let end = range.get("end").ok_or("LSP edit missing end")?;
        let start = position_to_byte(content, start)?;
        let end = position_to_byte(content, end)?;
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        ranges.push((start, end, new_text));
    }
    ranges.sort_by_key(|(start, _, _)| *start);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err("overlapping LSP text edits".into());
        }
    }
    let mut out = content.to_string();
    for (start, end, new_text) in ranges.into_iter().rev() {
        smelt_buffer::text::replace_range(&mut out, start..end, &new_text);
    }
    Ok(out)
}

fn position_to_byte(content: &str, position: &Value) -> Result<usize, String> {
    let line = position
        .get("line")
        .and_then(Value::as_u64)
        .ok_or("position missing line")? as usize;
    let character = position
        .get("character")
        .and_then(Value::as_u64)
        .ok_or("position missing character")? as usize;
    let mut byte = 0usize;
    for (idx, segment) in content.split_inclusive('\n').enumerate() {
        if idx == line {
            let line_text = segment.strip_suffix('\n').unwrap_or(segment);
            return Ok(byte + utf16_col_to_byte(line_text, character));
        }
        byte += segment.len();
    }
    if line == content.lines().count() {
        return Ok(content.len());
    }
    Err("position line out of range".into())
}

fn char_column_to_utf16(line: &str, col: usize) -> usize {
    line.chars().take(col).map(char::len_utf16).sum()
}

fn utf16_col_to_byte(line: &str, col: usize) -> usize {
    let mut units = 0usize;
    for (byte, ch) in line.char_indices() {
        if units >= col {
            return byte;
        }
        units += ch.len_utf16();
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};

    fn progress_message(token: &str, kind: &str, title: Option<&str>) -> Value {
        let mut value = json!({ "kind": kind });
        if let Some(title) = title {
            value["title"] = json!(title);
        }
        json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": token, "value": value }
        })
    }

    fn server_status(quiescent: bool) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "experimental/serverStatus",
            "params": { "health": "ok", "quiescent": quiescent }
        })
    }

    fn read_fake_lsp_message(reader: &mut impl BufRead) -> Option<Value> {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
        let mut body = vec![0; content_length?];
        reader.read_exact(&mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }

    fn write_fake_lsp_message(message: &Value) {
        let body = serde_json::to_vec(message).unwrap();
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
        stdout.write_all(&body).unwrap();
        stdout.flush().unwrap();
    }

    fn fake_workspace_symbol_response(request: &Value, uri: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": [{
                "name": "ReadySymbol",
                "kind": 23,
                "location": {
                    "uri": uri,
                    "range": {
                        "start": { "line": 2, "character": 4 },
                        "end": { "line": 2, "character": 15 }
                    }
                }
            }]
        })
    }

    #[test]
    fn fake_workspace_loading_lsp_server_process() {
        let mut stdin = std::io::BufReader::new(std::io::stdin().lock());
        let Some(initialize) = read_fake_lsp_message(&mut stdin) else {
            return;
        };
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(
            initialize["params"]["capabilities"]["window"]["workDoneProgress"],
            true
        );
        assert_eq!(
            initialize["params"]["capabilities"]["experimental"]["serverStatusNotification"],
            true
        );
        write_fake_lsp_message(&json!({
            "jsonrpc": "2.0",
            "id": initialize["id"],
            "result": { "capabilities": { "workspaceSymbolProvider": true } }
        }));

        let initialized = read_fake_lsp_message(&mut stdin).unwrap();
        assert_eq!(initialized["method"], "initialized");
        write_fake_lsp_message(&server_status(false));

        let loading_started = Instant::now();
        let ready = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            write_fake_lsp_message(&server_status(true));
        });
        let request = read_fake_lsp_message(&mut stdin).unwrap();
        assert!(
            loading_started.elapsed() >= Duration::from_millis(80),
            "workspace/symbol was sent while the server was loading"
        );
        ready.join().unwrap();
        assert_eq!(request["method"], "workspace/symbol");
        let path = std::env::current_dir().unwrap().join("src/app_config.rs");
        let uri = path_to_uri(&path).unwrap();
        write_fake_lsp_message(&fake_workspace_symbol_response(&request, &uri));

        while let Some(request) = read_fake_lsp_message(&mut stdin) {
            let method = request.get("method").and_then(Value::as_str).unwrap();
            let result = match method {
                "workspace/symbol" => {
                    write_fake_lsp_message(&fake_workspace_symbol_response(&request, &uri));
                    continue;
                }
                "textDocument/didOpen" | "textDocument/didChange" => continue,
                "textDocument/hover" => json!({ "contents": "ReadySymbol hover" }),
                "textDocument/definition" => json!([]),
                "textDocument/typeDefinition" => json!([]),
                "textDocument/implementation" => json!([]),
                "textDocument/documentSymbol" => json!([]),
                "shutdown" => Value::Null,
                "exit" => return,
                other => panic!("unexpected fake LSP request: {other}"),
            };
            write_fake_lsp_message(&json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": result,
            }));
            if method == "shutdown" {
                return;
            }
        }
    }

    #[tokio::test]
    async fn first_workspace_symbol_query_waits_for_server_readiness() {
        let executable = std::env::current_exe().unwrap();
        let manager = LspManager::default();
        manager
            .configure(LspConfig {
                servers: HashMap::from([(
                    "fake".to_string(),
                    LspServerConfig {
                        cmd: vec![
                            executable.display().to_string(),
                            "--exact".into(),
                            "lsp::tests::fake_workspace_loading_lsp_server_process".into(),
                            "--nocapture".into(),
                            "--test-threads=1".into(),
                        ],
                        extensions: vec!["rs".into()],
                        language_id: Some("rust".into()),
                        root_markers: Vec::new(),
                        init_timeout_ms: 2_000,
                        request_timeout_ms: 2_000,
                        startup_wait_ms: 1_000,
                        initialization_options: Value::Null,
                        settings: Value::Null,
                    },
                )]),
            })
            .await;

        let result = manager
            .workspace_symbols("ReadySymbol", Some("struct"), None, 20, true)
            .await
            .unwrap();
        assert_eq!(result["total"], 1);
        assert_eq!(result["symbols"][0]["name"], "ReadySymbol");
        assert_eq!(result["symbols"][0]["file_path"], "src/app_config.rs");

        let inspected = manager
            .dispatch_local(
                "inspect_symbol",
                json!({ "query": "ReadySymbol", "exact": true, "depth": 0 }),
            )
            .await
            .unwrap();
        assert_eq!(inspected["position"]["file_path"], "src/app_config.rs");
        assert_eq!(inspected["hover"], "ReadySymbol hover");
        manager.shutdown_all().await;
    }

    #[test]
    fn filtered_outline_totals_only_visible_symbols() {
        let symbols = normalize_document_symbols(&json!([
            {
                "name": "Wanted",
                "kind": 23,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 0 } },
                "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 6 } }
            },
            {
                "name": "Ignored",
                "kind": 23,
                "range": { "start": { "line": 4, "character": 0 }, "end": { "line": 6, "character": 0 } },
                "selectionRange": { "start": { "line": 4, "character": 0 }, "end": { "line": 4, "character": 7 } }
            }
        ]));
        let (total, visible) = limited_outline_symbols(
            &symbols,
            OutlineFilter {
                symbol: Some("Wanted"),
                kind: Some("struct"),
                name_contains: None,
                max_depth: None,
            },
            20,
        );

        assert_eq!(total, 1);
        assert_eq!(count_compact_outline_symbols(&visible), 1);
    }

    #[test]
    fn raw_reference_results_honor_the_requested_limit() {
        let result = raw_location_summary(json!([{"id": 1}, {"id": 2}, {"id": 3}]), 2);

        assert_eq!(result["total"], 3);
        assert_eq!(result["shown"], 2);
        assert_eq!(result["omitted"], 1);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["locations"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn diagnostics_wait_for_a_fresh_publication() {
        let diagnostics = Arc::new(DiagnosticsStore::new());
        let generation = diagnostics.generation("file:///test.rs").await;
        let publisher = diagnostics.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            publisher.publish("file:///test.rs".into(), json!([])).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            publisher
                .publish("file:///test.rs".into(), json!([{"message": "broken"}]))
                .await;
        });

        let result = diagnostics
            .wait_for_update("file:///test.rs", generation, Duration::from_secs(1))
            .await;
        assert_eq!(result[0]["message"], "broken");
    }

    #[test]
    fn workspace_progress_tracks_loading_work_only() {
        let tracker = WorkspaceProgressTracker::new();
        tracker.record(&progress_message("check", "begin", Some("cargo check")));
        assert!(!tracker.updates.borrow().seen);

        tracker.record(&progress_message("roots", "begin", Some("Roots Scanned")));
        assert!(tracker.updates.borrow().seen);
        assert_eq!(
            tracker
                .updates
                .borrow()
                .active
                .get("roots")
                .map(String::as_str),
            Some("Roots Scanned")
        );

        tracker.record(&progress_message("roots", "end", None));
        assert!(tracker.updates.borrow().active.is_empty());
    }

    #[tokio::test]
    async fn workspace_progress_waits_for_loading_to_finish() {
        let tracker = WorkspaceProgressTracker::new();
        tracker.mark_initialized();
        tracker.record(&progress_message("index", "begin", Some("Indexing")));
        let completion = tracker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            completion.record(&progress_message("index", "end", None));
        });

        tracker
            .wait_until_ready(Duration::from_secs(1), Duration::from_millis(10))
            .await
            .unwrap();
        assert!(tracker.updates.borrow().active.is_empty());
    }

    #[tokio::test]
    async fn workspace_progress_uses_server_quiescence_when_available() {
        let tracker = WorkspaceProgressTracker::new();
        tracker.mark_initialized();
        tracker.record(&server_status(false));
        let completion = tracker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            completion.record(&server_status(true));
        });

        tracker
            .wait_until_ready(Duration::from_secs(1), Duration::from_millis(1))
            .await
            .unwrap();
        assert_eq!(tracker.updates.borrow().server_quiescent, Some(true));
    }

    #[tokio::test]
    async fn workspace_progress_timeout_is_an_error_not_an_empty_result() {
        let tracker = WorkspaceProgressTracker::new();
        tracker.mark_initialized();
        tracker.record(&progress_message("index", "begin", Some("Indexing")));

        let error = tracker
            .wait_until_ready(Duration::from_millis(10), Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(error.contains("still loading"));
        assert!(error.contains("Indexing"));
    }

    fn test_server(cmd: &str, language_id: &str, extensions: &[&str]) -> ServerEntry {
        ServerEntry {
            config: LspServerConfig {
                cmd: vec![cmd.into()],
                extensions: extensions.iter().map(|ext| (*ext).into()).collect(),
                language_id: Some(language_id.into()),
                root_markers: Vec::new(),
                init_timeout_ms: default_init_timeout_ms(),
                request_timeout_ms: default_request_timeout_ms(),
                startup_wait_ms: default_startup_wait_ms(),
                initialization_options: Value::Null,
                settings: Value::Null,
            },
            clients: HashMap::new(),
        }
    }

    #[test]
    fn lsp_position_uses_character_columns() {
        let content = "a😀b\nnext";
        assert_eq!(
            lsp_position(content, 1, 1).unwrap(),
            json!({ "line": 0, "character": 0 })
        );
        assert_eq!(
            lsp_position(content, 1, 3).unwrap(),
            json!({ "line": 0, "character": 3 })
        );
        assert_eq!(
            lsp_position(content, 2, 3).unwrap(),
            json!({ "line": 1, "character": 2 })
        );
    }

    #[test]
    fn position_to_byte_uses_utf16_columns() {
        let content = "a😀b\nnext";
        assert_eq!(
            position_to_byte(content, &json!({ "line": 0, "character": 0 })).unwrap(),
            0
        );
        assert_eq!(
            position_to_byte(content, &json!({ "line": 0, "character": 1 })).unwrap(),
            1
        );
        assert_eq!(
            position_to_byte(content, &json!({ "line": 0, "character": 3 })).unwrap(),
            5
        );
        assert_eq!(
            position_to_byte(content, &json!({ "line": 1, "character": 2 })).unwrap(),
            9
        );
    }

    #[test]
    fn apply_text_edits_applies_from_end() {
        let edits = vec![
            json!({ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "newText": "A" }),
            json!({ "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 4 } }, "newText": "B" }),
        ];
        assert_eq!(apply_text_edits("a😀\nnext", &edits).unwrap(), "A😀\nB");
    }

    #[test]
    fn normalizes_document_symbols_and_outline_path() {
        let symbols = normalize_document_symbols(&json!([
            {
                "name": "outer",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 8 } },
                "children": [
                    {
                        "name": "inner",
                        "kind": 6,
                        "detail": "&self",
                        "range": { "start": { "line": 2, "character": 4 }, "end": { "line": 4, "character": 5 } },
                        "selectionRange": { "start": { "line": 2, "character": 7 }, "end": { "line": 2, "character": 12 } }
                    }
                ]
            }
        ]));

        assert_eq!(symbols[0].name, "outer");
        assert_eq!(symbols[0].kind, "function");
        assert_eq!(symbols[0].children[0].kind, "method");
        assert_eq!(
            enclosing_symbol_name(&symbols, 3, 5).as_deref(),
            Some("method inner")
        );
        assert_eq!(
            outline_path_at_position(&symbols, 3, 5),
            json!([
                {
                    "name": "outer",
                    "kind": "function",
                    "range": { "start": { "line": 1, "column": 1 }, "end": { "line": 10, "column": 2 } }
                },
                {
                    "name": "inner",
                    "kind": "method",
                    "range": { "start": { "line": 3, "column": 5 }, "end": { "line": 5, "column": 6 } }
                }
            ])
        );
    }

    #[test]
    fn normalizes_locations_and_kind_aliases() {
        let refs = normalize_locations(&json!([
            {
                "targetUri": "file:///tmp/demo.rs",
                "targetSelectionRange": { "start": { "line": 4, "character": 2 }, "end": { "line": 4, "character": 7 } }
            }
        ]));
        assert_eq!(refs[0].file_path, "/tmp/demo.rs");
        assert_eq!(refs[0].line, 5);
        assert_eq!(refs[0].column, 3);
        assert_eq!(normalize_kind_filter("trait"), "interface");
        assert_eq!(normalize_kind_filter("fn"), "function");
        assert_eq!(normalize_kind_filter("const"), "constant");
    }

    #[test]
    fn pick_server_uses_file_language_not_server_own_name() {
        let mut servers = HashMap::new();
        servers.insert(
            "typescript".to_string(),
            test_server("typescript-language-server", "typescript", &["ts", "js"]),
        );
        servers.insert(
            "rust-analyzer".to_string(),
            test_server("rust-analyzer", "rust", &["rs"]),
        );

        assert_eq!(
            pick_server(&servers, Path::new("crates/core/src/session.rs")),
            Some("rust-analyzer".to_string())
        );
        assert_eq!(
            pick_server(&servers, Path::new("web/src/app.ts")),
            Some("typescript".to_string())
        );
    }

    #[test]
    fn path_globs_hint_workspace_symbol_servers() {
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            test_server("rust-analyzer", "rust", &["rs"]),
        );
        servers.insert(
            "lua-language-server".to_string(),
            test_server("lua-language-server", "lua", &["lua"]),
        );

        assert_eq!(
            workspace_candidate_servers(&servers, Some("crates/**/*.rs")),
            vec!["rust-analyzer".to_string()]
        );
        assert_eq!(workspace_candidate_servers(&servers, None).len(), 2);
    }

    #[test]
    fn find_root_prefers_outer_workspace_root() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let crate_dir = workspace.join("crates/core/src");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(workspace.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::write(workspace.join("crates/core/Cargo.toml"), "[package]").unwrap();

        assert_eq!(
            find_root(
                &crate_dir.join("lib.rs"),
                &["Cargo.toml".to_string(), ".git".to_string()]
            ),
            workspace
        );
    }

    #[test]
    fn find_root_prefers_nested_repository_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let outer = temp.path().join("outer");
        let inner = outer.join("vendor/inner/src");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        std::fs::create_dir_all(outer.join("vendor/inner/.git")).unwrap();
        std::fs::write(outer.join("Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(outer.join("vendor/inner/Cargo.toml"), "[package]").unwrap();

        assert_eq!(
            find_root(
                &inner.join("lib.rs"),
                &["Cargo.toml".to_string(), ".git".to_string()]
            ),
            outer.join("vendor/inner")
        );
    }

    #[tokio::test]
    async fn workspace_symbol_query_without_hint_does_not_start_multiple_servers() {
        let manager = LspManager::default();
        manager
            .configure(LspConfig {
                servers: HashMap::from([
                    (
                        "rust-analyzer".to_string(),
                        test_server("rust-analyzer", "rust", &["rs"]).config,
                    ),
                    (
                        "lua-language-server".to_string(),
                        test_server("lua-language-server", "lua", &["lua"]).config,
                    ),
                ]),
            })
            .await;

        let err = manager
            .workspace_symbols("Thing", None, None, 20, false)
            .await
            .expect_err("ambiguous query should ask for a path_glob");

        assert!(err.contains("path_glob"));
        assert!(err.contains("rust-analyzer"));
        assert!(err.contains("lua-language-server"));
    }

    #[tokio::test]
    async fn older_configure_cannot_replace_newer_lsp_desired_state() {
        let manager = Arc::new(LspManager::default());
        let old_config = LspConfig {
            servers: HashMap::from([(
                "old".into(),
                test_server("unused", "text", &["txt"]).config,
            )]),
        };
        let old_configure = manager
            .prepare_configure(old_config)
            .expect("old desired revision");
        let (control, completion) = crate::test_util::controlled_completion(());
        let old_task = tokio::spawn(async move {
            completion.complete().await;
            old_configure.apply().await;
        });

        let release = control.wait_started().await;
        manager
            .configure(LspConfig {
                servers: HashMap::new(),
            })
            .await;
        release.send(()).unwrap();
        old_task.await.unwrap();

        assert!(
            manager.config_snapshot().servers.is_empty(),
            "an older configure completion must not reinstall a removed server"
        );
        let status = manager.controller_status();
        assert!(manager.prepare_configure(LspConfig::default()).is_none());
        assert_eq!(manager.controller_status(), status);
    }

    #[tokio::test]
    async fn stale_configure_completion_cannot_publish_after_server_removal() {
        let manager = Arc::new(LspManager::default());
        let old_config = LspConfig {
            servers: HashMap::from([(
                "old".into(),
                test_server("unused", "text", &["txt"]).config,
            )]),
        };
        let old_completion = manager
            .prepare_configure(old_config)
            .unwrap()
            .install()
            .unwrap();
        assert!(manager.config_snapshot().servers.contains_key("old"));
        let (control, completion) = crate::test_util::controlled_completion(());
        let old_task = tokio::spawn(async move {
            completion.complete().await;
            old_completion.finish().await;
        });

        let release = control.wait_started().await;
        manager.configure(LspConfig::default()).await;
        let current_status = manager.controller_status();
        release.send(()).unwrap();
        old_task.await.unwrap();

        assert!(manager.config_snapshot().servers.is_empty());
        assert_eq!(manager.controller_status(), current_status);
    }
}
