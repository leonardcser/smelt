pub(crate) mod agent;
pub(crate) mod cmdline;
pub(crate) mod content_keys;
pub(crate) mod engine_events;
pub(crate) mod events;
pub(crate) mod history;
pub(crate) mod lua_bridge;
pub(crate) mod lua_handlers;
pub(crate) mod mouse;
pub(crate) mod pane_focus;
pub(crate) mod render_loop;
pub(crate) mod status_bar;
pub(crate) mod transcript;
pub(crate) mod ui_host;
pub(crate) mod well_known;

use crate::input::PromptState;
use crate::state;
use engine::EngineHandle;
use protocol::Content;
use smelt_core::history::History;
use smelt_core::session::Session;
use smelt_core::ConfirmRequest;
use smelt_core::FrontendKind;
use std::sync::Arc;

use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, EventStream,
    },
    terminal::{self, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use std::collections::{HashMap, VecDeque};
use std::io;

use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct TuiApp {
    pub core: smelt_core::Core,
    pub lua: crate::lua::LuaRuntime,
    pub(crate) transcript: smelt_core::content::transcript::Transcript,
    pub(crate) parser: smelt_core::content::stream_parser::StreamParser,
    pub(crate) transcript_projection: crate::content::transcript_buf::TranscriptProjection,
    /// Plain-text snapshot of visible rows captured during `project_transcript_buffer`.
    /// Read by `compute_transcript_cursor` to look up the glyph under the soft cursor.
    pub(crate) last_viewport_text: Vec<String>,
    pub(crate) input_history: History,
    pub(crate) input: PromptState,
    pub(crate) exec: Option<crate::commands::ExecHandle>,
    /// Wakeup from cross-thread tasks that pushed to the Lua inbox. Drains the inbox so parked coroutines resume.
    lua_wakeup_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
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
    pub(crate) transcript_gutters: crate::window::WindowGutters,
    /// Viewport layout updated each frame; read by mouse hit-testing and scroll estimation.
    pub(crate) layout: crate::content::layout::LayoutState,

    /// Owned here so reducer handlers (`apply_ops`) can mutate it directly
    /// instead of threading `&mut Option<TurnState>` through every call.
    pub(crate) agent: Option<TurnState>,
    pub(crate) sleep_inhibit: crate::sleep_inhibit::SleepInhibitor,
    pub(crate) persister: crate::persist::Persister,
    pub(crate) pending_title: bool,
    pub(crate) last_width: u16,
    pub(crate) last_height: u16,
    pub(crate) next_turn_id: u64,
    /// Incremented on rewind/clear/load; invalidates in-flight compactions.
    pub(crate) compact_epoch: u64,
    pub(crate) pending_compact_epoch: u64,
    pub(crate) pending_turn_meta: Option<protocol::TurnMeta>,
    startup_auth_error: Option<String>,
    /// Trust state for `<cwd>/.smelt/`; surfaced as a startup toast then dropped.
    pub(crate) project_trust: Option<smelt_core::trust::TrustState>,
    pub(crate) app_focus: AppFocus,
    pub(crate) transcript_window: crate::smelt_term::Window,
    /// Tracks the last text dispatched as `TextChanged` on `PROMPT_WIN`.
    pub(crate) last_prompt_text: String,
    /// Vim mode captured at drag-start; restored on mouse-up.
    pub(crate) prompt_drag_return_vim_mode: Option<crate::smelt_term::VimMode>,
    /// Single global vim mode — authoritative source for status bar, Lua, and dispatch.
    pub(crate) vim_mode: crate::smelt_term::VimMode,
    pub extra_instructions: Option<String>,
    pub skill_section: Option<String>,
    pub(crate) prompt_sections: crate::prompt_sections::PromptSections,
    pub ui: crate::smelt_term::Ui,
    pub(crate) well_known: WellKnown,
}

pub use well_known::{
    PROMPT_ABOVE_WIN, PROMPT_BELOW_WIN, PROMPT_EDIT_BUF, PROMPT_WIN, TRANSCRIPT_WIN,
};

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
pub(crate) enum AppFocus {
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
        model: String,
        api_base: String,
        api_key_env: String,
        provider_type: String,
        permissions: Arc<smelt_core::permissions::Permissions>,
        engine: EngineHandle,
        settings: smelt_core::config::ResolvedSettings,
        reasoning_effort: protocol::ReasoningEffort,
        reasoning_cycle: Vec<protocol::ReasoningEffort>,
        mode_cycle: Vec<protocol::AgentMode>,
        shared_session: Arc<Mutex<Option<Session>>>,
        available_models: Vec<smelt_core::config::ResolvedModel>,
        cli_model_override: bool,
        cli_api_base_override: bool,
        cli_api_key_env_override: bool,
        startup_auth_error: Option<String>,
        lua: crate::lua::LuaRuntime,
        project_trust: smelt_core::trust::TrustState,
        cache: state::SessionCache,
    ) -> Self {
        let mode = cache.mode();
        let mut input = PromptState::new();
        let vim_enabled = settings.vim;
        if vim_enabled {
            input.set_vim_enabled(true);
        }
        input.command_arg_sources = Vec::new();
        let reasoning_effort = if reasoning_effort == protocol::ReasoningEffort::Off
            && cache.reasoning_effort != protocol::ReasoningEffort::Off
        {
            cache.reasoning_effort
        } else {
            reasoning_effort
        };

        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();

        let app_config = smelt_core::AppConfig {
            model,
            api_base,
            api_key_env,
            provider_type,
            available_models,
            model_config: engine::ModelConfig::default(),
            cli_model_override,
            cli_api_base_override,
            cli_api_key_env_override,
            mode,
            mode_cycle,
            reasoning_effort,
            reasoning_cycle,
            settings,
            context_window: None,
        };

        let (ui, transcript_display_buf, well_known) = {
            let (w, h) = terminal::size().unwrap_or((80, 24));
            let mut ui = crate::smelt_term::Ui::new();
            ui.set_terminal_size(w, h);
            let input_display_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
            let transcript_display_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
            assert!(ui.win_open_split_at(
                crate::app::TRANSCRIPT_WIN,
                transcript_display_buf,
                crate::smelt_term::SplitConfig {
                    region: "transcript".into(),
                    gutters: crate::smelt_term::Gutters::default(),
                },
            ));
            let prompt_above_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
            assert!(ui.win_open_split_at(
                crate::app::PROMPT_ABOVE_WIN,
                prompt_above_buf,
                crate::smelt_term::SplitConfig {
                    region: "prompt_above".into(),
                    gutters: crate::smelt_term::Gutters::default(),
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
                        pad_left: 1,
                        pad_right: 1,
                        scrollbar: false,
                    },
                },
            ));
            let prompt_below_buf = ui.buf_create(crate::smelt_term::BufCreateOpts::default());
            assert!(ui.win_open_split_at(
                crate::app::PROMPT_BELOW_WIN,
                prompt_below_buf,
                crate::smelt_term::SplitConfig {
                    region: "prompt_below".into(),
                    gutters: crate::smelt_term::Gutters::default(),
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
                        gutters: crate::smelt_term::Gutters::default(),
                    },
                )
                .expect("status buffer was just created");
            if let Some(win) = ui.win_mut(status_win) {
                win.focusable = false;
            }
            // Seed a minimal splits tree so overlay anchors can resolve before the first render frame.
            ui.set_layout(crate::content::layout::build_layout_tree(
                &crate::content::layout::LayoutInput {
                    term_height: h,
                    prompt_above_rows: 1,
                    prompt_input_rows: 1,
                },
                status_win,
            ));
            ui.set_focus(crate::app::PROMPT_WIN);
            (
                ui,
                transcript_display_buf,
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

        let core = smelt_core::Core::new(app_config, engine, FrontendKind::Tui, permissions);
        let (lua_wakeup_tx, lua_wakeup_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = lua.shared().wakeup_tx.set(lua_wakeup_tx);
        Self {
            core,
            lua,
            transcript: smelt_core::content::transcript::Transcript::new(),
            parser: smelt_core::content::stream_parser::StreamParser::new(),
            transcript_projection: crate::content::transcript_buf::TranscriptProjection::new(),
            last_viewport_text: Vec::new(),
            input_history: History::load(),
            input,
            exec: None,
            lua_wakeup_rx,
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
            working: smelt_core::working::WorkingState::new(),
            transcript_gutters: crate::window::TRANSCRIPT_GUTTERS,
            layout: crate::content::layout::LayoutState::default(),
            agent: None,
            sleep_inhibit: crate::sleep_inhibit::SleepInhibitor::new(),
            persister: crate::persist::Persister::spawn(),
            pending_title: false,
            last_width: terminal::size().map(|(w, _)| w).unwrap_or(80),
            last_height: terminal::size().map(|(_, h)| h).unwrap_or(24),
            next_turn_id: 1,
            compact_epoch: 0,
            pending_compact_epoch: 0,
            pending_turn_meta: None,
            startup_auth_error,
            project_trust: Some(project_trust),
            app_focus: AppFocus::Prompt,
            transcript_window: {
                let mut w = crate::smelt_term::Window::new(
                    crate::app::TRANSCRIPT_WIN,
                    transcript_display_buf,
                    crate::smelt_term::SplitConfig {
                        region: "transcript".into(),
                        gutters: crate::smelt_term::Gutters::default(),
                    },
                );
                w.set_vim_enabled(vim_enabled);
                w
            },
            last_prompt_text: String::new(),
            prompt_drag_return_vim_mode: None,
            vim_mode: crate::smelt_term::VimMode::Insert,
            extra_instructions: None,
            skill_section: None,
            prompt_sections: crate::prompt_sections::PromptSections::default(),
            ui,
            well_known,
        }
    }

    /// Rebuilds prompt sections from current app state and returns the assembled system prompt.
    pub(crate) fn rebuild_system_prompt(&mut self) -> String {
        let cwd = std::path::Path::new(&self.cwd);
        self.prompt_sections = crate::prompt_sections::build_defaults(
            cwd,
            self.core.config.mode,
            true, // TUI is always interactive
            self.skill_section.as_deref(),
            self.extra_instructions.as_deref(),
        );
        self.prompt_sections.assemble()
    }

    /// Fire due timer callbacks; re-arms recurring entries and drops one-shots.
    pub(crate) fn tick_timers(&mut self) {
        let now = std::time::Instant::now();
        let due = self.core.timers.drain_due(now, self.lua.lua());
        for func in due {
            let _perf = smelt_perf::perf::begin("lua:timer");
            if let Err(e) = func.call::<()>(()) {
                self.lua.record_error(format!("timer: {e}"));
            }
        }
    }

    /// Publish `vim_mode`, `confirms_pending`, `now`, and `spinner_frame` cells whenever their values change.
    pub(crate) fn publish_diff_cells(&mut self) {
        self.core
            .cells
            .publish_if_changed("vim_mode", format!("{:?}", self.vim_mode));
        self.core
            .cells
            .publish_if_changed("confirms_pending", !self.core.confirms.is_clear());
        let now_secs = std::time::SystemTime::now()
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

    /// Returns the current ghost-text prediction from the prompt buffer's completer extmark.
    pub(crate) fn prompt_completer_text(&mut self) -> Option<String> {
        let buf = self
            .ui
            .win_buf_mut(self.well_known.prompt)
            .expect("prompt window registered at startup");
        let ns = buf.create_namespace(crate::content::prompt_buf::COMPLETER_NS);
        buf.extmarks(ns).into_iter().find_map(|(_, mark)| {
            if let crate::smelt_term::ExtmarkPayload::VirtText { text, .. } = &mark.payload {
                Some(text.clone())
            } else {
                None
            }
        })
    }

    /// Replaces the prompt buffer's ghost-text prediction extmark.
    pub(crate) fn set_prompt_completer(&mut self, text: String) {
        let buf = self
            .ui
            .win_buf_mut(self.well_known.prompt)
            .expect("prompt window registered at startup");
        let ns = buf.create_namespace(crate::content::prompt_buf::COMPLETER_NS);
        buf.clear_namespace(ns, 0, usize::MAX);
        buf.set_extmark(
            ns,
            0,
            0,
            crate::smelt_term::ExtmarkOpts::virt_text(text, Some("GhostText".into())),
        );
    }

    pub(crate) fn clear_prompt_completer(&mut self) {
        let buf = self
            .ui
            .win_buf_mut(self.well_known.prompt)
            .expect("prompt window registered at startup");
        let ns = buf.create_namespace(crate::content::prompt_buf::COMPLETER_NS);
        buf.clear_namespace(ns, 0, usize::MAX);
    }

    pub(crate) fn take_prompt_completer(&mut self) -> Option<String> {
        let text = self.prompt_completer_text();
        if text.is_some() {
            self.clear_prompt_completer();
        }
        text
    }

    /// Width available for transcript content (terminal width minus gutter/scrollbar columns).
    pub(crate) fn transcript_width(&self) -> usize {
        let (w, _) = self.ui.terminal_size();
        (self.transcript_gutters.content_width(w) as usize).max(1)
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
            w.focusable = false;
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
                    target: crate::app::PROMPT_WIN.into(),
                    attach: crate::smelt_term::Corner::NW,
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

    pub async fn run(
        &mut self,
        mut ctx_rx: Option<tokio::sync::oneshot::Receiver<Option<u32>>>,
        initial_message: Option<String>,
    ) {
        crate::theme::detect_background(self.ui.theme_mut());
        crate::theme::populate_ui_theme(self.ui.theme_mut());
        terminal::enable_raw_mode().ok();
        let _ = io::stdout().execute(EnterAlternateScreen);
        // Disable DECAWM — writing to the bottom-right cell must not trigger auto-scroll.
        let _ = io::stdout().execute(DisableLineWrap);
        let _ = io::stdout().execute(cursor::Hide);
        let _ = io::stdout().execute(EnableBracketedPaste);
        let _ = io::stdout().execute(EnableFocusChange);
        let _ = io::stdout().execute(EnableMouseCapture);

        if !self.core.session.messages.is_empty() {
            self.restore_screen();
            if let Some(ref slug) = self.core.session.slug {
                self.set_task_label(slug.clone());
            }
            self.finish_transcript_turn();
            self.transcript_window.scroll_to_bottom();
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
        if let Some(err) = self.lua.take_load_error() {
            self.notify_error(format!("lua init: {err}"));
        }
        if let Some(state) = self.project_trust.take() {
            if matches!(state, smelt_core::trust::TrustState::Untrusted { .. }) {
                self.notify(
                    "project .smelt/ content not trusted; run /trust to load it".to_string(),
                );
            }
        }
        self.flush_lua_callbacks();
        self.input.command_arg_sources = self.lua.list_command_args();

        let mut term_events = EventStream::new();

        // Auto-submit initial message if provided (e.g. `agent "fix the bug"`).
        if let Some(msg) = initial_message {
            let trimmed = msg.trim();
            if let Some(cmd) = trimmed.strip_prefix('!') {
                if let Some(handle) = self.start_shell_escape(cmd) {
                    self.exec = Some(handle);
                }
            } else if trimmed.starts_with('/') && crate::completer::Completer::is_command(trimmed) {
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

        let mut t = Timers {
            last_esc: None,
            esc_vim_mode: None,
            last_ctrlc: None,
            last_keypress: None,
            pending_pane_chord: None,
            pending_chord: None,
        };
        let mut pending_dialogs: VecDeque<DeferredDialog> = VecDeque::new();
        const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);

        'main: loop {
            if self.pending_quit {
                self.discard_turn(true);
                break 'main;
            }
            let _app_guard = crate::lua::install_app_ptr(self);
            self.tick_timers();
            self.publish_diff_cells();
            self.drain_cells_pending();
            self.drive_lua_tasks();
            let (items, tick_errors) = self.lua.tick_statusline();
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
            }
            self.flush_lua_callbacks();

            if let Some(ref mut rx) = ctx_rx {
                if let Ok(result) = rx.try_recv() {
                    self.core.config.context_window = result;
                    ctx_rx = None;
                }
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
                        self.discard_turn(false);
                        break;
                    }
                };
                let action = if let Some(mut ag) = self.agent.take() {
                    let ctrl = self.handle_engine_event(ev, ag.turn_id, &mut ag.pending);
                    let action = self.dispatch_control(
                        ctrl,
                        &ag.pending,
                        &mut pending_dialogs,
                        t.last_keypress,
                    );
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

            if self.agent.is_none() && !self.queued_messages.is_empty() && !self.is_compacting() {
                let text = self.queued_messages.remove(0);
                if !text.is_empty() {
                    let outcome = self.process_input(&text);
                    let content = Content::text(text.clone());
                    self.apply_input_outcome(outcome, content, &text);
                }
            }

            if self.agent.is_none() && !pending_dialogs.is_empty() {
                pending_dialogs.clear();
                self.pending_dialog = false;
            }
            if !pending_dialogs.is_empty()
                && !self.focused_overlay_blocks_agent()
                && self.agent.is_some()
            {
                let idle = t
                    .last_keypress
                    .map(|lk| lk.elapsed() >= Duration::from_millis(CONFIRM_DEFER_MS))
                    .unwrap_or(true);
                while idle
                    && !pending_dialogs.is_empty()
                    && !self.focused_overlay_blocks_agent()
                    && self.agent.is_some()
                {
                    let deferred = pending_dialogs.pop_front().unwrap();
                    let ctrl = match deferred {
                        DeferredDialog::Confirm(req) => SessionControl::NeedsConfirm(req),
                    };
                    let taken = self.agent.take();
                    let pending_ref: &[PendingTool] =
                        taken.as_ref().map(|a| a.pending.as_slice()).unwrap_or(&[]);
                    let action = self.dispatch_control(
                        ctrl,
                        pending_ref,
                        &mut pending_dialogs,
                        t.last_keypress,
                    );
                    self.agent = taken;
                    if !action {
                        self.discard_turn(false);
                    }
                }
                self.pending_dialog = !pending_dialogs.is_empty();
            }

            self.render_normal(self.agent.is_some());
            let last_frame = Instant::now();

            let now = Instant::now();
            let yank_flash_active = self
                .core
                .clipboard
                .kill_ring
                .yank_flash_until()
                .is_some_and(|t| t > now);
            let has_animation = self.ui.focused_overlay().is_some()
                || self.has_active_exec()
                || self.working.is_animating()
                || yank_flash_active;

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
                        if self.dispatch_terminal_event(ev, &mut t) {
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
                                if self.dispatch_terminal_event(ev, &mut t) {
                                    break 'main;
                                }
                            }
                        }
                    }

                    if scroll_delta != 0 {
                        self.scroll_under_mouse(scroll_row, scroll_col, scroll_delta);
                    }

                    self.render_normal(self.agent.is_some());
                }

                Some(ev) = self.core.engine.recv() => {
                    if let Some(mut ag) = self.agent.take() {
                        let ctrl =
                            self.handle_engine_event(ev, ag.turn_id, &mut ag.pending);
                        let action = self.dispatch_control(
                            ctrl,
                            &ag.pending,
                            &mut pending_dialogs,
                            t.last_keypress,
                        );
                        self.agent = Some(ag);
                        if !action {
                            self.discard_turn(false);
                        }
                    } else {
                        self.handle_idle_engine_event(ev);
                    }
                }

                Some(_) = self.lua_wakeup_rx.recv() => {
                    while self.lua_wakeup_rx.try_recv().is_ok() {}
                    self.flush_lua_callbacks();
                    self.drive_lua_tasks();
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
                    let want = if let Some(started) = self.ui.drag_autoscroll_started() {
                        let held = started.elapsed().as_millis() as u64;
                        // Start at ~33 lines/sec (30 ms), ramp to ~200 lines/sec (5 ms).
                        let ms = 30u64.saturating_sub(held / 120).max(5);
                        Duration::from_millis(ms)
                    } else {
                        MIN_FRAME_INTERVAL
                    };
                    want.saturating_sub(since)
                }), if has_animation || self.ui.drag_autoscroll_started().is_some() => {
                    self.tick_drag_autoscroll();
                    self.render_normal(self.agent.is_some());
                }
            }
        }

        if self.agent.is_some() {
            self.finish_turn(true);
        }
        self.core
            .cells
            .set_dyn("shutdown", std::rc::Rc::new(smelt_core::cells::EventStub));
        self.drain_cells_pending();
        self.save_session();

        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = io::stdout().execute(EnableLineWrap);
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(cursor::Show);
        let _ = io::stdout().execute(DisableBracketedPaste);
        let _ = io::stdout().execute(DisableFocusChange);
        terminal::disable_raw_mode().ok();
    }
}

/// Poll one item from a `futures_core::Stream`, equivalent to `StreamExt::next`.
async fn stream_next<S>(stream: &mut S) -> Option<S::Item>
where
    S: futures_core::Stream + Unpin,
{
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}
