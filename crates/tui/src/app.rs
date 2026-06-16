pub(crate) mod agent;
pub(crate) mod cmdline;
pub(crate) mod cmdline_edit;
pub(crate) mod content_keys;
pub(crate) mod engine_events;
pub(crate) mod events;
#[cfg(test)]
mod harness_tests;
pub(crate) mod history;
pub(crate) mod host_dispatch;
pub(crate) mod lua_bridge;
pub(crate) mod lua_handlers;
pub(crate) mod mouse;
pub(crate) mod pane_focus;
pub(crate) mod queue;
pub(crate) mod render_loop;
pub(crate) mod reveal;
pub(crate) mod search;
#[cfg(any(test, feature = "harness"))]
pub mod test_harness;
pub(crate) mod transcript;
pub(crate) mod transcript_search;
pub(crate) mod ui_host;
pub(crate) mod well_known;

use crate::input::PromptState;
use engine::EngineHandle;
use protocol::Content;
use smelt_core::history::History;
use smelt_core::session::Session;
use smelt_core::transcript_model::Block;
use smelt_core::ConfirmRequest;
use smelt_core::FrontendKind;
use std::sync::Arc;

use crossterm::{event, terminal};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) struct ContextWindowUpdate {
    pub(crate) request_id: u64,
    pub(crate) model: String,
    pub(crate) api_base: String,
    pub(crate) provider_type: String,
    pub(crate) value: Option<u32>,
}

pub struct TuiApp {
    pub core: smelt_core::Core,
    pub lua: crate::lua::LuaRuntime,
    pub(crate) transcript: crate::app::transcript::TranscriptView,
    pub(crate) parser: smelt_core::content::stream_parser::StreamParser,
    pub(crate) resume_preview_cache: crate::app::transcript::ResumePreviewCache,
    pub(crate) input_history: History,
    pub(crate) input: PromptState,
    pub(crate) exec: Option<crate::commands::ExecHandle>,
    /// Wakeup from cross-thread tasks that pushed to the Lua inbox. Drains the inbox so parked coroutines resume.
    lua_wakeup_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    /// Host-callback receiver from the engine task. Lives next to the
    /// engine's event receiver but is moved out at construction time so
    /// the two can be polled in the same `tokio::select!`.
    pub(crate) host_rx: tokio::sync::mpsc::UnboundedReceiver<engine::HostCall>,
    pub(crate) queued_inputs: InputQueues,
    /// Current working directory (cached at startup).
    pub(crate) cwd: String,
    pub(crate) shared_session: Arc<Mutex<Option<Session>>>,
    pub(crate) task_label: Option<String>,
    pub(crate) pending_dialog: bool,
    pub(crate) pending_quit: bool,
    pub(crate) notification: Option<Notification>,
    pub(crate) cmdline: crate::app::cmdline::CmdlineState,
    pub(crate) search: crate::app::search::SearchState,
    pub(crate) picker_state: HashMap<crate::smelt_edit::WinId, crate::picker::PickerState>,
    pub(crate) paint_registry: crate::lua::paint::PaintRegistry,
    /// Drives cursor suppression when unfocused so input from another app
    /// doesn't draw a stale cursor in our window.
    pub(crate) term_focused: bool,
    pub(crate) working: smelt_core::working::WorkingState,
    /// Viewport layout updated each frame; read by mouse hit-testing and scroll estimation.
    pub(crate) layout: crate::content::layout::LayoutState,

    /// Owned here so reducer handlers (`apply_ops`) can mutate it directly
    /// instead of threading `&mut Option<TurnState>` through every call.
    pub(crate) agent: Option<TurnState>,
    pub(crate) inspect_server: Option<crate::inspect_server::Server>,
    pub(crate) sleep_inhibit: crate::sleep_inhibit::SleepInhibitor,
    pub(crate) persister: crate::persist::Persister,
    pub(crate) session_save_pending: bool,
    pub(crate) persisted_fingerprints: Option<PersistFingerprints>,
    pub(crate) session_dirty: bool,
    pub(crate) persisted_display_cache_generation: u64,
    pub(crate) last_width: u16,
    pub(crate) last_height: u16,
    pub(crate) next_turn_id: u64,
    pub(crate) pending_turn_meta: Option<protocol::TurnMeta>,
    pub(crate) pending_history_appends: Vec<PendingHistoryAppend>,
    process_completion_rx:
        tokio::sync::mpsc::UnboundedReceiver<smelt_core::process::ProcessCompletion>,
    pub(crate) context_tokens_updated_this_turn: bool,
    pub(crate) cancel_generation: u64,
    /// Set while routing an engine event whose `TurnState` has been moved
    /// out of `self.agent` to satisfy borrowing. Lua callbacks can still
    /// observe the active turn through `active_agent_turn_id`.
    pub(crate) dispatching_turn_id: Option<u64>,
    /// `smelt.work.busy` token stack. Non-empty → prompt top-bar
    /// indicator animates with the top token's label.
    pub(crate) busy_stack: BusyStack,
    startup_auth_error: Option<String>,
    /// Trust state for `<cwd>/.smelt/`; surfaced as a startup toast then dropped.
    pub(crate) project_trust: Option<smelt_core::trust::TrustState>,
    pub(crate) app_focus: AppFocus,
    /// Tracks the last text dispatched as `TextChanged` on `PROMPT_WIN`.
    pub(crate) last_prompt_text: String,
    /// On-disk inputs that feed the agent's system prompt. Single
    /// home for `AGENTS.md`, the [`engine::SkillLoader`] section, and
    /// the `--system-prompt` file content; refreshed in place by
    /// `/reload`.
    pub prompt_inputs: crate::prompt_inputs::PromptInputs,
    /// Drop guard for the auto-reload filesystem watcher. `None` when
    /// the watcher is disabled (`settings.auto_reload = false`) or when
    /// `notify` failed to subscribe to any of the configured roots.
    pub(crate) auto_reload: Option<crate::auto_reload::AutoReloadHandle>,
    /// Config reload requested from a busy context. Drained once the app is idle
    /// enough that wiping Lua callbacks cannot strand an active turn or modal.
    pub(crate) pending_lua_reload: bool,
    pub(crate) prompt_sections: crate::prompt_sections::PromptSections,
    pub ui: crate::smelt_edit::Ui,
    pub(crate) well_known: WellKnown,
    /// Timers + chord state observed and updated by event dispatch.
    pub(crate) timers: Timers,
    /// Confirm/dialog requests deferred while the user is still typing.
    pub(crate) pending_dialogs: VecDeque<DeferredDialog>,
    /// Owned for the lifetime of `run()`. `None` outside that scope - the
    /// test harness constructs a `TuiApp` without a real terminal and skips
    /// claiming. `Drop` here restores the terminal even on panic.
    pub(crate) terminal: Option<crate::term_setup::TuiTerminal>,
    /// Shared HTTP client used for background side-fetches (context window).
    /// `None` in the test harness.
    pub(crate) http_client: Option<engine::HttpClient>,
    /// Sender into the channel `run()` drains for context-window updates.
    /// `apply_model` spawns a fetch task that pushes the result here so the
    /// UI footer reflects the new model immediately.
    pub(crate) context_window_tx: Option<tokio::sync::mpsc::UnboundedSender<ContextWindowUpdate>>,
    /// Monotonic request id for context-window side fetches.
    pub(crate) context_window_request_id: u64,
    /// Current prompt input viewport height in rows after applying auto-wrap,
    /// manual resize, and terminal clamps. Updated during layout each frame.
    pub(crate) prompt_input_rows: u16,
    /// User-resized prompt input height. `None` means follow the auto-measured
    /// wrapped source/ghost-text height.
    pub(crate) prompt_input_rows_override: Option<u16>,
    /// In-flight drag from non-selectable prompt top chrome.
    pub(crate) prompt_resize_drag: Option<PromptResizeDrag>,
    /// Last prompt resize-handle click, used to reset manual height on double-click.
    pub(crate) prompt_resize_last_click: Option<PromptResizeClick>,
    /// Parser-visible prompt placeholder. `placeholders` owns the app-level text;
    /// this mirror lets the prompt parser render it as wrapped ghost text.
    pub(crate) prompt_placeholder_display: Arc<Mutex<Option<String>>>,
    /// Per-window placeholder text set through the app/Lua API.
    pub(crate) placeholders: HashMap<crate::smelt_edit::WinId, String>,
    /// Per-window placeholder dispatch options. Static placeholders may have text
    /// without dispatch opts; entries here are the interactive subset.
    pub placeholder_opts: HashMap<crate::smelt_edit::WinId, PlaceholderOpts>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PromptResizeDrag {
    pub(crate) start_row: u16,
    pub(crate) start_input_rows: u16,
    pub(crate) dragged: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PromptResizeClick {
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) at: Instant,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Notification {
    pub(crate) win: crate::smelt_edit::WinId,
    pub(crate) lifetime: NotificationLifetime,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum NotificationLifetime {
    Timed { expires_at: Instant },
    Sticky,
}

impl NotificationLifetime {
    fn timed(now: Instant) -> Self {
        Self::Timed {
            expires_at: now + Duration::from_millis(NOTIFICATION_TTL_MS),
        }
    }

    fn is_sticky(self) -> bool {
        matches!(self, Self::Sticky)
    }

    fn is_expired(self, now: Instant) -> bool {
        match self {
            Self::Timed { expires_at } => expires_at <= now,
            Self::Sticky => false,
        }
    }

    fn expiry_delay(self, now: Instant) -> Option<Duration> {
        match self {
            Self::Timed { expires_at } => Some(expires_at.saturating_duration_since(now)),
            Self::Sticky => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptWorkState {
    Idle,
    BackgroundBusy,
    TurnActive,
}

impl PromptWorkState {
    pub(crate) fn is_busy(self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub(crate) fn turn_is_active(self) -> bool {
        matches!(self, Self::TurnActive)
    }
}

/// Per-window dispatch policy for a placeholder. Set via Lua's
/// `Win:placeholder(text, opts)`; the dispatcher consults it on key events.
#[derive(Default, Clone)]
pub struct PlaceholderOpts {
    pub accept_keys: Vec<crate::smelt_edit::KeyBind>,
    pub dismiss_keys: Vec<crate::smelt_edit::KeyBind>,
}

#[cfg(any(test, feature = "harness"))]
pub(crate) use queue::MAX_QUEUED_MESSAGES;
pub(crate) use queue::{InputQueues, QueueStage, QueuedInput, QueuedTurnOptions};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PersistFingerprints {
    pub(crate) session: Vec<u8>,
    pub(crate) display_cache: Vec<u8>,
}

pub use well_known::{PROMPT_EDIT_BUF, PROMPT_WIN, TRANSCRIPT_WIN};

/// Stack of live `smelt.work.busy` tokens. Each `push` returns a
/// monotonic id consumed by `release`; the prompt top-bar indicator
/// animates with the most recently pushed token's label. The `since`
/// anchor marks when the stack first became non-empty so the spinner
/// glyph can advance even when no agent turn is live.
#[derive(Default)]
pub(crate) struct BusyStack {
    entries: Vec<(u64, String)>,
    next_id: u64,
    since: Option<Instant>,
}

pub use smelt_core::cells::WorkBusyEntry;

impl BusyStack {
    pub(crate) fn push(&mut self, label: String) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        if self.entries.is_empty() {
            self.since = Some(Instant::now());
        }
        self.entries.push((id, label));
        id
    }

    /// Drop the entry with `id`. Returns `true` if an entry was removed.
    pub(crate) fn release(&mut self, id: u64) -> bool {
        if let Some(pos) = self.entries.iter().position(|(i, _)| *i == id) {
            self.entries.remove(pos);
            if self.entries.is_empty() {
                self.since = None;
            }
            true
        } else {
            false
        }
    }

    pub(crate) fn is_busy(&self) -> bool {
        !self.entries.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.since = None;
    }

    pub(crate) fn top_label(&self) -> Option<String> {
        self.entries.last().map(|(_, l)| l.clone())
    }

    /// Elapsed time since the first token was pushed, or `None` when empty.
    pub(crate) fn elapsed(&self) -> Option<std::time::Duration> {
        self.since.map(|t| t.elapsed())
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn since(&self) -> Option<Instant> {
        self.since
    }

    /// Full stack newest-last, projected onto `WorkBusyEntry`. Cheap
    /// clone of the per-entry `(id, label)` pair; called once per tick
    /// by the cell publisher.
    pub(crate) fn entries_snapshot(&self) -> Vec<WorkBusyEntry> {
        self.entries
            .iter()
            .map(|(id, label)| WorkBusyEntry {
                id: *id,
                label: label.clone(),
            })
            .collect()
    }
}

/// Well-known stable `WinId`s for the always-present split-tree windows.
/// Prompt input and the transcript stay engine-owned; the prompt bars and
/// statusline are Lua-allocated by `runtime/lua/smelt/prompt_bar.lua` and
/// `runtime/lua/smelt/statusline.lua`.
pub(crate) struct WellKnown {
    pub(crate) prompt: crate::smelt_edit::WinId,
    pub(crate) transcript: crate::smelt_edit::WinId,
    pub(crate) cmdline: Option<crate::smelt_edit::WinId>,
}

/// Which pane currently holds focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppFocus {
    Prompt,
    Content,
}

pub(crate) struct TurnState {
    pub(crate) turn_id: u64,
    pub(crate) pending: Vec<PendingTool>,
    pub(crate) permissions: std::sync::Arc<smelt_core::permissions::Permissions>,
    pub(crate) _perf: Option<smelt_perf::perf::Guard>,
}

pub(crate) enum EventOutcome {
    Noop,
    Redraw,
    Quit,
    CancelAgent,
    /// Cancel the running agent and immediately start a new turn with the next queued item.
    InterruptWithQueued,
    /// Start a new turn without adding prompt text.
    ContinueTurn,
    Submit {
        content: Content,
        display: String,
    },
    Exec(crate::commands::ExecHandle),
}

pub(crate) enum CommandAction {
    Continue,
    Exec(crate::commands::ExecHandle),
}

pub(crate) enum InputOutcome {
    Continue,
    StartAgent,
    Exec(crate::commands::ExecHandle),
}

/// Mutable timer state shared across event handlers.
pub(crate) struct Timers {
    pub(crate) last_ctrlc: Option<Instant>,
    pub(crate) last_keypress: Option<Instant>,
    /// Pending `Ctrl-W` pane chord; next key navigates panes instead of flowing to input.
    pub(crate) pending_pane_chord: Option<Instant>,
    /// Pending transcript `z` fold chord while content pane is focused.
    /// This is a content-viewer chord because global Lua keymaps intentionally
    /// run before focus-specific transcript dispatch.
    pub(crate) pending_transcript_fold_chord: Option<Instant>,
    /// Active Lua-keymap chord sequence; `None` between chords.
    pub(crate) pending_chord: Option<PendingChord>,
}

/// State carried between keys of a multi-key chord. See [`Timers::pending_chord`].
pub(crate) struct PendingChord {
    pub(crate) tokens: Vec<String>,
    /// Vim mode captured before the first key was dispatched; surfaced to chord handlers.
    pub(crate) vim_mode_at_start: Option<crate::smelt_edit::VimMode>,
    pub(crate) policy: PendingChordPolicy,
}

#[derive(Clone, Copy)]
pub(crate) enum PendingChordPolicy {
    Sticky,
    Timed { expires_at: Instant },
}

impl PendingChordPolicy {
    fn expires_at(self) -> Option<Instant> {
        match self {
            Self::Sticky => None,
            Self::Timed { expires_at } => Some(expires_at),
        }
    }
}

/// Idle time after which a pending Esc-led Lua keymap chord expires.
pub(crate) const ESC_CHORD_TIMEOUT_MS: u64 = 500;

/// Idle time after the last keypress before showing a deferred permission dialog.
pub(crate) const CONFIRM_DEFER_MS: u64 = 1500;

/// How long a timed notification toast stays visible without user interaction.
pub(crate) const NOTIFICATION_TTL_MS: u64 = 5000;

pub(crate) enum DeferredDialog {
    Confirm(Box<ConfirmRequest>),
}

pub(crate) enum SessionControl {
    Continue,
    NeedsConfirm(Box<ConfirmRequest>),
    Done,
    Error,
}

/// How the active turn is ending. Drives whether queued inputs are preserved
/// and whether the next queued turn is auto-started.
pub(crate) enum TurnEnd {
    /// Clean completion: queue may chain into the next turn.
    Complete,
    /// User cancelled: queue is drained back to the prompt.
    Cancelled,
    /// Provider/engine error: queue is preserved so the user can retry.
    Errored,
}

pub(crate) struct PendingTool {
    pub(crate) call_id: String,
    pub(crate) name: String,
}

#[derive(Clone)]
pub(crate) struct PendingHistoryAppend {
    item: protocol::HistoryItem,
    replace_note_kind: Option<protocol::HistoryNoteKind>,
}

impl PendingHistoryAppend {
    pub(crate) fn mode_change(mode: String, text: String) -> Self {
        Self {
            item: protocol::HistoryItem::note(protocol::HistoryNote::mode_change_for_mode(
                mode, text,
            )),
            replace_note_kind: Some(protocol::HistoryNoteKind::ModeChange),
        }
    }

    pub(crate) fn process_status(note: protocol::HistoryNote) -> Self {
        Self {
            item: protocol::HistoryItem::note(note),
            replace_note_kind: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn history_item(&self) -> protocol::HistoryItem {
        self.item.clone()
    }

    pub(crate) fn transcript_block(&self, lua: &crate::lua::LuaRuntime) -> Block {
        crate::app::history::history_note_to_block(
            lua,
            self.item
                .as_note()
                .expect("pending history appends are notes"),
        )
    }

    pub(crate) fn replacement_note_kind(&self) -> Option<protocol::HistoryNoteKind> {
        self.replace_note_kind
    }

    pub(crate) fn mode(&self) -> Option<&str> {
        self.item.as_note().and_then(protocol::HistoryNote::mode)
    }

    pub(crate) fn history_append(
        &self,
        mode_base: Option<protocol::AgentMode>,
    ) -> protocol::HistoryAppend {
        match self.replace_note_kind {
            Some(protocol::HistoryNoteKind::ModeChange) => protocol::HistoryAppend::mode_change(
                self.item.clone(),
                mode_base.expect("mode history appends require a base mode"),
            ),
            Some(kind) => protocol::HistoryAppend::replace_note_kind(self.item.clone(), kind),
            None => protocol::HistoryAppend::append(self.item.clone()),
        }
    }

    pub(crate) fn matches_history_item(&self, item: &protocol::HistoryItem) -> bool {
        if &self.item == item {
            return true;
        }
        let Some(expected) = self.item.as_note() else {
            return false;
        };
        let Some(actual) = item.as_note() else {
            return false;
        };
        expected.kind() == actual.kind() && expected.text() == actual.text()
    }
}

impl TuiApp {
    pub(crate) fn active_agent_turn_id(&self) -> Option<u64> {
        self.agent
            .as_ref()
            .map(|agent| agent.turn_id)
            .or(self.dispatching_turn_id)
    }

    pub(crate) fn agent_is_running(&self) -> bool {
        self.active_agent_turn_id().is_some()
    }

    pub(crate) fn prompt_work_state(&self) -> PromptWorkState {
        let turn_active = self.agent_is_running() || self.working.is_compacting();
        if turn_active {
            PromptWorkState::TurnActive
        } else if self.busy_stack.is_busy() {
            PromptWorkState::BackgroundBusy
        } else {
            PromptWorkState::Idle
        }
    }

    pub(crate) fn turn_input_is_active(&self) -> bool {
        self.prompt_work_state().turn_is_active()
    }

    pub(crate) fn prompt_input_is_busy(&self) -> bool {
        self.prompt_work_state().is_busy()
    }

    pub(crate) fn can_continue_turn(&self) -> bool {
        !self.core.session.history.is_empty()
    }

    pub(crate) fn queue_input_for_request(&mut self, queued: QueuedInput) -> bool {
        if !self.turn_input_is_active() {
            return self.queued_inputs.try_push_turn(queued);
        }
        let text = queued.request_text().map(str::to_string);
        if !self.queued_inputs.try_push_request(queued) {
            return false;
        }
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            self.core.engine.send(protocol::UiCommand::Steer { text });
        }
        true
    }

    pub(crate) fn drain_queued_inputs_into_prompt(&mut self) {
        let (request_count, queued) = self.queued_inputs.drain_for_prompt();
        if request_count > 0 {
            self.core.engine.send(protocol::UiCommand::Unsteer {
                count: request_count,
            });
        }
        if queued.is_empty() {
            return;
        }

        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let mut prefix = queued
            .iter()
            .map(QueuedInput::prompt_replay_text)
            .collect::<Vec<_>>()
            .join("\n");
        if !prefix.is_empty() && !pctx.buf.source().is_empty() {
            prefix.push('\n');
        }
        self.input.prepend_text(&mut pctx, prefix);
    }

    pub(crate) fn clear_prompt_prediction(&mut self) {
        self.clear_placeholder(self.well_known.prompt);
    }

    pub(crate) fn invalidate_prompt_prediction(&mut self) {
        self.clear_prompt_prediction();
        self.bump_epoch("input_epoch");
    }

    pub(crate) fn publish_input_submit(&mut self, submitted: String) {
        self.invalidate_prompt_prediction();
        if !submitted.is_empty() {
            self.core
                .cells
                .set_dyn("input_submit", std::rc::Rc::new(submitted));
        }
        self.pump_lua();
    }

    pub(crate) fn bump_epoch(&mut self, name: &str) {
        let next = self
            .core
            .cells
            .get::<u64>(name)
            .unwrap_or_default()
            .wrapping_add(1);
        self.core.cells.set_dyn(name, std::rc::Rc::new(next));
    }

    pub(crate) fn mode_history_base(&self) -> protocol::AgentMode {
        let fallback = self.core.session.mode.as_deref().unwrap_or("normal");
        let history = &self.core.session.history;
        let end = if history.last().is_some_and(|item| {
            item.note_kind() == Some(protocol::HistoryNoteKind::ModeChange)
                && item
                    .as_note()
                    .and_then(protocol::HistoryNote::mode)
                    .is_some()
        }) {
            history.len() - 1
        } else {
            history.len()
        };
        let mode = history[..end]
            .iter()
            .rev()
            .filter_map(protocol::HistoryItem::as_note)
            .find_map(protocol::HistoryNote::mode)
            .unwrap_or(fallback);
        protocol::AgentMode::parse(mode).unwrap_or_else(protocol::AgentMode::normal)
    }

    fn queue_pending_history_append(
        &mut self,
        append: PendingHistoryAppend,
        mode_base: Option<&protocol::AgentMode>,
    ) {
        let replace_note_kind = append.replacement_note_kind();
        if replace_note_kind == Some(protocol::HistoryNoteKind::ModeChange) {
            let Some(new_mode) = append.mode().map(str::to_string) else {
                self.replace_or_push_pending_history_append(append);
                return;
            };
            let existing_idx = self.pending_history_appends.iter().position(|pending| {
                pending.replacement_note_kind() == Some(protocol::HistoryNoteKind::ModeChange)
            });
            if let Some(idx) = existing_idx {
                if mode_base.is_some_and(|base| base.as_str() == new_mode.as_str()) {
                    self.pending_history_appends.remove(idx);
                } else {
                    self.pending_history_appends[idx] = append;
                }
            } else if mode_base.is_none_or(|base| base.as_str() != new_mode.as_str()) {
                self.pending_history_appends.push(append);
            }
            return;
        }

        self.replace_or_push_pending_history_append(append);
    }

    fn replace_or_push_pending_history_append(&mut self, append: PendingHistoryAppend) {
        if let Some(kind) = append.replacement_note_kind() {
            if let Some(existing) = self
                .pending_history_appends
                .iter_mut()
                .find(|pending| pending.replacement_note_kind() == Some(kind))
            {
                *existing = append;
                return;
            }
        }
        self.pending_history_appends.push(append);
    }

    pub(crate) fn queue_history_append(&mut self, append: PendingHistoryAppend) {
        let mode_base = append.mode().map(|_| self.mode_history_base());
        let history_append = append.history_append(mode_base);
        let replace_note_kind = history_append.replacement_note_kind();

        if self.agent_is_running() {
            self.queue_pending_history_append(
                append.clone(),
                match &history_append.policy {
                    protocol::HistoryAppendPolicy::ModeChange { base } => Some(base),
                    _ => None,
                },
            );
            self.core
                .engine
                .send(protocol::UiCommand::AppendHistoryItem {
                    append: history_append,
                });
        } else if !self.core.session.history.is_empty() {
            let result = self.apply_history_append_to_history(&history_append);
            self.commit_history_append_block(
                append.transcript_block(&self.lua),
                replace_note_kind,
                result,
            );
        } else if let Some(kind) = replace_note_kind {
            self.pending_history_appends
                .retain(|pending| pending.replacement_note_kind() != Some(kind));
        }
    }

    pub(crate) fn start_queued_input(&mut self, queued: QueuedInput) {
        self.clear_prompt_prediction();
        match queued {
            QueuedInput::Request(req) => {
                let req = *req;
                match req.turn_options {
                    QueuedTurnOptions::CustomCommand { overrides } => {
                        let text = req.content.text_content().into_owned();
                        let turn = self.begin_command_request_turn(req.display, text, *overrides);
                        self.agent = Some(turn);
                    }
                    QueuedTurnOptions::Default if !req.content.is_empty() => {
                        let turn = self.begin_agent_turn(&req.display, req.content);
                        self.agent = Some(turn);
                    }
                    QueuedTurnOptions::Default => {}
                }
            }
            QueuedInput::ProcessStatus(note) if !note.text().is_empty() => {
                let turn = self.begin_process_status_turn(note);
                self.agent = Some(turn);
            }
            QueuedInput::ProcessStatus(_) => {}
        }
    }

    pub(crate) fn start_next_queued_input_if_idle(&mut self) -> bool {
        if self.prompt_input_is_busy() || self.queued_inputs.is_empty() {
            return false;
        }
        let Some(queued) = self.queued_inputs.pop_next_for_turn() else {
            return false;
        };
        let was_animating = self.working.is_animating();
        self.start_queued_input(queued);
        if was_animating && self.agent.is_none() {
            self.working.finish(smelt_core::working::TurnOutcome::Done);
        }
        true
    }

    pub(crate) fn apply_context_window_update(&mut self, update: ContextWindowUpdate) {
        if update.request_id == self.context_window_request_id
            && update.model == self.core.config.model
            && update.api_base == self.core.config.api_base
            && update.provider_type == self.core.config.provider_type
        {
            self.core.config.context_window = update.value;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: smelt_core::AppConfig,
        mut engine: EngineHandle,
        permissions: Arc<smelt_core::permissions::Permissions>,
        shared_session: Arc<Mutex<Option<Session>>>,
        startup_auth_error: Option<String>,
        lua: crate::lua::LuaRuntime,
        project_trust: smelt_core::trust::TrustState,
        clock: Arc<dyn engine::clock::Clock>,
        env: Arc<engine::env::RuntimeEnv>,
    ) -> Self {
        let host_rx = engine.take_host_rx();
        let input = PromptState::new();
        let vim_enabled = config.settings.vim;

        let cwd = env.cwd().to_string_lossy().into_owned();

        let app_config = config;

        let (term_w, term_h) = terminal::size().unwrap_or((80, 24));
        let prompt_placeholder_display = Arc::new(Mutex::new(None));
        let (ui, well_known) = {
            let mut ui = crate::smelt_edit::Ui::new();
            ui.set_terminal_size(term_w, term_h);
            // Install the baked default theme up-front. `run()` may
            // overwrite it after detecting light/dark, and Lua's
            // `theme.use(...)` will later swap to the user's
            // colorscheme - but until then every render path sees a
            // working theme. Tests that construct TuiApp without
            // calling run() rely on this.
            *ui.theme_mut() = crate::theme::default_baked().as_ref().clone();
            let input_display_buf = ui
                .buf_create_with_id(
                    crate::app::PROMPT_EDIT_BUF,
                    crate::smelt_edit::BufCreateOpts::default(),
                )
                .expect("PROMPT_EDIT_BUF slot is free");
            let parser: std::sync::Arc<dyn crate::smelt_edit::BufferParser> = std::sync::Arc::new(
                crate::content::prompt_parser::PromptBufferParser::with_placeholder(
                    input.store.clone(),
                    prompt_placeholder_display.clone(),
                ),
            );
            let copier: std::sync::Arc<dyn crate::smelt_edit::BufferCopy> = std::sync::Arc::new(
                crate::content::prompt_parser::PromptCopier::new(input.store.clone()),
            );
            if let Some(b) = ui.buf_mut(input_display_buf) {
                b.set_parser(parser);
                b.set_copier(copier);
                b.history = crate::smelt_edit::UndoHistory::new(Some(100));
            }
            let transcript_display_buf = ui.buf_create(crate::smelt_edit::BufCreateOpts::default());
            let transcript_copier: std::sync::Arc<dyn crate::smelt_edit::BufferCopy> =
                std::sync::Arc::new(crate::content::transcript_buf::TranscriptCopier);
            if let Some(b) = ui.buf_mut(transcript_display_buf) {
                b.readonly = true;
                b.set_copier(transcript_copier);
            }
            assert!(ui.win_open_split_at(
                crate::app::TRANSCRIPT_WIN,
                transcript_display_buf,
                crate::smelt_edit::SplitConfig {
                    region: "transcript".into(),
                    gutters: crate::smelt_edit::Gutters {
                        pad_left: 0,
                        pad_right: 0,
                        scrollbar: true,
                    },
                },
            ));
            if let Some(w) = ui.win_mut(crate::app::TRANSCRIPT_WIN) {
                w.set_surface(crate::smelt_edit::WindowSurface::readonly_text());
                w.set_vim_enabled(vim_enabled);
                // Transcript blocks (code, diff) stamp `SourceLine` per row;
                // `LineNumberGutter` is strict - text/markdown rows leave no
                // stamp and contribute no gutter width.
                w.gutter = Some(std::sync::Arc::new(
                    crate::smelt_edit::gutter::LineNumberGutter::new(),
                ));
                // Opt the transcript into per-frame tail-follow. Plugin leaves
                // stay pinned unless a caller explicitly requests tail mode.
                w.follow_tail();
            }
            assert!(ui.win_open_split_at(
                crate::app::PROMPT_WIN,
                input_display_buf,
                crate::smelt_edit::SplitConfig {
                    region: "prompt".into(),
                    gutters: crate::smelt_edit::Gutters {
                        // The reserved scrollbar column doubles as the right gutter
                        // when content fits, so `pad_right` stays 0 to avoid a
                        // double-wide gap. When content overflows, the scrollbar
                        // paints in that column and `pad_left` keeps the input
                        // off the left edge.
                        pad_left: 1,
                        pad_right: 0,
                        ..Default::default()
                    },
                },
            ));
            if let Some(w) = ui.win_mut(crate::app::PROMPT_WIN) {
                w.set_vim_enabled(vim_enabled);
                w.wrap = true;
                w.wrap_cursor_padding = true;
                // Chat input ergonomics: the prompt is for typing, so even with vim
                // enabled the first keystroke after startup should insert, not act
                // as a Normal-mode motion. Other vim-enabled leaves keep the default.
                if vim_enabled {
                    w.set_vim_mode(crate::smelt_edit::VimMode::Insert);
                }
            }
            // Seed a minimal splits tree (transcript + prompt) so overlay
            // anchors resolve before the first render frame. The Lua
            // composer (registered by `runtime/lua/smelt/layout.lua`)
            // replaces this on the next render once `bring_up_lua` runs
            // and `prompt_bar.lua` / `statusline.lua` have allocated
            // their windows.
            ui.set_layout(crate::content::layout::seed_layout_tree(
                /* prompt_input_rows */ 1,
            ));
            ui.set_focus(crate::app::PROMPT_WIN);
            (
                ui,
                WellKnown {
                    prompt: crate::app::PROMPT_WIN,
                    transcript: crate::app::TRANSCRIPT_WIN,
                    cmdline: None,
                },
            )
        };

        let working_clock = Arc::clone(&clock);
        let core = smelt_core::Core::new(
            app_config,
            engine,
            FrontendKind::Tui,
            permissions,
            clock,
            env,
        );
        let (process_completion_tx, process_completion_rx) = tokio::sync::mpsc::unbounded_channel();
        core.processes.set_completion_sender(process_completion_tx);
        let (lua_wakeup_tx, lua_wakeup_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = lua.shared().wakeup_tx.set(lua_wakeup_tx);
        Self {
            core,
            lua,
            transcript: crate::app::transcript::TranscriptView::new(),
            parser: smelt_core::content::stream_parser::StreamParser::new(),
            resume_preview_cache: crate::app::transcript::ResumePreviewCache::new(6),
            input_history: History::load(),
            input,
            exec: None,
            lua_wakeup_rx,
            host_rx,
            queued_inputs: InputQueues::default(),
            cwd,
            shared_session,
            task_label: None,
            pending_dialog: false,
            pending_quit: false,
            notification: None,
            cmdline: crate::app::cmdline::CmdlineState::default(),
            search: crate::app::search::SearchState::default(),
            picker_state: HashMap::new(),
            paint_registry: crate::lua::paint::PaintRegistry::default(),
            term_focused: true,
            working: smelt_core::working::WorkingState::new(working_clock),
            layout: crate::content::layout::LayoutState::default(),
            agent: None,
            inspect_server: None,
            sleep_inhibit: crate::sleep_inhibit::SleepInhibitor::new(),
            persister: crate::persist::Persister::spawn(),
            session_save_pending: false,
            persisted_fingerprints: None,
            session_dirty: false,
            persisted_display_cache_generation: 0,
            last_width: term_w,
            last_height: term_h,
            next_turn_id: 1,
            pending_turn_meta: None,
            pending_history_appends: Vec::new(),
            process_completion_rx,
            context_tokens_updated_this_turn: false,
            cancel_generation: 0,
            dispatching_turn_id: None,
            busy_stack: BusyStack::default(),
            startup_auth_error,
            project_trust: Some(project_trust),
            app_focus: AppFocus::Prompt,
            last_prompt_text: String::new(),
            prompt_input_rows: 1,
            prompt_input_rows_override: None,
            prompt_resize_drag: None,
            prompt_resize_last_click: None,
            prompt_placeholder_display,
            placeholders: HashMap::new(),
            prompt_inputs: crate::prompt_inputs::PromptInputs::default(),
            auto_reload: None,
            pending_lua_reload: false,
            prompt_sections: crate::prompt_sections::PromptSections::default(),
            ui,
            well_known,
            timers: Timers {
                last_ctrlc: None,
                last_keypress: None,
                pending_pane_chord: None,
                pending_transcript_fold_chord: None,
                pending_chord: None,
            },
            pending_dialogs: VecDeque::new(),
            terminal: None,
            http_client: None,
            context_window_tx: None,
            context_window_request_id: 0,
            placeholder_opts: HashMap::new(),
        }
    }

    /// Rebuilds prompt sections from current app state and returns the assembled system prompt.
    /// Mutates `self.prompt_sections`; call sites that just want to read
    /// the current system prompt (Lua getters, EngineAsk inheritance)
    /// should use [`Self::assemble_system_prompt`] instead.
    pub(crate) fn rebuild_system_prompt(&mut self) -> String {
        let cwd = std::path::Path::new(&self.cwd);
        self.prompt_sections = crate::prompt_sections::build_defaults(
            cwd,
            self.core.config.mode.clone(),
            true, // TUI is always interactive
            self.prompt_inputs.skill_section.as_deref(),
            self.prompt_inputs.instructions.as_deref(),
        );
        self.prompt_sections.assemble()
    }

    /// Pure variant of [`Self::rebuild_system_prompt`]: returns the
    /// assembled bytes without committing them to `self.prompt_sections`.
    /// "What is the system prompt right now" reads must not mutate state.
    pub(crate) fn assemble_system_prompt(&self) -> String {
        let cwd = std::path::Path::new(&self.cwd);
        crate::prompt_sections::build_defaults(
            cwd,
            self.core.config.mode.clone(),
            true,
            self.prompt_inputs.skill_section.as_deref(),
            self.prompt_inputs.instructions.as_deref(),
        )
        .assemble()
    }

    pub(crate) fn stop_background_processes(&mut self) {
        self.core.processes.clear();
        while self.process_completion_rx.try_recv().is_ok() {}
    }

    /// Fire due timer callbacks; re-arms recurring entries and drops one-shots.
    pub(crate) fn tick_timers(&mut self) {
        let due = self.core.timers.drain_due(self.lua.lua());
        if due.is_empty() {
            return;
        }
        let _guard = crate::lua::install_app_ptr(self);
        for func in due {
            let _perf = smelt_perf::perf::begin("lua:timer");
            if let Err(e) = func.call::<()>(()) {
                crate::lua::try_with_app(|app| {
                    app.lua.record_error(format!("timer: {e}"));
                });
            }
        }
    }

    fn keymap_pending_cell_value(&self) -> String {
        self.timers
            .pending_chord
            .as_ref()
            .map(|pending| crate::lua::display_chord_sequence(&pending.tokens.concat()))
            .unwrap_or_default()
    }

    pub(crate) fn expire_pending_keymap_chord(&mut self) -> bool {
        let now = self.core.clock.instant_now();
        let expired = self
            .timers
            .pending_chord
            .as_ref()
            .and_then(|pending| pending.policy.expires_at())
            .is_some_and(|expires_at| expires_at <= now);
        if expired {
            self.timers.pending_chord = None;
            return true;
        }
        false
    }

    fn pending_keymap_chord_expiry_delay(&self) -> Option<Duration> {
        let expires_at = self.timers.pending_chord.as_ref()?.policy.expires_at()?;
        Some(expires_at.saturating_duration_since(self.core.clock.instant_now()))
    }

    /// Publish `vim_mode`, `keymap_pending`, `confirms_pending`, `now`,
    /// `notification_visible`, `spinner_frame`, and the `work_*` family of cells whenever their values change.
    pub(crate) fn publish_diff_cells(&mut self) {
        let keymap_pending = self.keymap_pending_cell_value();
        self.core
            .cells
            .publish_if_changed("vim_mode", self.vim_mode_cell_value());
        self.core
            .cells
            .publish_if_changed("keymap_pending", keymap_pending);
        self.core
            .cells
            .publish_if_changed("confirms_pending", !self.core.confirms.is_clear());
        let now_secs = self
            .core
            .clock
            .system_now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.core.cells.publish_if_changed("now", now_secs);
        let frame = self
            .working
            .elapsed()
            .filter(|_| self.working.is_animating())
            .map(|e| smelt_core::content::spinner_frame_index(e) as u8)
            .unwrap_or(0);
        self.core.cells.publish_if_changed("spinner_frame", frame);

        let tps = self
            .working
            .turn_meta()
            .and_then(|m| m.avg_tps)
            .unwrap_or(0.0);
        self.core.cells.publish_if_changed("tps", tps);

        let task_label = self.task_label.clone().unwrap_or_default();
        self.core.cells.publish_if_changed("task_label", task_label);

        let running_procs = self.core.processes.running_count() as u32;
        self.core
            .cells
            .publish_if_changed("running_procs", running_procs);

        let permission_pending = self.pending_dialog && !self.focused_overlay_blocks_agent();
        self.core
            .cells
            .publish_if_changed("permission_pending", permission_pending);

        self.core
            .cells
            .publish_if_changed("notification_visible", self.notification.is_some());

        let cursor = self.focused_cursor_pos();
        self.core.cells.publish_if_changed("cursor_pos", cursor);

        self.publish_work_cells();
    }

    /// User-facing vim mode label for the focused surface - or empty
    /// when no vim-enabled surface owns input. The Lua statusline reads
    /// this directly from the `vim_mode` cell.
    fn vim_mode_cell_value(&self) -> String {
        let Some(mode) = self.focused_vim_mode() else {
            return String::new();
        };
        match mode {
            crate::smelt_edit::VimMode::Insert => "INSERT",
            crate::smelt_edit::VimMode::Visual => "VISUAL",
            crate::smelt_edit::VimMode::VisualLine => "VISUAL LINE",
            crate::smelt_edit::VimMode::Normal => "NORMAL",
        }
        .into()
    }

    /// Cursor position of the focused window, published as `cursor_pos`.
    /// Returns the default `(0, 0, 0)` when no focused window has lines.
    fn focused_cursor_pos(&self) -> smelt_core::cells::CursorPos {
        let Some(w) = self.ui.focused_window() else {
            return smelt_core::cells::CursorPos::default();
        };
        let total = self.ui.buf(w.buf).map(|b| b.line_count()).unwrap_or(0);
        if total == 0 {
            return smelt_core::cells::CursorPos::default();
        }
        let line_idx = w.cursor_abs_row();
        let col = w.cursor_col() as usize;
        let scroll_pct = if total <= 1 {
            100u8
        } else {
            ((line_idx * 100) / (total.saturating_sub(1) as u64)) as u8
        };
        smelt_core::cells::CursorPos {
            line: (line_idx as u32) + 1,
            col: (col as u32) + 1,
            scroll_pct: scroll_pct.min(100),
        }
    }

    /// Resolve the public `WorkState` and label from `WorkingState` +
    /// the per-app busy stack. Engine-side `Working`/`Retrying`/`Paused`
    /// win over busy tokens; an empty engine state with a live busy
    /// token reads as `Busy`; otherwise `Done`/`Interrupted` if a turn
    /// just archived, else `Idle`. The label is the top busy entry's
    /// when set, otherwise `"working"` for live engine phases, otherwise
    /// empty.
    pub(crate) fn resolve_work_state(&self) -> (smelt_core::working::WorkState, String) {
        use smelt_core::working::{TurnOutcome, WorkState};
        let engine = self.working.engine_state();
        let busy_label = self.busy_stack.top_label();
        let outcome = self.working.last_outcome();

        let state = if let Some(s) = engine {
            s
        } else if self.busy_stack.is_busy() {
            WorkState::Busy
        } else {
            match outcome {
                Some(TurnOutcome::Done) => WorkState::Done,
                Some(TurnOutcome::Interrupted) => WorkState::Interrupted,
                None => WorkState::Idle,
            }
        };

        let label = if let Some(l) = busy_label {
            l
        } else if matches!(
            engine,
            Some(WorkState::Working) | Some(WorkState::Retrying) | Some(WorkState::Paused)
        ) {
            self.working.phase_label().unwrap_or("working").to_string()
        } else {
            String::new()
        };

        (state, label)
    }

    /// Derive and publish the `work_*` cells from `WorkingState` and the
    /// per-app busy stack.
    fn publish_work_cells(&mut self) {
        use smelt_core::working::TurnOutcome;

        let (state, label) = self.resolve_work_state();
        let engine = self.working.engine_state();
        let outcome = self.working.last_outcome();

        let outcome_str = match outcome {
            Some(TurnOutcome::Done) if engine.is_none() => "done",
            Some(TurnOutcome::Interrupted) if engine.is_none() => "interrupted",
            _ => "",
        };

        let elapsed_ms = if self.working.is_animating() {
            self.working.elapsed()
        } else if self.busy_stack.is_busy() {
            self.busy_stack.elapsed()
        } else {
            self.working.elapsed()
        }
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

        let (retry_attempt, retry_remaining_ms) = self.working.retry_info().unwrap_or((0, 0));

        self.core
            .cells
            .publish_if_changed("work_state", state.as_str().to_string());
        self.core.cells.publish_if_changed("work_label", label);
        self.core
            .cells
            .publish_if_changed("work_elapsed_ms", elapsed_ms);
        self.core
            .cells
            .publish_if_changed("work_outcome", outcome_str.to_string());
        self.core
            .cells
            .publish_if_changed("work_retry_attempt", retry_attempt);
        self.core
            .cells
            .publish_if_changed("work_retry_remaining_ms", retry_remaining_ms);
        self.core
            .cells
            .publish_if_changed("work_busy", self.busy_stack.entries_snapshot());
    }

    /// Drain pending cell-fire notifications and invoke subscribers.
    pub(crate) fn drain_cells_pending(&mut self) {
        if !self.core.cells.has_pending() {
            return;
        }
        let fires = self.core.cells.drain_pending();
        let lua = self.lua.lua();
        let mut calls = Vec::new();
        for fire in fires {
            let value = self.core.cells.project_to_lua(&*fire.value, lua);
            let prev = self.core.cells.project_to_lua(&*fire.prev, lua);
            for cb in &fire.callbacks {
                let smelt_core::cells::SubscriberKind::Lua(handle) = &cb.kind;
                let func = match lua.registry_value::<mlua::Function>(&handle.key) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                calls.push((
                    fire.name.clone(),
                    value.clone(),
                    prev.clone(),
                    func,
                    cb.is_glob,
                ));
            }
        }
        let _guard = crate::lua::install_app_ptr(self);
        for (name, value, prev, func, is_glob) in calls {
            let _perf = smelt_perf::perf::begin("lua:cell_cb");
            let result = if is_glob {
                func.call::<()>((name.clone(), value, prev))
            } else {
                func.call::<()>((value, prev))
            };
            if let Err(e) = result {
                crate::lua::try_with_app(|app| {
                    app.lua.record_error(format!("cell `{name}`: {e}"));
                });
            }
        }
    }

    fn sync_prompt_placeholder_display(&mut self) {
        let text = self.placeholders.get(&crate::app::PROMPT_WIN).cloned();
        let mut guard = self.prompt_placeholder_display.lock().unwrap();
        if *guard == text {
            return;
        }
        *guard = text;
        if let Some(buf) = self.ui.buf_mut(crate::app::PROMPT_EDIT_BUF) {
            buf.invalidate_render_cache();
        }
    }

    /// Returns the current placeholder text on `win`, if any.
    pub(crate) fn placeholder_text(&mut self, win: crate::smelt_edit::WinId) -> Option<String> {
        if let Some(text) = self.placeholders.get(&win) {
            return Some(text.clone());
        }
        let buf = self.ui.win_buf_mut(win)?;
        crate::content::prompt_buf::placeholder_text(buf)
    }

    /// Set the placeholder text on `win`. Empty text clears any prior placeholder.
    pub fn set_placeholder(&mut self, win: crate::smelt_edit::WinId, text: String) {
        if text.is_empty() {
            self.clear_placeholder(win);
            return;
        }
        if win == crate::app::PROMPT_WIN {
            self.placeholders.insert(win, text);
            self.sync_prompt_placeholder_display();
            return;
        }
        if self.ui.win(win).is_none() {
            return;
        }
        self.placeholders.insert(win, text.clone());
        if let Some(buf) = self.ui.win_buf_mut(win) {
            crate::content::prompt_buf::set_placeholder_extmark(buf, Some(text));
        }
    }

    /// Clear the placeholder on `win` (text + opts). Idempotent.
    pub fn clear_placeholder(&mut self, win: crate::smelt_edit::WinId) {
        self.placeholders.remove(&win);
        if win == crate::app::PROMPT_WIN {
            self.sync_prompt_placeholder_display();
        }
        if let Some(buf) = self.ui.win_buf_mut(win) {
            crate::content::prompt_buf::set_placeholder_extmark(buf, None);
        }
        self.placeholder_opts.remove(&win);
    }

    /// Match a key against the placeholder dispatch policy for `win`.
    /// Returns `Some(Redraw)` if the key was consumed by accept/dismiss,
    /// `None` otherwise.
    ///
    /// Accept replaces the buffer with the first line of the placeholder text
    /// (multi-line predictions collapse to a single line for safety) and fires
    /// `WinEvent::PlaceholderAccepted`. Dismiss clears the placeholder and fires
    /// `WinEvent::PlaceholderDismissed`. Both run only when the buffer is empty
    /// - the same visibility rule that gates rendering.
    pub(crate) fn dispatch_placeholder_key(
        &mut self,
        win: crate::smelt_edit::WinId,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> Option<EventOutcome> {
        let text = self.placeholder_text(win)?;
        let opts = self.placeholder_opts.get(&win)?.clone();
        let buf_empty = self
            .ui
            .win_buf_mut(win)
            .map(|b| b.source().is_empty())
            .unwrap_or(true);
        if !buf_empty {
            return None;
        }
        let kb = crate::smelt_edit::KeyBind::new(code, mods);
        if opts.accept_keys.contains(&kb) {
            self.clear_placeholder(win);
            if win == self.well_known.prompt {
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.input.replace_text(&mut pctx, text.clone());
            }
            self.fire_placeholder_event(
                win,
                crate::smelt_edit::WinEvent::PlaceholderAccepted,
                text,
            );
            return Some(EventOutcome::Redraw);
        }
        if opts.dismiss_keys.contains(&kb) {
            self.clear_placeholder(win);
            self.fire_placeholder_event(
                win,
                crate::smelt_edit::WinEvent::PlaceholderDismissed,
                text,
            );
            return Some(EventOutcome::Redraw);
        }
        None
    }

    fn fire_placeholder_event(
        &mut self,
        win: crate::smelt_edit::WinId,
        event: crate::smelt_edit::WinEvent,
        text: String,
    ) {
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              w: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, w, payload);
        };
        self.ui.fire_win_event(
            win,
            event,
            crate::smelt_edit::Payload::Text { content: text },
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    /// Gutters configured on the transcript window (single source of truth: `Window.config.gutters`).
    pub(crate) fn transcript_gutters(&self) -> crate::smelt_edit::Gutters {
        self.ui
            .win(crate::app::TRANSCRIPT_WIN)
            .map(|w| w.config.gutters)
            .unwrap_or_default()
    }

    pub(crate) fn transcript_win(&self) -> &crate::smelt_edit::Window {
        self.ui
            .win(self.well_known.transcript)
            .expect("transcript window")
    }

    pub(crate) fn transcript_win_mut(&mut self) -> &mut crate::smelt_edit::Window {
        self.ui
            .win_mut(self.well_known.transcript)
            .expect("transcript window")
    }

    pub(crate) fn prompt_buf(&self) -> &crate::smelt_edit::Buffer {
        self.ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .expect("prompt edit buffer")
    }

    pub(crate) fn prompt_win(&self) -> &crate::smelt_edit::Window {
        self.ui.win(crate::app::PROMPT_WIN).expect("prompt window")
    }

    pub(crate) fn prompt_win_mut(&mut self) -> &mut crate::smelt_edit::Window {
        self.ui
            .win_mut(crate::app::PROMPT_WIN)
            .expect("prompt window")
    }

    /// Width available for transcript content (terminal width minus gutter/scrollbar columns).
    pub(crate) fn transcript_width(&self) -> usize {
        let (w, _) = self.ui.terminal_size();
        (self.transcript_gutters().content_width(w) as usize).max(1)
    }

    /// Resolves a raw leaf id to a live `Window` or a registered paint region (`None` if unrecognised).
    pub(crate) fn resolve_leaf_id(&self, raw_id: u64) -> Option<crate::lua::paint::LeafKind> {
        let win = crate::smelt_edit::WinId(raw_id);
        if self.ui.win(win).is_some() {
            // Catches a future regression where the smelt-edit allocator
            // grows past the partition boundary into paint-id space.
            debug_assert!(
                raw_id < crate::lua::paint::PAINT_ID_BASE,
                "WinId {raw_id} crossed into PaintId half (>= {})",
                crate::lua::paint::PAINT_ID_BASE
            );
            return Some(crate::lua::paint::LeafKind::Window(win));
        }
        let paint_id = crate::smelt_edit::layout::PaintId(raw_id);
        if self.paint_registry.contains(paint_id) {
            return Some(crate::lua::paint::LeafKind::Paint(paint_id));
        }
        None
    }

    pub(crate) fn notify(&mut self, message: String) {
        self.record_notice(
            smelt_core::messages::MessageKind::Info,
            "smelt".into(),
            message,
        );
    }

    pub(crate) fn notify_error(&mut self, message: String) {
        self.record_notice(
            smelt_core::messages::MessageKind::Error,
            "smelt".into(),
            message,
        );
    }

    pub(crate) fn notify_error_sticky(&mut self, message: String) {
        self.record_notice_with_lifetime(
            smelt_core::messages::MessageKind::Error,
            "smelt".into(),
            message,
            NotificationLifetime::Sticky,
        );
    }

    #[allow(dead_code)] // Only reachable via the Lua surface today.
    pub(crate) fn notify_warn(&mut self, message: String) {
        self.record_notice(
            smelt_core::messages::MessageKind::Warning,
            "smelt".into(),
            message,
        );
    }

    /// Append `body` to the persistent message log AND surface the first
    /// line of `body` as a toast clipped to the terminal width. Every
    /// user-visible toast in the TUI goes through here so `/messages`
    /// stays a faithful audit log and no toast can ever wrap past its
    /// reserved row.
    pub(crate) fn record_notice(
        &mut self,
        kind: smelt_core::messages::MessageKind,
        source: String,
        body: String,
    ) {
        self.record_notice_with_lifetime(
            kind,
            source,
            body,
            NotificationLifetime::timed(self.core.clock.instant_now()),
        );
    }

    fn record_notice_with_lifetime(
        &mut self,
        kind: smelt_core::messages::MessageKind,
        source: String,
        body: String,
        lifetime: NotificationLifetime,
    ) {
        if let Ok(mut messages) = self.lua.core_shared().messages.lock() {
            messages.append(kind, source, body.clone());
        }
        self.open_notification(kind, &body, lifetime);
    }

    fn open_notification(
        &mut self,
        kind: smelt_core::messages::MessageKind,
        body: &str,
        lifetime: NotificationLifetime,
    ) {
        use smelt_core::messages::MessageKind;
        if let Some(notification) = self.notification.take() {
            self.close_overlay_leaf(notification.win);
        }

        let label = match kind {
            MessageKind::Info => "info",
            MessageKind::Warning => "warn",
            MessageKind::Error => "error",
        };
        let indent = "  ";
        let gap = "  ";

        // The toast is a single visible row anchored over the prompt-above
        // region. A multi-line body (e.g. an error with a traceback) would
        // wrap past that row and obliterate the prompt bar underneath, so
        // collapse to the first line and clamp to terminal width.
        let summary = body.lines().next().unwrap_or("");
        let prefix_w = indent.len() + label.len() + gap.len();
        let term_w = self.ui.terminal_size().0 as usize;
        let available = term_w.saturating_sub(prefix_w);
        let summary =
            smelt_core::content::width::truncate_with_right_padding(summary, available, 2, "…");
        let line = format!("{indent}{label}{gap}{}", summary.text);

        let buf = self
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());

        let label_start = indent.len() as u16;
        let label_end = label_start + label.len() as u16;
        let msg_start = label_end + gap.len() as u16;
        let msg_end = msg_start + summary.body.chars().count() as u16;

        let label_color = match kind {
            MessageKind::Error => self.ui.theme().get("ErrorMsg").fg,
            MessageKind::Warning => self.ui.theme().get("WarningMsg").fg,
            MessageKind::Info => None,
        };
        if let Some(b) = self.ui.buf_mut(buf) {
            b.set_all_lines(vec![line]);
            b.add_highlight(
                0,
                label_start,
                label_end,
                crate::smelt_edit::SpanStyle {
                    fg: label_color,
                    bold: true,
                    ..Default::default()
                },
            );
            b.add_highlight(
                0,
                msg_start,
                msg_end,
                crate::smelt_edit::SpanStyle {
                    dim: true,
                    ..Default::default()
                },
            );
        }

        let Some(win) = self.ui.win_open_split(
            buf,
            crate::smelt_edit::SplitConfig {
                region: "notification".into(),
                gutters: Default::default(),
            },
        ) else {
            return;
        };
        if let Some(w) = self.ui.win_mut(win) {
            w.set_surface(crate::smelt_edit::WindowSurface::selectable_text());
        }

        let layout = crate::smelt_edit::LayoutTree::vbox(vec![(
            crate::smelt_edit::Constraint::Length(1),
            crate::smelt_edit::LayoutTree::hbox(vec![(
                crate::smelt_edit::Constraint::Percentage(100),
                crate::smelt_edit::LayoutTree::leaf(win),
            )]),
        )]);
        // Float `1` row above the prompt's Lua-allocated top bar (or the
        // prompt input on cold start, before Lua has registered the bar).
        // Anchoring against the named window keeps the toast correctly
        // placed even when queued/stash rows grow the top bar.
        let anchor = crate::content::layout::anchor_above_prompt_chrome(&self.ui, 1);
        let _overlay_id = self.ui.overlay_open(
            crate::smelt_edit::Overlay::new(layout, anchor)
                // Sits below dialogs (default overlay z 50) so a toast
                // never obscures a modal asking for input.
                .with_z(40),
        );
        self.notification = Some(Notification { win, lifetime });
    }

    pub(crate) fn dismiss_notification(&mut self) {
        if let Some(notification) = self.notification.take() {
            self.close_overlay_leaf(notification.win);
        }
    }

    pub(crate) fn dismiss_expired_notification(&mut self) -> bool {
        let now = self.core.clock.instant_now();
        if self
            .notification
            .is_some_and(|notification| notification.lifetime.is_expired(now))
        {
            self.dismiss_notification();
            return true;
        }
        false
    }

    pub(crate) fn notification_expiry_delay(&self) -> Option<Duration> {
        let now = self.core.clock.instant_now();
        self.notification
            .and_then(|notification| notification.lifetime.expiry_delay(now))
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn notification_win(&self) -> Option<crate::smelt_edit::WinId> {
        self.notification.map(|notification| notification.win)
    }

    pub(crate) fn set_task_label(&mut self, label: String) {
        self.task_label = if label.trim().is_empty() {
            None
        } else {
            Some(label)
        };
    }

    fn dispatch_ui_window_events(&mut self, include_tick: bool) {
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_edit::LuaHandle,
                 win: crate::smelt_edit::WinId,
                 payload: &crate::smelt_edit::Payload| {
                    lua.queue_invocation(handle, win, payload);
                };
            if include_tick {
                self.ui.dispatch_tick(&mut lua_invoke);
            }
            self.ui.dispatch_resize_events(&mut lua_invoke);
            self.ui.dispatch_scroll_events(&mut lua_invoke);
        }
        self.flush_lua_callbacks();
    }

    pub async fn run(&mut self, http_client: engine::HttpClient, initial_message: Option<String>) {
        let (ctx_tx, mut ctx_rx) = tokio::sync::mpsc::unbounded_channel::<ContextWindowUpdate>();
        self.http_client = Some(http_client);
        self.context_window_tx = Some(ctx_tx);
        self.refresh_context_window();
        crate::theme::detect_background(self.ui.theme_mut());
        // Install the baked default theme so the first frame renders with
        // real colors before Lua's `theme.use(...)` runs during bootstrap.
        // Lua-side colorschemes overwrite this via `smelt.theme.apply`.
        let is_light = self.ui.theme().is_light();
        let baked = crate::theme::default_baked_with_background(is_light);
        self.install_theme(baked);
        // Capture the thread-safe Lua command-name set directly. Rendering and
        // measurement can run outside Lua's main-thread APP context, so slash
        // command detection must reach the registry without consulting APP.
        // `commands` itself can't cross those boundaries (the handler holds a
        // `LuaHandle`), so this uses the name-only mirror.
        let command_names = self.lua.command_names_handle();
        smelt_core::commands::set_command_resolver(move |name| {
            command_names
                .lock()
                .map(|s| s.contains(name))
                .unwrap_or(false)
        });
        // RAII guard for the terminal envelope: raw mode + alt screen + mouse +
        // bracketed paste + focus + DECAWM-off + hidden cursor. Lives as long
        // as `run()`; `Drop` restores cooked mode and the normal screen, even
        // on panic. Shell-outs go through `self.terminal.as_ref().suspended()`.
        self.terminal = crate::term_setup::TuiTerminal::claim().ok();

        if !self.core.session.history.is_empty() {
            self.restore_screen();
            if let Some(ref slug) = self.core.session.slug {
                self.set_task_label(slug.clone());
            }
            self.finish_transcript_turn();
            self.transcript_win_mut().follow_tail();
        }
        if let Some(message) = self.startup_auth_error.take() {
            self.notify_error_sticky(message);
        }

        {
            let _guard = crate::lua::install_app_ptr(self);
            self.core.cells.set_dyn(
                "session_started",
                std::rc::Rc::new(self.core.session.id.clone()),
            );
            self.drain_cells_pending();
        }
        if let Some(state) = self.project_trust.take() {
            if matches!(state, smelt_core::trust::TrustState::Untrusted { .. }) {
                self.notify(
                    "project .smelt/ content not trusted; run /trust to load it".to_string(),
                );
            }
        }

        let mut auto_reload_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;
        if self.core.config.settings.auto_reload {
            let paths = crate::auto_reload::WatchPaths::discover(
                std::path::Path::new(&self.cwd),
                &self.prompt_inputs.skill_extra_paths,
                self.prompt_inputs.system_prompt_path.clone(),
            );
            if let Some((handle, rx)) = crate::auto_reload::spawn(paths) {
                self.auto_reload = Some(handle);
                auto_reload_rx = Some(rx);
            }
        }

        let mut term_events = match crate::term_input::TerminalInput::spawn() {
            Ok(input) => input,
            Err(e) => {
                self.notify_error(format!("terminal input: {e}"));
                self.terminal = None;
                return;
            }
        };
        // Independent SIGWINCH listener: crossterm's signal source intermittently drops
        // resize events (signal-hook-mio counter / mio readiness race), so we keep our
        // own tokio-native handler. Both fire on resize; the duplicate just hits an
        // idempotent `compositor.resize` and one extra full repaint.
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                .expect("install SIGWINCH listener");

        // Cold-start the Lua context through the same pipeline `/reload`
        // uses. `main` already ran a pre-TUI plugin pass to extract
        // engine config - that pass couldn't touch `smelt.win`,
        // `smelt.overlay`, `smelt.paint`, `smelt.cell:subscribe`, etc.
        // because the host pointer wasn't installed yet. Re-running
        // here inside `install_app_ptr` makes the host live for module
        // bodies on every Lua-context init (cold start AND `/reload`),
        // so plain `if persist().is_open then open() end` at module
        // top works in both. `lifecycle.on("ready")` hooks drain at
        // the end with `ctx.kind = "launch"`.
        let load_err = crate::lua::with_app_ptr(self, |app| app.bring_up_lua("launch"));
        if let Some(err) = load_err {
            self.notify_error_sticky(format!("lua init: {err}"));
        }

        // Auto-submit initial message if provided (e.g. `agent "fix the bug"`).
        if let Some(msg) = initial_message {
            let trimmed = msg.trim();
            if let Some(cmd) = trimmed.strip_prefix('!') {
                if let Some(handle) = self.start_shell_escape(cmd) {
                    self.exec = Some(handle);
                }
            } else if let Some(token) = smelt_core::commands::registered_command_token(trimmed) {
                let name = &token[1..];
                if self.lua.command_startup_ok(name) == Some(true) {
                    self.apply_lua_command(trimmed);
                } else {
                    self.notify_error(format!(
                        "\"{}\" has no effect as a startup argument",
                        trimmed
                    ));
                }
            } else {
                let content = Content::text(msg.clone());
                let turn = self.begin_agent_turn(&msg, content);
                self.agent = Some(turn);
            }
        }

        const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);

        'main: loop {
            let _app_guard = crate::lua::install_app_ptr(self);
            if self.pending_quit {
                self.discard_turn(TurnEnd::Cancelled);
                break 'main;
            }
            self.tick_timers();
            self.publish_diff_cells();
            self.drain_cells_pending();
            self.drive_lua_tasks();
            for _id in self.drain_finished_blocks() {
                self.core
                    .cells
                    .set_dyn("block_done", std::rc::Rc::new(smelt_core::cells::EventStub));
            }
            self.pump_lua();
            self.dispatch_ui_window_events(true);

            while let Ok(update) = ctx_rx.try_recv() {
                self.apply_context_window_update(update);
            }

            self.drain_host_calls();

            if self.drain_idle_work() {
                self.render_normal();
                continue 'main;
            }

            loop {
                let ev = match self.core.engine.try_recv() {
                    Ok(ev) => ev,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        engine::log::entry(
                            engine::log::Level::Warn,
                            "engine_stop",
                            &serde_json::json!({
                                "reason": "channel_disconnected",
                                "source": "try_recv_drain",
                            }),
                        );
                        self.discard_turn(TurnEnd::Errored);
                        break;
                    }
                };
                let action = self.dispatch_engine_event(ev);
                if !action {
                    break;
                }
            }

            while let Ok(completion) = self.process_completion_rx.try_recv() {
                self.handle_process_completed(completion.id, completion.exit_code);
            }

            if self.drain_idle_work() {
                self.render_normal();
                continue 'main;
            }

            self.start_next_queued_input_if_idle();

            if self.agent.is_none() && !self.pending_dialogs.is_empty() {
                self.pending_dialogs.clear();
                self.pending_dialog = false;
            }
            if !self.pending_dialogs.is_empty()
                && !self.focused_overlay_blocks_agent()
                && self.agent.is_some()
            {
                let idle = self
                    .timers
                    .last_keypress
                    .map(|lk| lk.elapsed() >= Duration::from_millis(CONFIRM_DEFER_MS))
                    .unwrap_or(true);
                while idle
                    && !self.pending_dialogs.is_empty()
                    && !self.focused_overlay_blocks_agent()
                    && self.agent.is_some()
                {
                    let deferred = self.pending_dialogs.pop_front().unwrap();
                    let ctrl = match deferred {
                        DeferredDialog::Confirm(req) => SessionControl::NeedsConfirm(req),
                    };
                    let mut turn = self
                        .agent
                        .take()
                        .expect("deferred dialog requires active turn");
                    let end = self.dispatch_control(ctrl, &mut turn);
                    self.agent = Some(turn);
                    match end {
                        SessionControl::Continue | SessionControl::NeedsConfirm(_) => {}
                        SessionControl::Done => {
                            self.discard_turn(TurnEnd::Complete);
                        }
                        SessionControl::Error => {
                            self.discard_turn(TurnEnd::Errored);
                        }
                    }
                }
                self.pending_dialog = !self.pending_dialogs.is_empty();
            }

            self.render_normal();
            let last_frame = self.core.clock.instant_now();

            let now = self.core.clock.instant_now();
            let yank_flash_active = self
                .core
                .clipboard
                .kill_ring
                .yank_flash_until()
                .is_some_and(|t| t > now);
            let row_yank_flash_active = self
                .ui
                .win(TRANSCRIPT_WIN)
                .and_then(|w| w.row_yank_flash_until())
                .is_some_and(|t| t > now);
            let drag_active = self.ui.drag_capture_window().is_some();
            let has_animation = self.ui.focused_overlay().is_some()
                || self.has_active_exec()
                || self.working.is_animating()
                || self.busy_stack.is_busy()
                || yank_flash_active
                || row_yank_flash_active
                || drag_active;
            let next_timer_delay = self
                .core
                .timers
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(now));
            let next_notification_delay = self.notification_expiry_delay();
            let next_keymap_delay = self.pending_keymap_chord_expiry_delay();
            let next_idle_delay = [next_timer_delay, next_notification_delay, next_keymap_delay]
                .into_iter()
                .flatten()
                .min();

            tokio::select! {
                biased;

                Some(ev) = term_events.recv() => {
                    // Coalesce scroll: batch rapid wheel ticks into a single motion + one render.
                    // Disabled when an overlay is focused so wheel events route into it.
                    let coalesce_scroll = self.ui.focused_overlay().is_none();
                    let mut scroll_delta: isize = 0;
                    let mut scroll_row: u16 = 0;
                    let mut scroll_col: u16 = 0;
                    let absorb = |ev: event::Event,
                                      delta: &mut isize,
                                      row: &mut u16,
                                      col: &mut u16|
                     -> Option<event::Event> {
                        if !coalesce_scroll {
                            return Some(ev);
                        }
                        if let event::Event::Mouse(m) = &ev {
                            match m.kind {
                                event::MouseEventKind::ScrollUp => {
                                    *delta -= 3;
                                    *row = m.row;
                                    *col = m.column;
                                    return None;
                                }
                                event::MouseEventKind::ScrollDown => {
                                    *delta += 3;
                                    *row = m.row;
                                    *col = m.column;
                                    return None;
                                }
                                _ => {}
                            }
                        }
                        Some(ev)
                    };

                    if let Some(ev) = absorb(
                        ev,
                        &mut scroll_delta,
                        &mut scroll_row,
                        &mut scroll_col,
                    ) {
                        if self.dispatch_terminal_event(ev) {
                            break 'main;
                        }
                    }

                    while let Ok(ev) = term_events.try_recv() {
                        if let Some(ev) = absorb(
                            ev,
                            &mut scroll_delta,
                            &mut scroll_row,
                            &mut scroll_col,
                        ) {
                            if self.dispatch_terminal_event(ev) {
                                break 'main;
                            }
                        }
                    }

                    if scroll_delta != 0 {
                        let _ = self.ui.scroll_at(scroll_row, scroll_col, scroll_delta);
                    }

                    self.dispatch_ui_window_events(false);
                    self.publish_diff_cells();
                    self.render_normal();
                }

                Some(completion) = self.process_completion_rx.recv() => {
                    self.handle_process_completed(completion.id, completion.exit_code);
                }

                Some(ev) = self.core.engine.recv() => {
                    self.dispatch_engine_event(ev);
                }

                Some(call) = self.host_rx.recv() => {
                    self.dispatch_host_call(call);
                    // Drain any pending follow-ups in the same wake so
                    // multiple host calls don't serialise on one tick.
                    self.drain_host_calls();
                }

                Some(_) = self.lua_wakeup_rx.recv() => {
                    while self.lua_wakeup_rx.try_recv().is_ok() {}
                    self.flush_lua_callbacks();
                    self.drive_lua_tasks();
                    self.render_normal();
                }

                Some(_) = async {
                    match auto_reload_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // Drain follow-up signals so an editor that produced
                    // a fresh burst right at the boundary doesn't queue
                    // a second reload tick we'd execute immediately.
                    if let Some(rx) = auto_reload_rx.as_mut() {
                        while rx.try_recv().is_ok() {}
                    }
                    if self.prompt_input_is_busy() || self.ui.active_modal().is_some() {
                        self.schedule_lua_reload();
                        continue;
                    }
                    self.reload_lua();
                    self.render_normal();
                }

                Some(ev) = async {
                    match self.exec.as_mut() {
                        Some(handle) => handle.rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match ev {
                        crate::commands::ExecEvent::Output(line) => {
                            self.append_exec_output(&line);
                        }
                        crate::commands::ExecEvent::Done(code) => {
                            self.finish_exec(code);
                            self.finalize_exec();
                            self.exec = None;
                        }
                    }
                }

                _ = tokio::time::sleep({
                    let since = last_frame.elapsed();
                    let want = self
                        .ui
                        .drag_autoscroll_interval()
                        .unwrap_or(MIN_FRAME_INTERVAL);
                    want.saturating_sub(since)
                }), if has_animation => {
                    if self.ui.tick_drag_autoscroll() {
                        self.dispatch_ui_window_events(false);
                    }
                    self.publish_diff_cells();
                    self.render_normal();
                }

                _ = tokio::time::sleep(next_idle_delay.unwrap_or(Duration::MAX)), if next_idle_delay.is_some() => {
                    self.tick_timers();
                    self.drive_lua_tasks();
                    self.dismiss_expired_notification();
                    self.expire_pending_keymap_chord();
                    self.publish_diff_cells();
                    self.render_normal();
                }

                Some(_) = sigwinch.recv() => {
                    if let Ok((w, h)) = terminal::size() {
                        if w != self.last_width || h != self.last_height {
                            self.handle_resize(w, h);
                            self.render_normal();
                        }
                    }
                }
            }
        }

        crate::lua::with_app_ptr(self, |app| {
            if app.agent.is_some() {
                app.finish_turn(crate::app::TurnEnd::Cancelled);
            }
            app.core
                .cells
                .set_dyn("shutdown", std::rc::Rc::new(smelt_core::cells::EventStub));
            app.drain_cells_pending();
            app.stop_background_processes();
            app.save_session();
        });

        // Stop the stdin reader before releasing terminal modes so no background
        // thread can keep consuming bytes after the TUI gives the terminal back.
        drop(term_events);

        // Drop the terminal guard last so any rendering above stays in TUI mode.
        self.terminal = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_context_window_update_does_not_overwrite_current_generation() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.context_window_request_id = 2;
        app.app.core.config.model = "gpt-5.5".into();
        app.app.core.config.api_base = "https://codex.example".into();
        app.app.core.config.provider_type = "codex".into();
        app.app.core.config.context_window = Some(272_000);

        app.app.apply_context_window_update(ContextWindowUpdate {
            request_id: 1,
            model: "gpt-5.5".into(),
            api_base: "https://codex.example".into(),
            provider_type: "codex".into(),
            value: None,
        });

        assert_eq!(app.app.core.config.context_window, Some(272_000));
    }

    #[test]
    fn current_context_window_update_applies_even_when_value_is_none() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.context_window_request_id = 2;
        app.app.core.config.model = "Qwen/Qwen3.6-27B".into();
        app.app.core.config.api_base = "https://openai-compatible.example".into();
        app.app.core.config.provider_type = "openai-compatible".into();
        app.app.core.config.context_window = Some(272_000);

        app.app.apply_context_window_update(ContextWindowUpdate {
            request_id: 2,
            model: "Qwen/Qwen3.6-27B".into(),
            api_base: "https://openai-compatible.example".into(),
            provider_type: "openai-compatible".into(),
            value: None,
        });

        assert_eq!(app.app.core.config.context_window, None);
    }

    #[test]
    fn matching_request_id_with_stale_model_identity_does_not_apply() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.context_window_request_id = 2;
        app.app.core.config.model = "gpt-5.5".into();
        app.app.core.config.api_base = "https://codex.example".into();
        app.app.core.config.provider_type = "codex".into();
        app.app.core.config.context_window = Some(272_000);

        app.app.apply_context_window_update(ContextWindowUpdate {
            request_id: 2,
            model: "Qwen/Qwen3.6-27B".into(),
            api_base: "https://openai-compatible.example".into(),
            provider_type: "openai-compatible".into(),
            value: None,
        });

        assert_eq!(app.app.core.config.context_window, Some(272_000));
    }
}
