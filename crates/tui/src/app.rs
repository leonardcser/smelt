pub(crate) mod agent;
pub(crate) mod cmdline;
pub(crate) mod cmdline_edit;
pub(crate) mod content_keys;
pub(crate) mod engine_events;
pub(crate) mod events;
pub(crate) mod history;
pub(crate) mod host_dispatch;
pub(crate) mod lua_bridge;
pub(crate) mod lua_handlers;
pub(crate) mod mouse;
pub(crate) mod pane_focus;
pub(crate) mod render_loop;
pub(crate) mod status_bar;
#[cfg(any(test, feature = "harness"))]
pub mod test_harness;
pub(crate) mod transcript;
pub(crate) mod ui_host;
pub(crate) mod well_known;

use crate::input::PromptState;
use engine::EngineHandle;
use protocol::Content;
use smelt_core::history::History;
use smelt_core::session::Session;
use smelt_core::ConfirmRequest;
use smelt_core::FrontendKind;
use std::sync::Arc;

use crossterm::{
    event::{self, EventStream},
    terminal,
};
use std::collections::{HashMap, VecDeque};

use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct TuiApp {
    pub core: smelt_core::Core,
    pub lua: crate::lua::LuaRuntime,
    pub(crate) transcript: smelt_core::content::transcript::Transcript,
    pub(crate) parser: smelt_core::content::stream_parser::StreamParser,
    pub(crate) transcript_projection: crate::content::transcript_buf::TranscriptProjection,
    pub(crate) input_history: History,
    pub(crate) input: PromptState,
    pub(crate) exec: Option<crate::commands::ExecHandle>,
    /// Wakeup from cross-thread tasks that pushed to the Lua inbox. Drains the inbox so parked coroutines resume.
    lua_wakeup_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    /// Host-callback receiver from the engine task. Lives next to the
    /// engine's event receiver but is moved out at construction time so
    /// the two can be polled in the same `tokio::select!`.
    pub(crate) host_rx: tokio::sync::mpsc::UnboundedReceiver<engine::HostCall>,
    pub(crate) queued_messages: Vec<String>,
    /// Current working directory (cached at startup).
    pub(crate) cwd: String,
    pub(crate) shared_session: Arc<Mutex<Option<Session>>>,
    pub(crate) task_label: Option<String>,
    pub(crate) pending_dialog: bool,
    pub(crate) pending_quit: bool,
    /// Items from Lua-registered statusline sources, appended each frame.
    pub(crate) custom_status_items: Vec<crate::content::status::StatusItem>,
    /// Last error per statusline source; rate-limits toast spam.
    statusline_last_errors: HashMap<String, String>,
    pub(crate) notification: Option<crate::smelt_term::WinId>,
    pub(crate) cmdline: crate::app::cmdline::CmdlineState,
    pub(crate) picker_state: HashMap<crate::smelt_term::WinId, crate::picker::PickerState>,
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
    pub(crate) sleep_inhibit: crate::sleep_inhibit::SleepInhibitor,
    pub(crate) persister: crate::persist::Persister,
    pub(crate) last_width: u16,
    pub(crate) last_height: u16,
    pub(crate) next_turn_id: u64,
    pub(crate) pending_turn_meta: Option<protocol::TurnMeta>,
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
    pub(crate) prompt_sections: crate::prompt_sections::PromptSections,
    pub ui: crate::smelt_term::Ui,
    pub(crate) well_known: WellKnown,
    /// Timers + chord state observed and updated by event dispatch.
    pub(crate) timers: Timers,
    /// Confirm/dialog requests deferred while the user is still typing.
    pub(crate) pending_dialogs: VecDeque<DeferredDialog>,
    /// Owned for the lifetime of `run()`. `None` outside that scope — the
    /// test harness constructs a `TuiApp` without a real terminal and skips
    /// claiming. `Drop` here restores the terminal even on panic.
    pub(crate) terminal: Option<crate::term_setup::TuiTerminal>,
    /// Shared HTTP client used for background side-fetches (context window).
    /// `None` in the test harness.
    pub(crate) http_client: Option<engine::HttpClient>,
    /// Sender into the channel `run()` drains for context-window updates.
    /// `apply_model` spawns a fetch task that pushes the result here so the
    /// UI footer reflects the new model immediately.
    pub(crate) context_window_tx: Option<tokio::sync::mpsc::UnboundedSender<Option<u32>>>,
    /// Per-window placeholder dispatch options. `text` lives on the
    /// buffer (extmark) for the prompt; this side-table holds the
    /// accept/dismiss chord policy plugins configure when calling
    /// `Win:placeholder(text, opts)`.
    pub(crate) placeholder_opts: HashMap<crate::smelt_term::WinId, PlaceholderOpts>,
}

/// Per-window dispatch policy for a placeholder. Set via Lua's
/// `Win:placeholder(text, opts)`; the dispatcher consults it on key events.
#[derive(Default, Clone)]
pub(crate) struct PlaceholderOpts {
    pub accept_keys: Vec<crate::smelt_term::KeyBind>,
    pub dismiss_keys: Vec<crate::smelt_term::KeyBind>,
}

pub use well_known::{
    PROMPT_ABOVE_WIN, PROMPT_BELOW_WIN, PROMPT_EDIT_BUF, PROMPT_WIN, TRANSCRIPT_WIN,
};

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

    pub(crate) fn top_label(&self) -> Option<String> {
        self.entries.last().map(|(_, l)| l.clone())
    }

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
pub(crate) struct WellKnown {
    pub(crate) prompt: crate::smelt_term::WinId,
    pub(crate) prompt_above: crate::smelt_term::WinId,
    pub(crate) prompt_below: crate::smelt_term::WinId,
    pub(crate) transcript: crate::smelt_term::WinId,
    pub(crate) statusline: crate::smelt_term::WinId,
    pub(crate) cmdline: Option<crate::smelt_term::WinId>,
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
    pub(crate) _perf: Option<smelt_perf::perf::Guard>,
}

pub(crate) enum EventOutcome {
    Noop,
    Redraw,
    Quit,
    CancelAgent,
    /// Cancel the running agent and immediately start a new turn with the oldest queued message.
    InterruptWithQueued,
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
    /// Timestamp of the most recent Esc; used by double-Esc cancel logic in `resolve_agent_esc`.
    pub(crate) last_esc: Option<Instant>,
    pub(crate) esc_vim_mode: Option<crate::smelt_term::VimMode>,
    pub(crate) last_ctrlc: Option<Instant>,
    pub(crate) last_keypress: Option<Instant>,
    /// Pending `Ctrl-W` pane chord; next key navigates panes instead of flowing to input.
    pub(crate) pending_pane_chord: Option<Instant>,
    /// Active Lua-keymap chord sequence; `None` between chords.
    pub(crate) pending_chord: Option<PendingChord>,
}

/// State carried between keys of a multi-key chord. See [`Timers::pending_chord`].
pub(crate) struct PendingChord {
    pub(crate) tokens: Vec<String>,
    /// Wall time of the first key; chords older than [`CHORD_TIMEOUT_MS`] are discarded.
    pub(crate) started: Instant,
    /// Vim mode captured before the first key was dispatched; surfaced to chord handlers.
    pub(crate) vim_mode_at_start: Option<crate::smelt_term::VimMode>,
}

/// Idle time after which a pending chord expires and the next key starts a fresh sequence.
pub(crate) const CHORD_TIMEOUT_MS: u64 = 500;

/// Idle time after the last keypress before showing a deferred permission dialog.
pub(crate) const CONFIRM_DEFER_MS: u64 = 1500;

/// Hard cap on how many user submissions stack up while a background
/// plugin holds the spinner busy. Sensible bursts are under 10; anything
/// past this is almost certainly a hung plugin, and silently dropping
/// the overflow is preferable to unbounded memory growth.
pub(crate) const MAX_QUEUED_MESSAGES: usize = 64;

pub(crate) enum DeferredDialog {
    Confirm(Box<ConfirmRequest>),
}

pub(crate) enum SessionControl {
    Continue,
    NeedsConfirm(Box<ConfirmRequest>),
    Done,
}

pub(crate) struct PendingTool {
    pub(crate) call_id: String,
    pub(crate) name: String,
}

impl TuiApp {
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

        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();

        let app_config = config;

        let transcript_projection = crate::content::transcript_buf::TranscriptProjection::new();
        let (term_w, term_h) = terminal::size().unwrap_or((80, 24));
        let (ui, well_known) = {
            let mut ui = crate::smelt_term::Ui::new();
            ui.set_terminal_size(term_w, term_h);
            // Install the baked default theme up-front. `run()` may
            // overwrite it after detecting light/dark, and Lua's
            // `theme.use(...)` will later swap to the user's
            // colorscheme — but until then every render path sees a
            // working theme. Tests that construct TuiApp without
            // calling run() rely on this.
            *ui.theme_mut() = crate::theme::default_baked().as_ref().clone();
            let input_display_buf = ui
                .buf_create_with_id(
                    crate::app::PROMPT_EDIT_BUF,
                    crate::smelt_term::BufCreateOpts::default(),
                )
                .expect("PROMPT_EDIT_BUF slot is free");
            let parser: std::sync::Arc<dyn crate::smelt_term::BufferParser> = std::sync::Arc::new(
                crate::content::prompt_parser::PromptBufferParser::new(input.store.clone()),
            );
            let copier: std::sync::Arc<dyn crate::smelt_term::BufferCopy> = std::sync::Arc::new(
                crate::content::prompt_parser::PromptCopier::new(input.store.clone()),
            );
            if let Some(b) = ui.buf_mut(input_display_buf) {
                b.set_parser(parser);
                b.set_copier(copier);
                b.history = crate::smelt_term::UndoHistory::new(Some(100));
            }
            let transcript_display_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
            let transcript_copier: std::sync::Arc<dyn crate::smelt_term::BufferCopy> =
                std::sync::Arc::new(crate::content::transcript_buf::TranscriptCopier);
            if let Some(b) = ui.buf_mut(transcript_display_buf) {
                b.readonly = true;
                b.set_copier(transcript_copier);
            }
            assert!(ui.win_open_split_at(
                crate::app::TRANSCRIPT_WIN,
                transcript_display_buf,
                crate::smelt_term::SplitConfig {
                    region: "transcript".into(),
                    gutters: crate::smelt_term::Gutters {
                        pad_left: 0,
                        pad_right: 0,
                        scrollbar: true,
                    },
                },
            ));
            if let Some(w) = ui.win_mut(crate::app::TRANSCRIPT_WIN) {
                w.set_vim_enabled(vim_enabled);
                // Transcript blocks (code, diff) stamp `SourceLine` per row;
                // `LineNumberGutter` is strict — text/markdown rows leave no
                // stamp and contribute no gutter width.
                w.gutter = Some(std::sync::Arc::new(
                    crate::smelt_term::gutter::LineNumberGutter::new(),
                ));
                // Opt the transcript into per-frame tail-follow. Plugin leaves
                // keep the default `false`; `Ui::apply_tail_follow` ignores
                // them and they stay where the caller put them.
                w.follow_tail = true;
            }
            let prompt_above_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
            assert!(ui.win_open_split_at(
                crate::app::PROMPT_ABOVE_WIN,
                prompt_above_buf,
                crate::smelt_term::SplitConfig {
                    region: "prompt_above".into(),
                    gutters: crate::smelt_term::Gutters {
                        scrollbar: false,
                        ..Default::default()
                    },
                },
            ));
            if let Some(w) = ui.win_mut(crate::app::PROMPT_ABOVE_WIN) {
                w.focusable = false;
            }
            assert!(ui.win_open_split_at(
                crate::app::PROMPT_WIN,
                input_display_buf,
                crate::smelt_term::SplitConfig {
                    region: "prompt".into(),
                    gutters: crate::smelt_term::Gutters {
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
                // Chat input ergonomics: the prompt is for typing, so even with vim
                // enabled the first keystroke after startup should insert, not act
                // as a Normal-mode motion. Other vim-enabled leaves keep the default.
                if vim_enabled {
                    w.set_vim_mode(crate::smelt_term::VimMode::Insert);
                }
            }
            let prompt_below_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
            assert!(ui.win_open_split_at(
                crate::app::PROMPT_BELOW_WIN,
                prompt_below_buf,
                crate::smelt_term::SplitConfig {
                    region: "prompt_below".into(),
                    gutters: crate::smelt_term::Gutters {
                        scrollbar: false,
                        ..Default::default()
                    },
                },
            ));
            if let Some(w) = ui.win_mut(crate::app::PROMPT_BELOW_WIN) {
                w.focusable = false;
            }
            let status_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
            let status_win = ui
                .win_open_split(
                    status_buf,
                    crate::smelt_term::SplitConfig {
                        region: "status".into(),
                        gutters: crate::smelt_term::Gutters {
                            scrollbar: false,
                            ..Default::default()
                        },
                    },
                )
                .expect("status buffer was just created");
            if let Some(win) = ui.win_mut(status_win) {
                win.focusable = false;
            }
            // Seed a minimal splits tree so overlay anchors can resolve before the first render frame.
            ui.set_layout(crate::content::layout::build_layout_tree(
                &crate::content::layout::LayoutInput {
                    term_height: term_h,
                    prompt_above_rows: 1,
                    prompt_input_rows: 1,
                },
                status_win,
            ));
            ui.set_focus(crate::app::PROMPT_WIN);
            (
                ui,
                WellKnown {
                    prompt: crate::app::PROMPT_WIN,
                    prompt_above: crate::app::PROMPT_ABOVE_WIN,
                    prompt_below: crate::app::PROMPT_BELOW_WIN,
                    transcript: crate::app::TRANSCRIPT_WIN,
                    statusline: status_win,
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
        let (lua_wakeup_tx, lua_wakeup_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = lua.shared().wakeup_tx.set(lua_wakeup_tx);
        Self {
            core,
            lua,
            transcript: smelt_core::content::transcript::Transcript::new(),
            parser: smelt_core::content::stream_parser::StreamParser::new(),
            transcript_projection,
            input_history: History::load(),
            input,
            exec: None,
            lua_wakeup_rx,
            host_rx,
            queued_messages: Vec::new(),
            cwd,
            shared_session,
            task_label: None,
            pending_dialog: false,
            pending_quit: false,
            custom_status_items: Vec::new(),
            statusline_last_errors: HashMap::new(),
            notification: None,
            cmdline: crate::app::cmdline::CmdlineState::default(),
            picker_state: HashMap::new(),
            paint_registry: crate::lua::paint::PaintRegistry::default(),
            term_focused: true,
            working: smelt_core::working::WorkingState::new(working_clock),
            layout: crate::content::layout::LayoutState::default(),
            agent: None,
            sleep_inhibit: crate::sleep_inhibit::SleepInhibitor::new(),
            persister: crate::persist::Persister::spawn(),
            last_width: term_w,
            last_height: term_h,
            next_turn_id: 1,
            pending_turn_meta: None,
            busy_stack: BusyStack::default(),
            startup_auth_error,
            project_trust: Some(project_trust),
            app_focus: AppFocus::Prompt,
            last_prompt_text: String::new(),
            prompt_inputs: crate::prompt_inputs::PromptInputs::default(),
            auto_reload: None,
            prompt_sections: crate::prompt_sections::PromptSections::default(),
            ui,
            well_known,
            timers: Timers {
                last_esc: None,
                esc_vim_mode: None,
                last_ctrlc: None,
                last_keypress: None,
                pending_pane_chord: None,
                pending_chord: None,
            },
            pending_dialogs: VecDeque::new(),
            terminal: None,
            http_client: None,
            context_window_tx: None,
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
            self.core.config.mode,
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
            self.core.config.mode,
            true,
            self.prompt_inputs.skill_section.as_deref(),
            self.prompt_inputs.instructions.as_deref(),
        )
        .assemble()
    }

    /// Fire due timer callbacks; re-arms recurring entries and drops one-shots.
    pub(crate) fn tick_timers(&mut self) {
        let due = self.core.timers.drain_due(self.lua.lua());
        for func in due {
            let _perf = smelt_perf::perf::begin("lua:timer");
            if let Err(e) = func.call::<()>(()) {
                self.lua.record_error(format!("timer: {e}"));
            }
        }
    }

    /// Publish `vim_mode`, `confirms_pending`, `now`, `spinner_frame`,
    /// and the `work_*` family of cells whenever their values change.
    pub(crate) fn publish_diff_cells(&mut self) {
        self.core.cells.publish_if_changed(
            "vim_mode",
            self.focused_vim_mode_label().unwrap_or_default(),
        );
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

        self.publish_work_cells();
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
            "working".to_string()
        } else {
            String::new()
        };

        (state, label)
    }

    /// Pre-composed top-bar indicator state, or `None` when the
    /// indicator should not render (`Idle` with no last outcome).
    pub(crate) fn indicator_info(&self) -> Option<crate::content::prompt_buf::IndicatorInfo> {
        use crate::content::prompt_buf::IndicatorInfo;
        use smelt_core::working::WorkState;
        let (state, resolved_label) = self.resolve_work_state();
        if matches!(state, WorkState::Idle) {
            return None;
        }
        let label = if resolved_label.is_empty() {
            match state {
                WorkState::Done => "done".to_string(),
                WorkState::Interrupted => "interrupted".to_string(),
                WorkState::Paused => "paused".to_string(),
                _ => resolved_label,
            }
        } else {
            resolved_label
        };
        // Active labels get a trailing ellipsis ("working…", "compacting…")
        // so they read as in-progress even at a glance.
        let label = if matches!(
            state,
            WorkState::Working | WorkState::Retrying | WorkState::Busy
        ) && !label.is_empty()
        {
            format!("{label}\u{2026}")
        } else {
            label
        };
        let animating = matches!(
            state,
            WorkState::Working | WorkState::Retrying | WorkState::Busy
        );
        let elapsed = match state {
            WorkState::Busy => self.busy_stack.since().map(|s| s.elapsed()),
            _ => self.working.elapsed(),
        };
        let glyph = if animating {
            smelt_core::content::glyph_for(elapsed.unwrap_or_default())
        } else if matches!(state, WorkState::Paused) {
            smelt_core::content::glyph_for(std::time::Duration::ZERO)
        } else {
            ""
        };
        // Duration suppressed for `Interrupted` — the label alone reads
        // cleaner without trailing zero-or-stale seconds.
        let duration_text = if matches!(state, WorkState::Interrupted) {
            None
        } else {
            elapsed
                .filter(|d| d.as_secs() > 0)
                .map(|d| smelt_core::utils::format_duration(d.as_secs()))
        };
        let retry_text = self.working.retry_info().map(|(attempt, remaining_ms)| {
            format!("retrying in {}s #{}", remaining_ms / 1000, attempt)
        });
        let pulse_elapsed = if animating { elapsed } else { None };
        Some(IndicatorInfo {
            state,
            label,
            glyph,
            duration_text,
            retry_text,
            pulse_elapsed,
        })
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

        let elapsed_ms = self
            .working
            .elapsed()
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
        for fire in fires {
            let value = self.core.cells.project_to_lua(&*fire.value, lua);
            let prev = self.core.cells.project_to_lua(&*fire.prev, lua);
            for cb in &fire.callbacks {
                let smelt_core::cells::SubscriberKind::Lua(handle) = &cb.kind;
                let func = match lua.registry_value::<mlua::Function>(&handle.key) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let _perf = smelt_perf::perf::begin("lua:cell_cb");
                let result = if cb.is_glob {
                    func.call::<()>((fire.name.clone(), value.clone(), prev.clone()))
                } else {
                    func.call::<()>((value.clone(), prev.clone()))
                };
                if let Err(e) = result {
                    self.lua.record_error(format!("cell `{}`: {e}", fire.name));
                }
            }
        }
    }

    /// Returns the current placeholder text on `win`, if any. Stored as an
    /// extmark on the window's buffer in the well-known placeholder namespace.
    pub(crate) fn placeholder_text(&mut self, win: crate::smelt_term::WinId) -> Option<String> {
        let buf = self.ui.win_buf_mut(win)?;
        let ns = buf.create_namespace(crate::content::prompt_buf::PLACEHOLDER_NS);
        buf.extmarks(ns).into_iter().find_map(|(_, mark)| {
            if let crate::smelt_term::ExtmarkPayload::VirtText { text, .. } = &mark.payload {
                Some(text.clone())
            } else {
                None
            }
        })
    }

    /// Set the placeholder text on `win`. Replaces any prior placeholder.
    pub(crate) fn set_placeholder(&mut self, win: crate::smelt_term::WinId, text: String) {
        let Some(buf) = self.ui.win_buf_mut(win) else {
            return;
        };
        let ns = buf.create_namespace(crate::content::prompt_buf::PLACEHOLDER_NS);
        buf.clear_namespace(ns, 0, usize::MAX);
        buf.set_extmark(
            ns,
            0,
            0,
            crate::smelt_term::ExtmarkOpts::virt_text(text, Some("GhostText".into())),
        );
    }

    /// Clear the placeholder on `win` (text + opts). Idempotent.
    pub(crate) fn clear_placeholder(&mut self, win: crate::smelt_term::WinId) {
        if let Some(buf) = self.ui.win_buf_mut(win) {
            let ns = buf.create_namespace(crate::content::prompt_buf::PLACEHOLDER_NS);
            buf.clear_namespace(ns, 0, usize::MAX);
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
    /// — the same visibility rule that gates rendering.
    pub(crate) fn dispatch_placeholder_key(
        &mut self,
        win: crate::smelt_term::WinId,
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
        let kb = crate::smelt_term::KeyBind::new(code, mods);
        if opts.accept_keys.contains(&kb) {
            self.clear_placeholder(win);
            if win == self.well_known.prompt {
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.input.replace_text(&mut pctx, text.clone());
            }
            self.fire_placeholder_event(
                win,
                crate::smelt_term::WinEvent::PlaceholderAccepted,
                text,
            );
            return Some(EventOutcome::Redraw);
        }
        if opts.dismiss_keys.contains(&kb) {
            self.clear_placeholder(win);
            self.fire_placeholder_event(
                win,
                crate::smelt_term::WinEvent::PlaceholderDismissed,
                text,
            );
            return Some(EventOutcome::Redraw);
        }
        None
    }

    fn fire_placeholder_event(
        &mut self,
        win: crate::smelt_term::WinId,
        event: crate::smelt_term::WinEvent,
        text: String,
    ) {
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_term::LuaHandle,
                              w: crate::smelt_term::WinId,
                              payload: &crate::smelt_term::Payload| {
            lua.queue_invocation(handle, w, payload);
        };
        self.ui.fire_win_event(
            win,
            event,
            crate::smelt_term::Payload::Text { content: text },
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    /// Gutters configured on the transcript window (single source of truth: `Window.config.gutters`).
    pub(crate) fn transcript_gutters(&self) -> crate::smelt_term::Gutters {
        self.ui
            .win(crate::app::TRANSCRIPT_WIN)
            .map(|w| w.config.gutters)
            .unwrap_or_default()
    }

    pub(crate) fn transcript_win(&self) -> &crate::smelt_term::Window {
        self.ui
            .win(self.well_known.transcript)
            .expect("transcript window")
    }

    pub(crate) fn transcript_win_mut(&mut self) -> &mut crate::smelt_term::Window {
        self.ui
            .win_mut(self.well_known.transcript)
            .expect("transcript window")
    }

    pub(crate) fn prompt_buf(&self) -> &crate::smelt_term::Buffer {
        self.ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .expect("prompt edit buffer")
    }

    pub(crate) fn prompt_win(&self) -> &crate::smelt_term::Window {
        self.ui.win(crate::app::PROMPT_WIN).expect("prompt window")
    }

    pub(crate) fn prompt_win_mut(&mut self) -> &mut crate::smelt_term::Window {
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
        let win = crate::smelt_term::WinId(raw_id);
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
        let paint_id = crate::smelt_term::layout::PaintId(raw_id);
        if self.paint_registry.contains(paint_id) {
            return Some(crate::lua::paint::LeafKind::Paint(paint_id));
        }
        None
    }

    pub(crate) fn notify(&mut self, message: String) {
        self.open_notification(message, false);
    }

    pub(crate) fn notify_error(&mut self, message: String) {
        self.open_notification(message, true);
    }

    fn open_notification(&mut self, message: String, is_error: bool) {
        if let Some(win) = self.notification.take() {
            self.close_overlay_leaf(win);
        }

        let label = if is_error { "error" } else { "info" };
        let indent = " ";
        let gap = "  ";
        let line = format!("{indent}{label}{gap}{message}");

        let buf = self
            .ui
            .buf_create(crate::smelt_term::BufCreateOpts::default());

        let label_start = indent.len() as u16;
        let label_end = label_start + label.len() as u16;
        let msg_start = label_end + gap.len() as u16;
        let msg_end = msg_start + message.chars().count() as u16;

        let label_color = if is_error {
            self.ui.theme().get("ErrorMsg").fg
        } else {
            None
        };
        if let Some(b) = self.ui.buf_mut(buf) {
            b.set_all_lines(vec![line]);
            b.add_highlight(
                0,
                label_start,
                label_end,
                crate::smelt_term::SpanStyle {
                    fg: label_color,
                    bold: true,
                    ..Default::default()
                },
            );
            b.add_highlight(
                0,
                msg_start,
                msg_end,
                crate::smelt_term::SpanStyle {
                    dim: true,
                    ..Default::default()
                },
            );
        }

        let Some(win) = self.ui.win_open_split(
            buf,
            crate::smelt_term::SplitConfig {
                region: "notification".into(),
                gutters: Default::default(),
            },
        ) else {
            return;
        };
        if let Some(w) = self.ui.win_mut(win) {
            // Non-focusable surface: text-selectable for drag-copy, but the
            // caret-leaf predicate (focusable && !mouse_scroll) keeps `cpos`
            // untouched on click — notification rows have no meaningful caret.
            w.focusable = false;
            w.selectable = true;
        }

        let layout = crate::smelt_term::LayoutTree::vbox(vec![(
            crate::smelt_term::Constraint::Length(1),
            crate::smelt_term::LayoutTree::hbox(vec![(
                crate::smelt_term::Constraint::Percentage(100),
                crate::smelt_term::LayoutTree::leaf(win),
            )]),
        )]);
        let _overlay_id = self.ui.overlay_open(
            crate::smelt_term::Overlay::new(
                layout,
                crate::smelt_term::layout::Anchor::Win {
                    target: crate::app::PROMPT_ABOVE_WIN.into(),
                    attach: crate::smelt_term::Align::NW,
                    row_offset: -1,
                    col_offset: 0,
                },
            )
            // Sits below dialogs (default overlay z 50) so a toast
            // never obscures a modal asking for input.
            // Sits below dialogs (z 50) so a toast never obscures a modal.
            .with_z(40),
        );
        self.notification = Some(win);
    }

    pub(crate) fn dismiss_notification(&mut self) {
        if let Some(win) = self.notification.take() {
            self.close_overlay_leaf(win);
        }
    }

    pub(crate) fn set_task_label(&mut self, label: String) {
        self.task_label = if label.trim().is_empty() {
            None
        } else {
            Some(label)
        };
    }

    pub async fn run(&mut self, http_client: engine::HttpClient, initial_message: Option<String>) {
        let (ctx_tx, mut ctx_rx) = tokio::sync::mpsc::unbounded_channel::<Option<u32>>();
        self.http_client = Some(http_client);
        self.context_window_tx = Some(ctx_tx);
        self.refresh_context_window();
        crate::theme::detect_background(self.ui.theme_mut());
        // Install the baked default theme so the first frame renders with
        // real colors before Lua's `theme.use(...)` runs during bootstrap.
        // Lua-side colorschemes overwrite this via `smelt.theme.apply`.
        let mut baked = crate::theme::default_baked().as_ref().clone();
        baked.set_light(self.ui.theme().is_light());
        *self.ui.theme_mut() = baked;
        // Publish to the process-wide active theme slot so the diff
        // renderer (which can't reach the app context from a worker
        // thread) reads the right colors.
        smelt_core::theme::set_active(self.ui.theme().clone());
        // Capture the thread-safe Lua command-name set directly. Going through
        // `try_with_app` would only work on the main thread (APP is a thread-
        // local), and `layout_block_into` runs in worker threads via
        // `std::thread::scope`, so the resolver must reach the registry
        // without consulting APP. `commands` itself can't cross threads (the
        // handler holds a `LuaHandle`), so this uses the name-only mirror.
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

        if !self.core.session.messages.is_empty() {
            self.restore_screen();
            if let Some(ref slug) = self.core.session.slug {
                self.set_task_label(slug.clone());
            }
            self.finish_transcript_turn();
            self.transcript_win_mut().scroll_to_bottom();
        }
        if let Some(message) = self.startup_auth_error.take() {
            self.notify_error(message);
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

        let mut term_events = EventStream::new();
        // Independent SIGWINCH listener: crossterm's signal source intermittently drops
        // resize events (signal-hook-mio counter / mio readiness race), so we keep our
        // own tokio-native handler. Both fire on resize; the duplicate just hits an
        // idempotent `compositor.resize` and one extra full repaint.
        let mut sigwinch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                .expect("install SIGWINCH listener");

        // Cold-start the Lua context through the same pipeline `/reload`
        // uses. `main` already ran a pre-TUI plugin pass to extract
        // engine config — that pass couldn't touch `smelt.win`,
        // `smelt.overlay`, `smelt.paint`, `smelt.cell:subscribe`, etc.
        // because the host pointer wasn't installed yet. Re-running
        // here inside `install_app_ptr` makes the host live for module
        // bodies on every Lua-context init (cold start AND `/reload`),
        // so plain `if persist().is_open then open() end` at module
        // top works in both. `lifecycle.on("ready")` hooks drain at
        // the end with `ctx.kind = "launch"`.
        let load_err = crate::lua::with_app_ptr(self, |app| app.bring_up_lua("launch"));
        if let Some(err) = load_err {
            self.notify_error(format!("lua init: {err}"));
        }

        // Auto-submit initial message if provided (e.g. `agent "fix the bug"`).
        if let Some(msg) = initial_message {
            let trimmed = msg.trim();
            if let Some(cmd) = trimmed.strip_prefix('!') {
                if let Some(handle) = self.start_shell_escape(cmd) {
                    self.exec = Some(handle);
                }
            } else if trimmed.starts_with('/') && smelt_core::commands::is_command(trimmed) {
                let name = trimmed
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
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
                self.discard_turn(true);
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
            {
                let lua = &self.lua;
                let mut lua_invoke =
                    |handle: crate::smelt_term::LuaHandle,
                     win: crate::smelt_term::WinId,
                     payload: &crate::smelt_term::Payload| {
                        lua.queue_invocation(handle, win, payload);
                    };
                self.ui.dispatch_tick(&mut lua_invoke);
                self.ui.dispatch_scroll_events(&mut lua_invoke);
                self.ui.dispatch_resize_events(&mut lua_invoke);
            }
            self.flush_lua_callbacks();

            while let Ok(result) = ctx_rx.try_recv() {
                self.core.config.context_window = result;
            }

            self.drain_host_calls();

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
                        self.discard_turn(false);
                        break;
                    }
                };
                let action = if let Some(mut ag) = self.agent.take() {
                    let ctrl = self.handle_engine_event(ev, ag.turn_id, &mut ag.pending);
                    let action = self.dispatch_control(ctrl, &ag.pending);
                    self.agent = Some(ag);
                    action
                } else {
                    // No active turn — handle out-of-band events.
                    self.handle_idle_engine_event(ev);
                    true
                };
                if !action {
                    self.discard_turn(false);
                    break;
                }
            }

            if self.agent.is_none()
                && !self.queued_messages.is_empty()
                && !self.busy_stack.is_busy()
            {
                let text = self.queued_messages.remove(0);
                if !text.is_empty() {
                    let outcome = self.process_input(&text);
                    let content = Content::text(text.clone());
                    self.apply_input_outcome(outcome, content, &text);
                }
            }

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
                    let taken = self.agent.take();
                    let pending_ref: &[PendingTool] =
                        taken.as_ref().map(|a| a.pending.as_slice()).unwrap_or(&[]);
                    let action = self.dispatch_control(ctrl, pending_ref);
                    self.agent = taken;
                    if !action {
                        self.discard_turn(false);
                    }
                }
                self.pending_dialog = !self.pending_dialogs.is_empty();
            }

            // Recompute statusline after engine events drain so a turn ending mid-iteration
            // (TurnComplete) flips the spinner pill to "done" in the same frame, instead of
            // showing stale items until the next input event.
            let (items, tick_errors) = self.lua.tick_statusline(self.ui.theme());
            self.custom_status_items = items;
            for (name, msg) in tick_errors {
                match msg {
                    Some(new_msg) => {
                        if self.statusline_last_errors.get(&name) != Some(&new_msg) {
                            self.notify_error(new_msg.clone());
                            self.statusline_last_errors.insert(name, new_msg);
                        }
                    }
                    None => {
                        self.statusline_last_errors.remove(&name);
                    }
                }
            }

            self.render_normal(self.agent.is_some());
            let last_frame = self.core.clock.instant_now();

            let now = self.core.clock.instant_now();
            let yank_flash_active = self
                .core
                .clipboard
                .kill_ring
                .yank_flash_until()
                .is_some_and(|t| t > now);
            let drag_active = self.ui.drag_capture_window().is_some();
            let has_animation = self.ui.focused_overlay().is_some()
                || self.has_active_exec()
                || self.working.is_animating()
                || self.busy_stack.is_busy()
                || yank_flash_active
                || drag_active;
            let next_timer_delay = self
                .core
                .timers
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(now));

            tokio::select! {
                biased;

                Some(Ok(ev)) = stream_next(&mut term_events) => {
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

                    while event::poll(Duration::ZERO).unwrap_or(false) {
                        if let Ok(ev) = event::read() {
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
                    }

                    if scroll_delta != 0 {
                        let _ = self.ui.scroll_at(scroll_row, scroll_col, scroll_delta);
                    }

                    self.render_normal(self.agent.is_some());
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
                    self.render_normal(self.agent.is_some());
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
                    if self.agent.is_some() || self.ui.active_modal().is_some() {
                        // Defer: re-arm on the next debounced batch.
                        continue;
                    }
                    self.reload_lua();
                    self.render_normal(self.agent.is_some());
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
                    self.ui.tick_drag_autoscroll();
                    self.render_normal(self.agent.is_some());
                }

                _ = tokio::time::sleep(next_timer_delay.unwrap_or(Duration::MAX)), if next_timer_delay.is_some() => {
                    self.tick_timers();
                    self.drive_lua_tasks();
                    self.render_normal(self.agent.is_some());
                }

                Some(_) = sigwinch.recv() => {
                    if let Ok((w, h)) = terminal::size() {
                        if w != self.last_width || h != self.last_height {
                            self.handle_resize(w, h);
                            self.render_normal(self.agent.is_some());
                        }
                    }
                }
            }
        }

        crate::lua::with_app_ptr(self, |app| {
            if app.agent.is_some() {
                app.finish_turn(true);
            }
            app.core
                .cells
                .set_dyn("shutdown", std::rc::Rc::new(smelt_core::cells::EventStub));
            app.drain_cells_pending();
            app.save_session();
        });

        // Drop the terminal guard last so any rendering above stays in TUI mode.
        self.terminal = None;
    }
}

/// Poll one item from a `futures_core::Stream`, equivalent to `StreamExt::next`.
async fn stream_next<S>(stream: &mut S) -> Option<S::Item>
where
    S: futures_core::Stream + Unpin,
{
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}
