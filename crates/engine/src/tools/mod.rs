pub mod file_state;
pub mod notebook;
pub(crate) mod result_dedup;
pub(crate) mod web_cache;

pub use file_state::{file_mtime_ms, staleness_error, FileState, FileStateCache};

use crate::provider::{FunctionSchema, ToolDefinition};
pub use notebook::NotebookRenderData;
use protocol::ToolHooks;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub(crate) struct ToolResult {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    /// Structured metadata passed through to ToolOutcome for machine-readable data.
    pub(crate) metadata: Option<serde_json::Value>,
}

impl ToolResult {
    pub(crate) fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: None,
        }
    }

    pub(crate) fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: None,
        }
    }

    pub(crate) fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Context provided to tools during execution. All Tool impls left in
/// engine (MCP adapters) ignore it — kept as a placeholder so the
/// trait signature can grow back if a future engine-side tool needs
/// cancel propagation or other engine facilities.
pub(crate) struct ToolContext;

pub(crate) type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

pub(crate) trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execute<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolContext) -> ToolFuture<'a>;
    /// Evaluate per-call permission hooks. Returns a `ToolHooks`
    /// carrying:
    /// - `needs_confirm`: confirm-dialog message (None falls back to the
    ///   tool name).
    /// - `approval_patterns`: glob patterns offered as session-level
    ///   "always allow" choices.
    /// - `preflight_error`: pre-execution error that skips the dialog
    ///   and fails the call immediately.
    ///
    /// Mirrors the shape returned by plugin tools through
    /// `ToolHooksRequest` so the engine consumes both paths
    /// uniformly.
    fn evaluate_hooks(&self, _args: &HashMap<String, Value>) -> ToolHooks {
        ToolHooks::default()
    }
}

pub(crate) struct ToolEntry {
    pub(crate) tool: Box<dyn Tool>,
    /// MCP tools use the `mcp` permission ruleset rather than the per-tool
    /// `tools` ruleset; tracked here so the trait stays dispatch-only.
    pub(crate) is_mcp: bool,
}

/// Resolves and executes tool calls during a turn. The engine
/// never touches tool impls directly — every per-call decision (schema
/// list, hook eval, dispatch, ruleset selection) routes through this
/// trait. A future tui-side `ToolRuntime` walks a Lua-driven registry
/// behind the same surface.
///
/// Lookup, hook evaluation, and dispatch all return `Option` so the
/// engine can synthesise a "tool not found" result when the LLM emits
/// a call for a tool the dispatcher doesn't know.
///
/// The trait carries no permission policy — engine applies its rules
/// over the unfiltered tool list returned by `definitions`, using
/// `is_mcp` to pick the right ruleset. When permissions move to Lua
/// hooks (P5.c) the engine-side filter retires.
pub(crate) trait ToolDispatcher: Send + Sync {
    /// All tool definitions registered with this dispatcher. The engine
    /// applies permission filtering externally.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// True when the named tool exists in this dispatcher.
    fn contains(&self, name: &str) -> bool;

    /// True when the named tool routes through the `mcp` permission
    /// ruleset rather than the per-tool `tools` ruleset.
    fn is_mcp(&self, name: &str) -> bool;

    /// Per-call permission hooks. `None` means the tool is unknown.
    fn evaluate_hooks(&self, name: &str, args: &HashMap<String, Value>) -> Option<ToolHooks>;

    /// Dispatch a tool call. `None` means the tool is unknown; the
    /// engine handles that case by emitting a synthetic error result.
    fn dispatch<'a>(
        &'a self,
        name: &str,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext,
    ) -> Option<ToolFuture<'a>>;
}

impl ToolDispatcher for ToolRegistry {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|e| {
                ToolDefinition::new(FunctionSchema {
                    name: e.tool.name().into(),
                    description: e.tool.description().into(),
                    parameters: e.tool.parameters(),
                })
            })
            .collect()
    }

    fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    fn is_mcp(&self, name: &str) -> bool {
        self.get(name).is_some_and(|e| e.is_mcp)
    }

    fn evaluate_hooks(&self, name: &str, args: &HashMap<String, Value>) -> Option<ToolHooks> {
        self.get(name).map(|e| e.tool.evaluate_hooks(args))
    }

    fn dispatch<'a>(
        &'a self,
        name: &str,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext,
    ) -> Option<ToolFuture<'a>> {
        self.get(name).map(|e| e.tool.execute(args, ctx))
    }
}

#[derive(Default)]
pub(crate) struct ToolRegistry {
    tools: Vec<ToolEntry>,
}

impl ToolRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn register_mcp(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(ToolEntry { tool, is_mcp: true });
    }

    pub(crate) fn get(&self, name: &str) -> Option<&ToolEntry> {
        self.tools.iter().find(|e| e.tool.name() == name)
    }
}

pub(crate) fn str_arg(args: &HashMap<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn tool_arg_summary(tool_name: &str, args: &HashMap<String, Value>) -> String {
    match tool_name {
        "bash" => str_arg(args, "command"),
        "read_file" | "write_file" | "edit_file" => display_path(&str_arg(args, "file_path")),
        "edit_notebook" => display_path(&str_arg(args, "notebook_path")),
        "glob" | "grep" => {
            confirm_with_optional_path(str_arg(args, "pattern"), &str_arg(args, "path"))
                .unwrap_or_default()
        }
        "web_fetch" => str_arg(args, "url"),
        "web_search" => str_arg(args, "query"),
        "exit_plan_mode" => "plan ready".into(),
        "read_process_output" | "stop_process" => str_arg(args, "id"),
        "ask_user_question" => {
            let count = args
                .get("questions")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("{} question{}", count, if count == 1 { "" } else { "s" })
        }
        "load_skill" => str_arg(args, "name"),
        _ => String::new(),
    }
}

/// Convert an absolute path to a relative one if it's inside the cwd.
pub fn display_path(path: &str) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        let prefix = cwd.to_string_lossy();
        if let Some(rest) = path.strip_prefix(prefix.as_ref()) {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                return ".".into();
            }
            return rest.into();
        }
    }
    path.into()
}

/// Build a confirm label like `"pattern"` or `"pattern in dir"`, omitting the
/// path when it is the cwd.
pub(crate) fn confirm_with_optional_path(label: String, path: &str) -> Option<String> {
    if path.is_empty() || path == "." {
        Some(label)
    } else {
        Some(format!("{} in {}", label, display_path(path)))
    }
}

/// Maximum lines of tool output sent to the LLM. Individual tools may
/// enforce their own (often larger) limits before this; this is the final
/// trim applied when building the API request.
pub(crate) const MAX_TOOL_OUTPUT_LINES: usize = 2000;

/// Trim tool output to `max_lines` for LLM context. Appends a note with
/// the total line count when truncated.
pub(crate) fn trim_tool_output(content: &str, max_lines: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    let total = content.lines().count();
    if total <= max_lines {
        return content.to_string();
    }
    let mut out: String = content
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str(&format!("\n... (trimmed, {} lines total)", total));
    out
}

/// Acquire an exclusive, non-blocking advisory lock on the given file path.
/// Returns `Ok(guard)` on success. Returns `Err(message)` if the file is
/// locked by another process (EWOULDBLOCK) or on any other I/O error.
/// The lock is released when the guard is dropped.
#[cfg(unix)]
pub fn try_flock(path: &str) -> Result<FlockGuard, String> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err("File is currently being edited by another agent, try again later.".into());
        }
        return Err(format!("flock error: {err}"));
    }
    Ok(FlockGuard { _file: file })
}

#[cfg(not(unix))]
pub fn try_flock(_path: &str) -> Result<FlockGuard, String> {
    Ok(FlockGuard { _file: None })
}

pub struct FlockGuard {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(not(unix))]
    _file: Option<()>,
}

pub(crate) fn build_tools() -> ToolRegistry {
    ToolRegistry::new()
}
