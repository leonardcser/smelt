//! Filesystem watcher registry shared between `LuaShared` and the
//! `smelt.fs.watch` and `smelt.git.watch` Lua APIs. Kept neutral of the api layer so
//! `shared.rs` doesn't need to depend on `crate::lua::api::*`.

use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Per-watcher mutable state shared between the notify thread and the
/// Lua-driven arm/stop calls.
#[derive(Default)]
pub struct WatcherState {
    /// Events queued since the last drain. Drained whole on each arm.
    pub pending: Vec<serde_json::Value>,
    /// External task id waiting for the next event batch. The notify
    /// thread takes this and resolves it as soon as one event arrives.
    pub armed: Option<u64>,
    pub closed: bool,
}

/// What a `WatcherEntry` observes.
pub enum WatchTarget {
    Path { path: PathBuf, mode: RecursiveMode },
    Repository(crate::git_watch::Repository),
}

enum Subscription {
    Path {
        _watcher: RecommendedWatcher,
    },
    Repository {
        _watcher: crate::git_watch::RepositoryWatcher,
    },
}

/// Lives in `LuaShared::watchers`. Candidate entries retain only desired
/// subscription values until generation commit. Dropping an active entry tears
/// down its OS subscription.
pub struct WatcherEntry {
    pub state: Arc<Mutex<WatcherState>>,
    target: WatchTarget,
    sink: crate::lua::LuaResumeSink,
    /// Held only for its drop, which ends the OS subscription.
    subscription: Option<Subscription>,
}

impl WatcherEntry {
    pub fn new(
        target: WatchTarget,
        sink: crate::lua::LuaResumeSink,
        active: bool,
    ) -> Result<Self, String> {
        let mut entry = Self {
            state: Arc::new(Mutex::new(WatcherState::default())),
            target,
            sink,
            subscription: None,
        };
        if active {
            entry.activate()?;
        }
        Ok(entry)
    }

    pub fn activate(&mut self) -> Result<(), String> {
        if self.subscription.is_some() {
            return Ok(());
        }
        let state = Arc::clone(&self.state);
        let sink = self.sink.clone();
        let coalesce = matches!(self.target, WatchTarget::Repository(_));
        let deliver = move |event: &notify::Event| deliver(&state, &sink, event, coalesce);
        self.subscription = Some(match &self.target {
            WatchTarget::Path { path, mode } => {
                let mut watcher = RecommendedWatcher::new(
                    move |result: notify::Result<notify::Event>| {
                        if let Ok(event) = result {
                            deliver(&event);
                        }
                    },
                    Config::default(),
                )
                .map_err(|error| error.to_string())?;
                watcher
                    .watch(path, *mode)
                    .map_err(|error| error.to_string())?;
                Subscription::Path { _watcher: watcher }
            }
            WatchTarget::Repository(repository) => Subscription::Repository {
                _watcher: crate::git_watch::watch(repository.clone(), move || {
                    deliver(&notify::Event::new(notify::EventKind::Any));
                })?,
            },
        });
        Ok(())
    }
}

/// Queue `event` and resolve the armed task, if any, with everything queued.
fn deliver(
    state: &Mutex<WatcherState>,
    sink: &crate::lua::LuaResumeSink,
    event: &notify::Event,
    coalesce: bool,
) {
    let payload = event_to_json(event);
    let resume = {
        let Ok(mut state) = state.lock() else {
            return;
        };
        if state.closed {
            return;
        }
        if coalesce {
            state.pending.clear();
        }
        state.pending.push(payload);
        state
            .armed
            .take()
            .map(|task_id| (task_id, std::mem::take(&mut state.pending)))
    };
    if let Some((task_id, events)) = resume {
        sink.resolve_json(task_id, serde_json::Value::Array(events));
    }
}

fn event_to_json(event: &notify::Event) -> serde_json::Value {
    use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind, RenameMode};
    let (kind, detail) = match event.kind {
        notify::EventKind::Create(kind) => (
            "create",
            Some(match kind {
                CreateKind::File => "file",
                CreateKind::Folder => "folder",
                CreateKind::Any => "any",
                CreateKind::Other => "other",
            }),
        ),
        notify::EventKind::Modify(kind) => match kind {
            ModifyKind::Name(RenameMode::From) => ("rename", Some("from")),
            ModifyKind::Name(RenameMode::To) => ("rename", Some("to")),
            ModifyKind::Name(RenameMode::Both) => ("rename", Some("both")),
            ModifyKind::Name(_) => ("rename", None),
            ModifyKind::Data(_) => ("modify", Some("data")),
            ModifyKind::Metadata(_) => ("modify", Some("metadata")),
            ModifyKind::Any => ("modify", Some("any")),
            ModifyKind::Other => ("modify", Some("other")),
        },
        notify::EventKind::Remove(kind) => (
            "remove",
            Some(match kind {
                RemoveKind::File => "file",
                RemoveKind::Folder => "folder",
                RemoveKind::Any => "any",
                RemoveKind::Other => "other",
            }),
        ),
        notify::EventKind::Access(kind) => (
            "access",
            Some(match kind {
                AccessKind::Open(_) => "open",
                AccessKind::Close(AccessMode::Write) => "close_write",
                AccessKind::Close(_) => "close",
                AccessKind::Read => "read",
                AccessKind::Any => "any",
                AccessKind::Other => "other",
            }),
        ),
        notify::EventKind::Other => ("other", None),
        notify::EventKind::Any => ("any", None),
    };
    let paths = event
        .paths
        .iter()
        .map(|path| serde_json::Value::String(path.to_string_lossy().into_owned()))
        .collect();
    let mut object = serde_json::Map::new();
    object.insert("kind".into(), serde_json::Value::String(kind.into()));
    if let Some(detail) = detail {
        object.insert("detail".into(), serde_json::Value::String(detail.into()));
    }
    object.insert("paths".into(), serde_json::Value::Array(paths));
    serde_json::Value::Object(object)
}
