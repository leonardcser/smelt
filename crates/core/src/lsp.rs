use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};

#[derive(Clone, Debug, serde::Deserialize)]
pub struct LspServerConfig {
    pub cmd: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
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

#[derive(Clone, Debug, serde::Deserialize)]
pub struct LspConfig {
    #[serde(default = "default_start_policy")]
    pub start: String,
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

enum ClientForFile {
    Ready(Arc<LspClient>),
    NotReady(Value),
}

#[derive(Default)]
pub struct LspManager {
    servers: StdMutex<HashMap<String, ServerEntry>>,
}

type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
type DiagnosticsByUri = Arc<Mutex<HashMap<String, Value>>>;
type StderrTail = Arc<Mutex<VecDeque<String>>>;

struct LspClient {
    name: String,
    root: PathBuf,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingRequests,
    diagnostics: DiagnosticsByUri,
    document_versions: Mutex<HashMap<String, i32>>,
    request_timeout_ms: u64,
    init_timeout_ms: u64,
    initialization_options: Value,
    next_id: AtomicU64,
    _child: Mutex<Child>,
}

fn default_start_policy() -> String {
    "background".to_string()
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

impl LspManager {
    pub fn configure_sync(&self, config: LspConfig) {
        let start_background = config.start == "background";
        let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        servers.clear();
        for (name, config) in config.servers {
            let mut entry = ServerEntry {
                config,
                clients: HashMap::new(),
            };
            if start_background {
                let root = find_root(
                    &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                    &entry.config.root_markers,
                );
                entry.clients.insert(
                    root.clone(),
                    LspClientSlot::start(name.clone(), entry.config.clone(), root),
                );
            }
            servers.insert(name, entry);
        }
    }

    pub async fn status(&self) -> String {
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
        let client = match self.client_for_file(file_path).await? {
            ClientForFile::Ready(client) => client,
            ClientForFile::NotReady(value) => return Ok(value),
        };
        client.sync_document(file_path).await?;
        client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": text_document(file_path)? }),
            )
            .await
    }

    pub async fn definition(
        &self,
        file_path: &str,
        line: u64,
        column: u64,
    ) -> Result<Value, String> {
        let client = match self.client_for_file(file_path).await? {
            ClientForFile::Ready(client) => client,
            ClientForFile::NotReady(value) => return Ok(value),
        };
        let text = client.sync_document(file_path).await?;
        client
            .request(
                "textDocument/definition",
                text_position_params(file_path, &text, line, column)?,
            )
            .await
    }

    pub async fn references(
        &self,
        file_path: &str,
        line: u64,
        column: u64,
        include_declaration: bool,
    ) -> Result<Value, String> {
        let client = match self.client_for_file(file_path).await? {
            ClientForFile::Ready(client) => client,
            ClientForFile::NotReady(value) => return Ok(value),
        };
        let text = client.sync_document(file_path).await?;
        let mut params = text_position_params(file_path, &text, line, column)?;
        params["context"] = json!({ "includeDeclaration": include_declaration });
        client.request("textDocument/references", params).await
    }

    pub async fn diagnostics(&self, file_path: Option<&str>) -> Result<Value, String> {
        if let Some(path) = file_path {
            let client = match self.client_for_file(path).await? {
                ClientForFile::Ready(client) => client,
                ClientForFile::NotReady(value) => return Ok(value),
            };
            client.sync_document(path).await?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            let uri = file_uri(path)?;
            let diagnostics = client.diagnostics.lock().await;
            return Ok(diagnostics.get(&uri).cloned().unwrap_or_else(|| json!([])));
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
            let diagnostics = client.diagnostics.lock().await;
            for (uri, value) in diagnostics.iter() {
                out.insert(uri.clone(), value.clone());
            }
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
        let client = match self.client_for_file(file_path).await? {
            ClientForFile::Ready(client) => client,
            ClientForFile::NotReady(value) => return Ok(value),
        };
        let text = client.sync_document(file_path).await?;
        let mut params = text_position_params(file_path, &text, line, column)?;
        params["newName"] = json!(new_name);
        let edit = client.request("textDocument/rename", params).await?;
        let summary = workspace_edit_summary(&edit);
        if apply {
            apply_workspace_edit(&edit)?;
        }
        Ok(json!({ "applied": apply, "summary": summary, "edit": edit }))
    }

    async fn client_for_file(&self, file_path: &str) -> Result<ClientForFile, String> {
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
        Ok(match slot.wait_ready().await {
            Ok(client) => ClientForFile::Ready(client),
            Err(value) => {
                let mut value = value;
                value["server"] = json!(server_name);
                ClientForFile::NotReady(value)
            }
        })
    }
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
                LspClientState::Ready { init_ms, .. } => {
                    lines.push("  state: ready".into());
                    lines.push(format!("  initialized_in: {init_ms}ms"));
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
        let client = Arc::new(Self {
            name,
            root,
            stdin: Arc::new(Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            document_versions: Mutex::new(HashMap::new()),
            request_timeout_ms: config.request_timeout_ms,
            init_timeout_ms: config.init_timeout_ms,
            initialization_options: config.initialization_options.clone(),
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        });
        spawn_stderr_reader(stderr, stderr_tail);
        spawn_reader(
            stdout,
            client.stdin.clone(),
            client.pending.clone(),
            client.diagnostics.clone(),
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
                            "references": {},
                            "rename": { "prepareSupport": false },
                            "publishDiagnostics": {}
                        },
                        "workspace": { "workspaceEdit": { "documentChanges": true } }
                    },
                    "initializationOptions": self.initialization_options.clone()
                }),
                Duration::from_millis(self.init_timeout_ms),
            )
            .await?;
        self.notify("initialized", json!({})).await?;
        Ok(result).map(|_| ())
    }

    async fn sync_document(&self, file_path: &str) -> Result<String, String> {
        let uri = file_uri(file_path)?;
        let text = tokio::fs::read_to_string(file_path)
            .await
            .map_err(|e| format!("read {}: {e}", file_path))?;
        let version = {
            let mut versions = self.document_versions.lock().await;
            let version = versions
                .entry(uri.clone())
                .and_modify(|v| *v += 1)
                .or_insert(1);
            *version
        };

        let result = if version == 1 {
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id(file_path),
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
            let mut versions = self.document_versions.lock().await;
            if version == 1 {
                versions.remove(&uri);
            } else if let Some(current) = versions.get_mut(&uri) {
                *current -= 1;
            }
            return Err(err);
        }
        Ok(text)
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
            } else if msg.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                if let Some(params) = msg.get("params") {
                    if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                        let value = params
                            .get("diagnostics")
                            .cloned()
                            .unwrap_or_else(|| json!([]));
                        diagnostics.lock().await.insert(uri.to_string(), value);
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

fn pick_server(servers: &HashMap<String, ServerEntry>, file: &Path) -> Option<String> {
    let ext = file.extension().and_then(|s| s.to_str()).unwrap_or("");
    let language_id = language_id(file.to_str().unwrap_or_default());
    for (name, entry) in servers {
        if entry
            .config
            .languages
            .iter()
            .any(|ft| ft == ext || ft == &language_id || ft == name)
            || name == ext
            || name == &language_id
        {
            return Some(name.clone());
        }
    }
    if servers.len() == 1 {
        return servers.keys().next().cloned();
    }
    None
}

fn find_root(file: &Path, markers: &[String]) -> PathBuf {
    let mut dir = if file.is_dir() {
        file.to_path_buf()
    } else {
        file.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };
    loop {
        if markers.iter().any(|m| dir.join(m).exists()) {
            return dir;
        }
        if !dir.pop() {
            return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        }
    }
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

fn language_id(file_path: &str) -> String {
    match Path::new(file_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
    {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "lua" => "lua",
        other => other,
    }
    .to_string()
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
            if let Some(edits) = edits.as_array() {
                by_uri.entry(uri.clone()).or_default().extend(edits.clone());
            }
        }
    }
    if let Some(changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            let uri = change
                .get("textDocument")
                .and_then(|d| d.get("uri"))
                .and_then(Value::as_str);
            let edits = change.get("edits").and_then(Value::as_array);
            if let (Some(uri), Some(edits)) = (uri, edits) {
                by_uri
                    .entry(uri.to_string())
                    .or_default()
                    .extend(edits.clone());
            }
        }
    }
    for (uri, edits) in by_uri {
        let path = uri_to_path(&uri)?;
        let content =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let new_content = apply_text_edits(&content, &edits)?;
        std::fs::write(&path, new_content).map_err(|e| format!("write {}: {e}", path.display()))?;
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
}
