//! Filesystem-driven `/reload` trigger. Watches Lua config inputs
//! (init.lua, plugins, commands, tools, completers, dialogs, runtime
//! overrides) and pushes a debounced wake-up signal into the TUI run loop,
//! which then re-enters [`crate::app::TuiApp::reload_lua`].
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

/// Roots the auto-reloader watches. This intentionally covers Lua config
/// development only; prompt inputs (`AGENTS.md`, `SKILL.md`, and
/// `--system-prompt`) are refreshed by manual `/reload` so instruction
/// changes remain explicit.
pub struct WatchPaths {
    /// `~/.config/smelt/` - recursive.
    pub global_config: Option<PathBuf>,
    /// `.smelt/` under cwd - recursive when present. When missing, its
    /// parent is watched recursively and events are filtered back to `.smelt/`
    /// so first-time project config starts hot-reloading without a restart.
    pub project_config: Option<PathBuf>,
}

impl WatchPaths {
    /// Resolve every watchable root for the running session. Paths that
    /// don't exist on disk are still included - `notify` errors are
    /// logged and skipped per-path so a missing optional root never
    /// disables the entire watcher.
    pub fn discover(cwd: &Path) -> Self {
        Self {
            global_config: Some(smelt_core::config::config_dir()),
            project_config: Some(cwd.join(".smelt")),
        }
    }

    fn relevant_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(p) = self.global_config.as_ref() {
            roots.push(p.clone());
        }
        if let Some(p) = self.project_config.as_ref() {
            roots.push(p.clone());
        }
        roots
    }
}

/// Spawn the watcher and debouncer. Returns the drop guard plus a
/// receiver that yields `()` each time a debounced filesystem change
/// warrants a reload.
pub fn spawn(paths: WatchPaths) -> Option<(AutoReloadHandle, UnboundedReceiver<()>)> {
    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (signal_tx, signal_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let relevant_roots = paths.relevant_roots();

    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if !relevant(&event, &relevant_roots) {
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
        } else if let Some(parent) = p.parent() {
            if try_watch(&mut watcher, parent, RecursiveMode::Recursive) {
                subscribed_any = true;
            }
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

/// Keep only events that can plausibly change Lua config behavior. Drops
/// metadata-only events (atime bumps from `cat`, permission probes from
/// editors) and ignores prompt inputs (`AGENTS.md`, `SKILL.md`, markdown
/// commands, system-prompt files) so instruction changes remain explicit
/// through manual `/reload`.
fn relevant(event: &notify::Event, roots: &[PathBuf]) -> bool {
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
    event.paths.iter().any(|p| relevant_path(p, roots))
}

fn relevant_path(path: &Path, roots: &[PathBuf]) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("lua")
        && roots.iter().any(|root| path.starts_with(root))
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

    #[tokio::test]
    async fn watcher_does_not_replay_changes_from_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.lua");
        std::fs::write(&init, "before = true\n").unwrap();

        let (_handle, mut rx) = spawn(WatchPaths {
            global_config: Some(dir.path().to_path_buf()),
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
        let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel();
        let (signal_tx, mut signal_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            debounce_loop(&mut raw_rx, signal_tx).await;
        });

        raw_tx.send(()).unwrap();
        raw_tx.send(()).unwrap();
        assert!(tokio::time::timeout(DEBOUNCE / 2, signal_rx.recv())
            .await
            .is_err());
        assert!(tokio::time::timeout(DEBOUNCE * 2, signal_rx.recv())
            .await
            .unwrap()
            .is_some());
        assert!(signal_rx.try_recv().is_err());

        drop(raw_tx);
        task.await.unwrap();
    }
}
