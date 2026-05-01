use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single agent's entry in the on-disk registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// Human-readable ID like "agent-1". Used by the model to address agents.
    pub agent_id: String,
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub git_root: Option<String>,
    pub git_branch: Option<String>,
    pub cwd: String,
    pub status: AgentStatus,
    pub task_slug: Option<String>,
    pub session_id: String,
    pub socket_path: String,
    pub depth: u8,
    pub started_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Working,
    Idle,
}

fn registry_dir() -> PathBuf {
    crate::paths::state_dir().join("registry")
}

fn entry_path(pid: u32) -> PathBuf {
    registry_dir().join(format!("{pid}.json"))
}

/// Read and parse all valid registry entries from disk.
fn iter_entries() -> Vec<(PathBuf, RegistryEntry)> {
    let dir = registry_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut result = vec![];
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let data = match std::fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Ok(reg) = serde_json::from_str::<RegistryEntry>(&data) {
            result.push((path, reg));
        }
    }
    result
}

/// Deregister this agent and clean up its socket. Does NOT send SIGTERM
/// (use `kill_agent` for that). Intended for self-cleanup on exit.
pub fn cleanup_self(pid: u32) {
    kill_descendants(pid);
    deregister(pid);
    crate::socket::cleanup_socket(pid);
}

const AGENT_NAMES: &[&str] = &[
    "amber", "birch", "blaze", "bloom", "bolt", "brook", "briar", "cedar", "cliff", "cloud",
    "coral", "crane", "dusk", "ember", "fern", "finch", "flint", "frost", "gale", "grove", "haze",
    "hedge", "holly", "iris", "ivy", "jade", "lark", "leaf", "maple", "marsh", "moss", "oak",
    "olive", "onyx", "peak", "pine", "plum", "pond", "rain", "reed", "ridge", "sage", "shade",
    "slate", "snow", "spark", "stone", "swift", "thorn", "wren",
];

/// Generate a unique agent name. Picks from a pool of short words, falling
/// back to `word-{hash}` if all are taken.
pub fn next_agent_id() -> String {
    let used: std::collections::HashSet<String> = iter_entries()
        .into_iter()
        .map(|(_, e)| e.agent_id)
        .collect();
    // Pick randomly from available names.
    let available: Vec<&&str> = AGENT_NAMES.iter().filter(|n| !used.contains(**n)).collect();
    if !available.is_empty() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let idx = (ts as usize ^ std::process::id() as usize) % available.len();
        return available[idx].to_string();
    }
    // All names taken — append a short hash.
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{}-{:x}", AGENT_NAMES[0], (pid as u128 ^ ts) & 0xffff)
}

/// Find an agent by its human-readable ID (e.g. "agent-1").
pub fn find_by_id(agent_id: &str) -> Option<RegistryEntry> {
    iter_entries()
        .into_iter()
        .map(|(_, e)| e)
        .find(|e| e.agent_id == agent_id && is_pid_alive(e.pid))
}

/// Write this agent's entry to the registry.
pub fn register(entry: &RegistryEntry) -> std::io::Result<()> {
    let dir = registry_dir();
    std::fs::create_dir_all(&dir)?;
    let path = entry_path(entry.pid);
    let json = serde_json::to_string_pretty(entry).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// Remove this agent's entry from the registry.
fn deregister(pid: u32) {
    let _ = std::fs::remove_file(entry_path(pid));
}

/// Update the status field for this agent.
pub fn update_status(pid: u32, status: AgentStatus) {
    if let Ok(mut entry) = read_entry(pid) {
        entry.status = status;
        let _ = register(&entry);
    }
}

/// Update the task_slug field for this agent.
pub fn update_slug(pid: u32, slug: &str) {
    if let Ok(mut entry) = read_entry(pid) {
        entry.task_slug = Some(slug.to_string());
        let _ = register(&entry);
    }
}

/// Read a single registry entry by PID.
pub fn read_entry(pid: u32) -> Result<RegistryEntry, String> {
    let path = entry_path(pid);
    let data = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str(&data).map_err(|e| e.to_string())
}

/// Check if a PID is alive.
#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    true
}

/// Discover all live agents in the same scope (git root or cwd).
/// Prunes dead entries as a side effect.
pub fn discover(scope: &str) -> Vec<RegistryEntry> {
    let mut result = vec![];
    for (path, reg) in iter_entries() {
        if !is_pid_alive(reg.pid) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let matches = reg
            .git_root
            .as_deref()
            .map(|gr| gr == scope)
            .unwrap_or(false)
            || reg.cwd == scope;
        if matches {
            result.push(reg);
        }
    }
    result
}

/// Find all direct children of a given PID.
pub fn children_of(parent_pid: u32) -> Vec<RegistryEntry> {
    iter_entries()
        .into_iter()
        .map(|(_, e)| e)
        .filter(|e| e.parent_pid == Some(parent_pid) && is_pid_alive(e.pid))
        .collect()
}

/// Prune all dead entries from the registry directory.
pub fn prune_dead() {
    for (path, reg) in iter_entries() {
        if !is_pid_alive(reg.pid) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Check if `pid` is an ancestor of `root_pid` by walking parent_pid chains.
/// Reads registry entries on each step. Returns true if the chain reaches
/// `root_pid` within 10 hops.
pub fn is_in_tree(pid: u32, root_pid: u32) -> bool {
    let mut current = pid;
    for _ in 0..10 {
        if current == root_pid {
            return true;
        }
        match read_entry(current) {
            Ok(entry) => match entry.parent_pid {
                Some(parent) => current = parent,
                None => break,
            },
            Err(_) => break,
        }
    }
    false
}

/// Kill an agent and all its descendants, clean up registry, socket, and logs.
#[cfg(unix)]
pub fn kill_agent(pid: u32) {
    kill_descendants(pid);
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    deregister(pid);
    crate::socket::cleanup_socket(pid);
}

#[cfg(not(unix))]
pub fn kill_agent(pid: u32) {
    deregister(pid);
    crate::socket::cleanup_socket(pid);
}

/// Read the last N lines of an agent's log file.
pub fn read_agent_logs(session_dir: &std::path::Path, pid: u32, max_lines: usize) -> Vec<String> {
    let path = session_dir.join("agent_logs").join(format!("{pid}.log"));
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let lines: Vec<String> = content.lines().map(String::from).collect();
            let start = lines.len().saturating_sub(max_lines);
            lines[start..].to_vec()
        }
        Err(_) => vec![],
    }
}

/// Send SIGTERM to all descendants of a given PID (recursive).
#[cfg(unix)]
pub fn kill_descendants(pid: u32) {
    for child in children_of(pid) {
        kill_descendants(child.pid);
        unsafe {
            libc::kill(child.pid as i32, libc::SIGTERM);
        }
        deregister(child.pid);
    }
}

#[cfg(not(unix))]
pub fn kill_descendants(_pid: u32) {}
