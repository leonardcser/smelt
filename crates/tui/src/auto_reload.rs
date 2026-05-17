//! Filesystem-driven `/reload` trigger. Watches the on-disk inputs the
//! agent depends on (init.lua, plugins, commands, skills, AGENTS.md, the
//! `--system-prompt` file) and pushes a debounced wake-up signal into
//! the TUI run loop, which then re-enters [`crate::app::TuiApp::reload_lua`].
//!
//! Owned by `TuiApp` so the watcher stays alive for the session and the
//! drop guard tears down the `notify::RecommendedWatcher` on shutdown.

use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Debounce window: events arriving inside this interval are coalesced
/// into a single reload signal. Picked to swallow editor "save → swap
/// tempfile in place" bursts (vim's `:w` typically fires 2–4 events).
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Handle returned by [`spawn`]. Owns the `notify` watcher; dropping it
/// stops the OS subscription. The accompanying signal task exits when
/// the watcher channel closes.
pub struct AutoReloadHandle {
    _watcher: RecommendedWatcher,
}

/// Roots the auto-reloader watches. Mirrors the actual loader code paths
/// in `crates/core/src/lua/runtime.rs` (init.lua + plugins) and
/// `crates/tui/src/instructions.rs` (AGENTS.md walk).
pub struct WatchPaths {
    /// `~/.config/smelt/` — recursive.
    pub global_config: Option<PathBuf>,
    /// `.smelt/` under cwd — recursive. Optional even when the
    /// directory does not yet exist: the watcher silently skips
    /// missing paths so first-time project setup still wires up after
    /// a `/reload`.
    pub project_config: Option<PathBuf>,
    /// AGENTS.md files anywhere from cwd up to filesystem root,
    /// plus the global `~/.config/smelt/AGENTS.md` if present.
    pub agents_md: Vec<PathBuf>,
    /// `--system-prompt <path>` when the flag pointed at a file.
    pub system_prompt: Option<PathBuf>,
    /// Out-of-tree skill roots from `cfg.skills.paths`. Watched
    /// recursively so SKILL.md edits trigger reload even when they
    /// live outside the global/project configs.
    pub extra_skill_dirs: Vec<PathBuf>,
}

impl WatchPaths {
    /// Resolve every watchable root for the running session. Paths that
    /// don't exist on disk are still included — `notify` errors are
    /// logged and skipped per-path so a missing optional root never
    /// disables the entire watcher.
    pub fn discover(cwd: &Path, skill_extra: &[PathBuf], system_prompt: Option<PathBuf>) -> Self {
        let global_config = Some(smelt_core::config::config_dir());
        let project_config = Some(cwd.join(".smelt"));

        let mut agents = Vec::new();
        let global_agents = smelt_core::config::config_dir().join("AGENTS.md");
        agents.push(global_agents);
        let mut dir: Option<&Path> = Some(cwd);
        while let Some(d) = dir {
            agents.push(d.join("AGENTS.md"));
            dir = d.parent();
        }
        // Extra skill paths from `cfg.skills.paths` aren't covered by
        // the global/project roots — watch them explicitly so out-of-tree
        // SKILL.md edits also trigger reload. Strip entries already
        // inside the global/project roots to avoid redundant subscriptions.
        let mut extra_skill_dirs: Vec<PathBuf> = skill_extra.to_vec();
        if let Some(g) = global_config.as_ref() {
            extra_skill_dirs.retain(|p| !p.starts_with(g));
        }
        if let Some(p) = project_config.as_ref() {
            extra_skill_dirs.retain(|q| !q.starts_with(p));
        }

        Self {
            global_config,
            project_config,
            agents_md: agents,
            system_prompt,
            extra_skill_dirs,
        }
    }
}

/// Spawn the watcher and debouncer. Returns the drop guard plus a
/// receiver that yields `()` each time a debounced filesystem change
/// warrants a reload.
pub fn spawn(paths: WatchPaths) -> Option<(AutoReloadHandle, UnboundedReceiver<()>)> {
    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !relevant(&event) {
                return;
            }
            let _ = raw_tx.send(());
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
    if let Some(p) = paths.global_config.as_ref() {
        if try_watch(&mut watcher, p, RecursiveMode::Recursive) {
            subscribed_any = true;
        }
    }
    if let Some(p) = paths.project_config.as_ref() {
        if try_watch(&mut watcher, p, RecursiveMode::Recursive) {
            subscribed_any = true;
        }
    }
    for p in &paths.agents_md {
        if try_watch(&mut watcher, p, RecursiveMode::NonRecursive) {
            subscribed_any = true;
        }
    }
    if let Some(p) = paths.system_prompt.as_ref() {
        if try_watch(&mut watcher, p, RecursiveMode::NonRecursive) {
            subscribed_any = true;
        }
    }
    for p in &paths.extra_skill_dirs {
        if try_watch(&mut watcher, p, RecursiveMode::Recursive) {
            subscribed_any = true;
        }
    }

    if !subscribed_any {
        return None;
    }

    tokio::spawn(async move {
        debounce_loop(&mut raw_rx, signal_tx).await;
    });

    Some((AutoReloadHandle { _watcher: watcher }, signal_rx))
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

/// Keep only events that can plausibly change agent behaviour. Drops
/// metadata-only events (atime bumps from `cat`, permission probes from
/// editors) and anything outside `.lua`/`.md`/`AGENTS.md`. Directory
/// events pass through so adding/removing whole plugin or skill
/// directories still fires.
fn relevant(event: &notify::Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    // Modify(Metadata) is just chmod/touch; ignore.
    if let EventKind::Modify(notify::event::ModifyKind::Metadata(_)) = event.kind {
        return false;
    }
    if event.paths.is_empty() {
        // Rescan events carry no paths; treat as relevant.
        return true;
    }
    event.paths.iter().any(|p| relevant_path(p))
}

fn relevant_path(path: &Path) -> bool {
    if path.is_dir() {
        return true;
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name == "AGENTS.md" || name == "SKILL.md" {
        return true;
    }
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("lua") | Some("md")
    )
}

/// Coalesce a burst of raw events into a single signal. After the first
/// event arrives, wait for [`DEBOUNCE`] of silence before forwarding;
/// any event landing inside the window resets the timer.
async fn debounce_loop(raw_rx: &mut UnboundedReceiver<()>, signal_tx: UnboundedSender<()>) {
    loop {
        if raw_rx.recv().await.is_none() {
            return;
        }
        loop {
            match tokio::time::timeout(DEBOUNCE, raw_rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return,
                Err(_) => break,
            }
        }
        if signal_tx.send(()).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn relevant_path_accepts_lua_and_md() {
        assert!(relevant_path(Path::new("plugins/foo.lua")));
        assert!(relevant_path(Path::new("commands/bar.md")));
    }

    #[test]
    fn relevant_path_accepts_named_marker_files() {
        assert!(relevant_path(Path::new("/proj/AGENTS.md")));
        assert!(relevant_path(Path::new("skills/x/SKILL.md")));
    }

    #[test]
    fn relevant_path_rejects_unrelated_extensions() {
        assert!(!relevant_path(Path::new("foo.txt")));
        assert!(!relevant_path(Path::new("notes.json")));
    }
}
