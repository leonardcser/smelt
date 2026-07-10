use super::{find_root, LspConfig, LspManager};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(not(unix))]
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const STARTUP_WAIT: Duration = Duration::from_secs(10);
const SPAWN_RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Deserialize, Serialize)]
struct DaemonRequest {
    config: LspConfig,
    operation: String,
    args: Value,
}

#[derive(Debug, Deserialize, Serialize)]
struct DaemonResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

pub async fn call(config: LspConfig, operation: &str, args: Value) -> Result<Value, String> {
    call_impl(config, operation, args).await
}

#[cfg(unix)]
async fn call_impl(config: LspConfig, operation: &str, args: Value) -> Result<Value, String> {
    let (socket, root) = socket_path(&config)?;
    let request = DaemonRequest {
        config,
        operation: operation.to_string(),
        args,
    };

    if let Ok(response) = send_request(&socket, &request).await {
        return response.into_result();
    }

    ensure_daemon(&socket, &root).await?;
    let deadline = tokio::time::Instant::now() + STARTUP_WAIT;
    loop {
        match send_request(&socket, &request).await {
            Ok(response) => return response.into_result(),
            Err(err) if tokio::time::Instant::now() < deadline => {
                let _ = err;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(not(unix))]
async fn call_impl(config: LspConfig, operation: &str, args: Value) -> Result<Value, String> {
    static MANAGER: OnceLock<Arc<LspManager>> = OnceLock::new();
    let manager = MANAGER
        .get_or_init(|| Arc::new(LspManager::default()))
        .clone();
    manager.configure(config).await;
    manager.dispatch_local(operation, args).await
}

#[cfg(unix)]
pub async fn run(socket: PathBuf) -> Result<(), String> {
    if let Some(parent) = socket.parent() {
        ensure_private_dir(parent)
            .map_err(|err| format!("prepare LSP daemon dir {}: {err}", parent.display()))?;
    }
    let listener = tokio::net::UnixListener::bind(&socket)
        .map_err(|err| format!("bind LSP daemon socket {}: {err}", socket.display()))?;
    let manager = Arc::new(LspManager::default());

    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, listener.accept()).await {
            Ok(Ok((stream, _))) => {
                let manager = manager.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(manager, stream).await;
                });
            }
            Ok(Err(err)) => {
                manager.shutdown_all().await;
                let _ = tokio::fs::remove_file(&socket).await;
                return Err(format!("accept LSP daemon connection: {err}"));
            }
            Err(_) => {
                manager.shutdown_all().await;
                let _ = tokio::fs::remove_file(&socket).await;
                return Ok(());
            }
        }
    }
}

#[cfg(not(unix))]
pub async fn run(_socket: PathBuf) -> Result<(), String> {
    Err("shared LSP daemon sockets are only supported on Unix".to_string())
}

#[cfg(unix)]
async fn handle_connection(
    manager: Arc<LspManager>,
    stream: tokio::net::UnixStream,
) -> Result<(), String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|err| err.to_string())?;
    let mut stream = reader.into_inner();
    let request: DaemonRequest = serde_json::from_str(&line).map_err(|err| err.to_string())?;
    manager.configure(request.config).await;
    let response = match manager
        .dispatch_local(&request.operation, request.args)
        .await
    {
        Ok(result) => DaemonResponse {
            result: Some(result),
            err: None,
        },
        Err(err) => DaemonResponse {
            result: None,
            err: Some(err),
        },
    };
    write_response(&mut stream, &response).await
}

#[cfg(unix)]
async fn send_request(socket: &Path, request: &DaemonRequest) -> Result<DaemonResponse, String> {
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|err| format!("connect LSP daemon {}: {err}", socket.display()))?;
    let body = serde_json::to_vec(request).map_err(|err| err.to_string())?;
    stream
        .write_all(&body)
        .await
        .map_err(|err| err.to_string())?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|err| err.to_string())?;
    stream.flush().await.map_err(|err| err.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|err| err.to_string())?;
    serde_json::from_str(&line).map_err(|err| err.to_string())
}

#[cfg(unix)]
async fn write_response(
    stream: &mut tokio::net::UnixStream,
    response: &DaemonResponse,
) -> Result<(), String> {
    let body = serde_json::to_vec(response).map_err(|err| err.to_string())?;
    stream
        .write_all(&body)
        .await
        .map_err(|err| err.to_string())?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|err| err.to_string())?;
    stream.flush().await.map_err(|err| err.to_string())
}

impl DaemonResponse {
    fn into_result(self) -> Result<Value, String> {
        if let Some(err) = self.err {
            return Err(err);
        }
        Ok(self.result.unwrap_or(Value::Null))
    }
}

#[cfg(unix)]
async fn ensure_daemon(socket: &Path, root: &Path) -> Result<(), String> {
    if let Some(parent) = socket.parent() {
        ensure_private_dir(parent)
            .map_err(|err| format!("prepare LSP daemon dir {}: {err}", parent.display()))?;
    }
    let lock = socket.with_extension("startup");
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(file) => {
                let _guard = StartupLock {
                    path: lock.clone(),
                    _file: file,
                };
                if tokio::net::UnixStream::connect(socket).await.is_ok() {
                    return Ok(());
                }
                let _ = tokio::fs::remove_file(socket).await;
                spawn_daemon(socket, root).await?;
                return wait_for_socket(socket).await;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if wait_for_socket(socket).await.is_ok() {
                    return Ok(());
                }
                let _ = tokio::fs::remove_file(&lock).await;
            }
            Err(err) => {
                return Err(format!("create LSP daemon lock {}: {err}", lock.display()));
            }
        }
    }
}

#[cfg(unix)]
struct StartupLock {
    path: PathBuf,
    _file: std::fs::File,
}

#[cfg(unix)]
impl Drop for StartupLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
async fn wait_for_socket(socket: &Path) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + STARTUP_WAIT;
    loop {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(_) => return Ok(()),
            Err(err) if tokio::time::Instant::now() < deadline => {
                let _ = err;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => return Err(format!("connect LSP daemon {}: {err}", socket.display())),
        }
    }
}

#[cfg(unix)]
async fn spawn_daemon(socket: &Path, root: &Path) -> Result<(), String> {
    spawn_daemon_with(socket, root, daemon_executable_path).await
}

#[cfg(unix)]
async fn spawn_daemon_with(
    socket: &Path,
    root: &Path,
    executable: impl Fn() -> Result<PathBuf, String>,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + STARTUP_WAIT;
    loop {
        let exe = executable()?;
        let mut command = std::process::Command::new(&exe);
        command
            .arg("lsp-daemon")
            .arg(socket)
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::process::without_controlling_terminal(&mut command);
        match command.spawn() {
            Ok(_) => return Ok(()),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(SPAWN_RETRY_INTERVAL).await;
            }
            Err(err) => {
                return Err(format!("start LSP daemon with {}: {err}", exe.display()));
            }
        }
    }
}

#[cfg(unix)]
fn daemon_executable_path() -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    {
        let proc_exe = PathBuf::from("/proc/self/exe");
        if std::fs::metadata(&proc_exe).is_ok() {
            return Ok(proc_exe);
        }
    }
    std::env::current_exe().map_err(|err| format!("resolve smelt executable: {err}"))
}

#[cfg(unix)]
fn socket_path(config: &LspConfig) -> Result<(PathBuf, PathBuf), String> {
    let cwd = std::env::current_dir().map_err(|err| format!("read current directory: {err}"))?;
    let root_markers = config
        .servers
        .values()
        .flat_map(|server| server.root_markers.iter().cloned())
        .collect::<Vec<_>>();
    let root = canonicalize_lossy(find_root(&cwd, &root_markers));
    let key = daemon_key(&root, config)?;
    let dir = socket_dir()?;
    Ok((dir.join(format!("{key}.sock")), root))
}

#[cfg(unix)]
fn daemon_key(root: &Path, config: &LspConfig) -> Result<String, String> {
    let root = root.to_string_lossy();
    let mut servers = config.servers.iter().collect::<Vec<_>>();
    servers.sort_by(|a, b| a.0.cmp(b.0));
    let servers = servers
        .into_iter()
        .map(|(name, server)| {
            serde_json::to_value(server)
                .map(|value| serde_json::json!([name, stable_value(value)]))
                .map_err(|err| err.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let stable = serde_json::json!({
        "executable": daemon_executable_identity()?,
        "root": root,
        "servers": servers,
    });
    let bytes = serde_json::to_vec(&stable).map_err(|err| err.to_string())?;
    let digest = Sha256::digest(bytes);
    Ok(digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

#[cfg(unix)]
fn daemon_executable_identity() -> Result<Value, String> {
    use std::os::unix::fs::MetadataExt;

    let path = daemon_executable_path()?;
    let metadata = std::fs::metadata(&path)
        .map_err(|err| format!("read smelt executable metadata {}: {err}", path.display()))?;
    Ok(serde_json::json!({
        "path": canonicalize_lossy(path),
        "device": metadata.dev(),
        "inode": metadata.ino(),
        "size": metadata.size(),
        "modified_seconds": metadata.mtime(),
        "modified_nanoseconds": metadata.mtime_nsec(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(unix)]
fn stable_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(stable_value).collect()),
        Value::Object(map) => {
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, stable_value(value)))
                    .collect(),
            )
        }
        value => value,
    }
}

#[cfg(unix)]
fn canonicalize_lossy(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(unix)]
fn socket_dir() -> Result<PathBuf, String> {
    let root = runtime_root()?;
    let smelt = root.join("smelt");
    let lsp = smelt.join("lsp");
    ensure_private_dir(&smelt).map_err(|err| format!("prepare {}: {err}", smelt.display()))?;
    ensure_private_dir(&lsp).map_err(|err| format!("prepare {}: {err}", lsp.display()))?;
    Ok(lsp)
}

#[cfg(unix)]
fn runtime_root() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return Ok(path);
    }
    let path = std::env::temp_dir().join(format!("smelt-runtime-{}", unsafe { libc::geteuid() }));
    ensure_private_dir(&path).map_err(|err| format!("prepare {}: {err}", path.display()))?;
    Ok(path)
}

#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "directory path is a symlink",
        ));
    }
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    let metadata = std::fs::metadata(path)?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "directory is not owned by the current user",
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "directory is accessible by group or other users",
        ));
    }
    Ok(())
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::LspServerConfig;
    use std::collections::HashMap;

    #[test]
    #[cfg(unix)]
    fn daemon_key_ignores_map_insertion_order() {
        let config_a = LspConfig {
            servers: HashMap::from([
                (
                    "rust".to_string(),
                    server_config("rust-analyzer", [("b", 2), ("a", 1)]),
                ),
                (
                    "lua".to_string(),
                    server_config("lua-language-server", [("x", 9)]),
                ),
            ]),
        };
        let config_b = LspConfig {
            servers: HashMap::from([
                (
                    "lua".to_string(),
                    server_config("lua-language-server", [("x", 9)]),
                ),
                (
                    "rust".to_string(),
                    server_config("rust-analyzer", [("a", 1), ("b", 2)]),
                ),
            ]),
        };

        assert_eq!(
            daemon_key(Path::new("/tmp/project"), &config_a).unwrap(),
            daemon_key(Path::new("/tmp/project"), &config_b).unwrap()
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn daemon_spawn_retries_executable_replacement_window() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("smelt");
        let replacement = executable.clone();
        let install = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            tokio::fs::write(&replacement, "#!/bin/sh\nexit 0\n")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();
        });

        spawn_daemon_with(&dir.path().join("daemon.sock"), dir.path(), || {
            Ok(executable.clone())
        })
        .await
        .unwrap();
        install.await.unwrap();
    }

    #[cfg(unix)]
    fn server_config<const N: usize>(cmd: &str, settings: [(&str, i64); N]) -> LspServerConfig {
        let settings = settings
            .into_iter()
            .map(|(key, value)| (key.to_string(), Value::Number(value.into())))
            .collect();
        LspServerConfig {
            cmd: vec![cmd.to_string()],
            extensions: Vec::new(),
            language_id: None,
            root_markers: Vec::new(),
            init_timeout_ms: 1,
            request_timeout_ms: 1,
            startup_wait_ms: 1,
            initialization_options: Value::Null,
            settings: Value::Object(settings),
        }
    }
}
