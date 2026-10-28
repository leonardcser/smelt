//! Claude Code's NDJSON control channel, as used by the official Agent SDK.

use protocol::{AgentMode, ModelTarget, ReasoningEffort};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct LaunchOptions<'a> {
    pub cwd: &'a Path,
    pub target: &'a ModelTarget,
    pub mode: &'a AgentMode,
    pub effort: &'a ReasoningEffort,
    pub resume: Option<&'a str>,
    /// Forks the resumed session after this assistant message.
    pub resume_at: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub override_prompt: Option<&'a str>,
    pub plans_directory: Option<&'a Path>,
    pub disallowed_tools: &'a [String],
    pub tools: bool,
    pub persist_session: bool,
    pub fast_mode: bool,
    pub schema: Option<&'a Value>,
}

pub(super) struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    frame: Vec<u8>,
    stderr: tokio::task::JoinHandle<()>,
    pid: Option<u32>,
}

pub(super) fn control(id: &str, request: Value) -> Value {
    json!({"type":"control_request", "request_id":id, "request":request})
}

pub(super) fn reply(id: &str, response: Value) -> Value {
    json!({"type":"control_response", "response":{
        "subtype":"success", "request_id":id, "response":response
    }})
}

pub(super) fn permission_mode(mode: &AgentMode) -> &'static str {
    if mode.as_str() == "plan" {
        "plan"
    } else {
        "default"
    }
}

/// `SMELT_CLAUDE_CODE_EXECUTABLE`, or `claude` on `PATH`.
fn executable() -> std::ffi::OsString {
    std::env::var_os("SMELT_CLAUDE_CODE_EXECUTABLE")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "claude.exe"
            } else {
                "claude"
            }
            .into()
        })
}

/// The Claude Code executable's path, found without starting it.
pub(super) fn locate() -> Option<PathBuf> {
    let executable = PathBuf::from(executable());
    if executable.components().count() > 1 {
        return executable.is_file().then_some(executable);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(&executable))
        .find(|path| path.is_file())
}

/// Whether the Claude Code executable exists, without starting it.
pub fn installed() -> bool {
    locate().is_some()
}

impl Process {
    pub(super) fn spawn(options: LaunchOptions<'_>) -> Result<Self, String> {
        let LaunchOptions {
            cwd,
            target,
            mode,
            effort,
            resume,
            resume_at,
            prompt,
            override_prompt,
            plans_directory,
            disallowed_tools,
            tools,
            persist_session,
            fast_mode,
            schema,
        } = options;
        if !target.api_base.is_empty() || !target.api_key.is_empty() {
            return Err("claude-code uses Claude Code's own authentication and endpoint configuration; remove api_base and api_key_env".into());
        }
        let executable = executable();
        #[cfg(windows)]
        if Path::new(&executable).extension().is_some_and(|extension| {
            ["cmd", "bat", "ps1"]
                .iter()
                .any(|blocked| extension.eq_ignore_ascii_case(blocked))
        }) {
            return Err("Use the native claude.exe executable, not a shell launcher".into());
        }
        let mut command = Command::new(executable);
        command
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--replay-user-messages",
                "--permission-prompt-tool",
                "stdio",
                "--model",
            ])
            .arg(&target.model)
            .args(["--permission-mode", permission_mode(mode)])
            .args(["--tools", if tools { "default" } else { "" }])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(cwd)
            .kill_on_drop(true)
            .env("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1");
        if !persist_session {
            command.arg("--no-session-persistence");
        }
        if let Some(session) = resume {
            command.arg(format!("--resume={session}"));
            if let Some(message) = resume_at {
                command
                    .arg(format!("--resume-session-at={message}"))
                    .arg("--fork-session");
            }
        }
        if let Some(prompt) = override_prompt {
            command.arg("--system-prompt").arg(prompt);
        }
        if let Some(prompt) = prompt.filter(|s| !s.is_empty()) {
            // Keep the native Claude Code preset; do not impersonate it with smelt's prompt.
            command.arg("--append-system-prompt").arg(prompt);
        }
        if !matches!(effort, ReasoningEffort::Off) {
            command.arg("--effort").arg(effort.label());
        }
        if let Some(schema) = schema {
            command.arg("--json-schema").arg(schema.to_string());
        }
        let mut settings = json!({
            "alwaysThinkingEnabled": !matches!(effort, ReasoningEffort::Off),
            "fastMode": fast_mode && target.config.supports_fast_mode == Some(true),
            "disableAllHooks": !tools,
        });
        if let Some(directory) = plans_directory {
            settings["plansDirectory"] = json!(directory);
        }
        command.arg("--settings").arg(settings.to_string());
        if tools {
            if !disallowed_tools.is_empty() {
                command
                    .arg("--disallowedTools")
                    .arg(disallowed_tools.join(","));
            }
            command.arg("--mcp-config").arg(
                json!({"mcpServers":{
                    "smelt":{"type":"sdk", "name":"smelt"}
                }})
                .to_string(),
            );
        } else {
            command
                .arg("--strict-mcp-config")
                .arg("--mcp-config")
                .arg("{\"mcpServers\":{}}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = command.spawn().map_err(|e| format!(
            "could not start Claude Code: {e}. Install Claude Code, run `claude auth login`, or set SMELT_CLAUDE_CODE_EXECUTABLE to its executable path"
        ))?;
        let stdin = child
            .stdin
            .take()
            .ok_or("Claude Code stdin is unavailable")?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or("Claude Code stdout is unavailable")?,
        );
        let mut stderr = child
            .stderr
            .take()
            .ok_or("Claude Code stderr is unavailable")?;
        // Drain all stderr without retaining unbounded output or logging credentials.
        let stderr = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
            }
        });
        Ok(Self {
            pid: child.id(),
            child,
            stdin: Some(stdin),
            stdout,
            frame: Vec::new(),
            stderr,
        })
    }

    /// Starts Claude Code without tools, persistence or a conversation, to ask
    /// it about its installation or account.
    pub(super) fn spawn_bare() -> Result<Self, String> {
        Self::spawn(LaunchOptions {
            cwd: &std::env::temp_dir(),
            target: &ModelTarget {
                model: "default".into(),
                api_base: String::new(),
                api_key: String::new(),
                provider_type: "claude-code".into(),
                provider: "claude-code".into(),
                config: Default::default(),
            },
            mode: &AgentMode::normal(),
            effort: &ReasoningEffort::Off,
            resume: None,
            resume_at: None,
            prompt: None,
            override_prompt: None,
            plans_directory: None,
            disallowed_tools: &[],
            tools: false,
            persist_session: false,
            fast_mode: false,
            schema: None,
        })
    }

    /// Sends a control request and returns its response, skipping other output.
    pub(super) async fn ask(&mut self, id: &str, request: Value) -> Result<Value, String> {
        self.write(control(id, request)).await?;
        loop {
            let mut frame = self
                .read()
                .await?
                .ok_or("Claude Code exited before it answered")?;
            if frame["type"] != "control_response" || frame["response"]["request_id"] != id {
                continue;
            }
            if frame["response"]["subtype"] == "error" {
                return Err(frame["response"]["error"]
                    .as_str()
                    .unwrap_or("Claude Code rejected the request")
                    .to_owned());
            }
            return Ok(frame["response"]["response"].take());
        }
    }

    pub(super) async fn write(&mut self, message: Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        let stdin = self
            .stdin
            .as_mut()
            .ok_or("Claude Code control channel is closed")?;
        tokio::time::timeout(Duration::from_secs(30), stdin.write_all(&bytes))
            .await
            .map_err(|_| "Claude Code stopped reading its control channel".to_string())?
            .map_err(|e| format!("Claude Code control write failed: {e}"))
    }

    pub(super) async fn read(&mut self) -> Result<Option<Value>, String> {
        loop {
            // Keep partial frames on the process: select! may cancel this read
            // whenever a command or permission response arrives.
            let available = self
                .stdout
                .fill_buf()
                .await
                .map_err(|e| format!("Claude Code output read failed: {e}"))?;
            if available.is_empty() {
                if self.frame.is_empty() {
                    return Ok(None);
                }
                return Err("Claude Code exited with an incomplete output frame".into());
            }
            let end = available.iter().position(|b| *b == b'\n');
            let n = end.map_or(available.len(), |end| end + 1);
            if self.frame.len().saturating_add(n) > MAX_FRAME_BYTES {
                return Err("Claude Code output frame exceeds 16 MiB".into());
            }
            self.frame.extend_from_slice(&available[..n]);
            self.stdout.consume(n);
            if end.is_none() {
                continue;
            }
            let frame = std::mem::take(&mut self.frame);
            // Some CLI builds emit diagnostic lines on stdout. Match the
            // official SDK's handling, without exposing their contents in logs.
            if frame.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
                continue;
            }
            return serde_json::from_slice(&frame)
                .map(Some)
                .map_err(|_| "Claude Code returned invalid JSON on its control channel".into());
        }
    }

    pub(super) async fn close(&mut self) {
        // End stdin first, then bound shutdown. kill_on_drop also covers unwinding.
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.shutdown().await;
            drop(stdin);
        }
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            self.kill_group();
            let _ = self.child.kill().await;
        }
        self.kill_group();
        self.pid = None;
        self.stderr.abort();
    }

    fn kill_group(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // This process group was created exclusively for this child at spawn.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill_group();
        self.stderr.abort();
    }
}
