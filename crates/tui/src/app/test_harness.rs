//! End-to-end test harness for `TuiApp`.
//!
//! Input is a `SourceEvent` stream (Term / Engine / Tick); output is a
//! structured `Action` log plus snapshots of inspectable state. The
//! input/output shape is the same one the eventual fuzz target will
//! use (see `FUZZING_PLAN.md`), so suites written against this harness
//! survive when the DST architecture lands.
//!
//! Side effects are contained by pointing every `$HOME`/XDG path at a
//! process-wide tempdir.
//!
//! Several helpers (`feed`, `inject_engine`, `actions`, `clear_actions`,
//! `Tick`, `Action::Quit`, etc.) are intentionally part of the public
//! shape even before any suite consumes them — they're the load-bearing
//! seams future suites and the eventual fuzz target plug into.

#![allow(dead_code)]

use crate::app::{AppFocus, TuiApp};
use crate::smelt_term::{OverlayId, VimMode, WinId};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use engine::EngineHandle;
use protocol::{AgentMode, EngineEvent, ReasoningEffort, UiCommand};
use std::sync::{Arc, Mutex, OnceLock};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// One unit of input to the TestApp loop.
#[derive(Debug, Clone)]
pub(crate) enum SourceEvent {
    Term(Event),
    Engine(EngineEvent),
    /// Advance virtual time by N milliseconds. Today's TuiApp reads
    /// `Instant::now()` directly so this is a placeholder until Phase 1
    /// of `FUZZING_PLAN.md` lands; included so the wire-format is stable.
    #[allow(dead_code)]
    Tick(u64),
}

/// One observed out-bound effect of a `SourceEvent`.
#[derive(Debug, Clone)]
pub(crate) enum Action {
    /// A `UiCommand` was sent on the engine channel.
    EngineSend(UiCommand),
    /// The event dispatch asked the app to quit.
    Quit,
}

/// Immutable snapshot of state observable by tests.
#[derive(Debug, Clone)]
pub(crate) struct AppSnapshot {
    pub(crate) app_focus: AppFocus,
    pub(crate) vim_mode: VimMode,
    pub(crate) cmdline_open: bool,
    pub(crate) cmdline_text: String,
    pub(crate) focused_overlay: Option<OverlayId>,
    pub(crate) prompt_text: String,
    pub(crate) queued_messages: Vec<String>,
    pub(crate) agent_running: bool,
    pub(crate) term_focused: bool,
    pub(crate) quit_requested: bool,
    pub(crate) notification: Option<WinId>,
    pub(crate) pending_quit: bool,
}

/// Test driver around a real `TuiApp`.
pub(crate) struct TestApp {
    pub(crate) app: TuiApp,
    cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    actions: Vec<Action>,
    quit: bool,
}

pub(crate) struct TestAppBuilder {
    vim: bool,
    mode: AgentMode,
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self {
            vim: false,
            mode: AgentMode::Normal,
        }
    }
}

impl TestAppBuilder {
    /// Enable vim-mode on the prompt window.
    pub(crate) fn with_vim(mut self, vim: bool) -> Self {
        self.vim = vim;
        self
    }

    pub(crate) fn with_mode(mut self, mode: AgentMode) -> Self {
        self.mode = mode;
        self
    }

    pub(crate) fn build(self) -> TestApp {
        ensure_test_home();

        let (engine, cmd_rx, event_tx) = EngineHandle::for_test();

        let permissions = Arc::new(smelt_core::permissions::Permissions::load());
        let mut settings = smelt_core::config::SettingsConfig::default().resolve();
        settings.vim = self.vim;
        let shared_session = Arc::new(Mutex::new(None));
        let mut lua = crate::lua::LuaRuntime::new();
        // Match production startup: autoload registers built-in
        // commands (`:quit`, `:help`, ...) and the default keymap.
        lua.load_autoload();

        let config = smelt_core::AppConfig {
            model: String::new(),
            api_base: String::new(),
            api_key_env: String::new(),
            provider_type: String::new(),
            available_models: Vec::new(),
            model_config: engine::ModelConfig::default(),
            cli_model_override: false,
            cli_api_base_override: false,
            cli_api_key_env_override: false,
            mode: self.mode,
            mode_cycle: vec![
                AgentMode::Normal,
                AgentMode::Plan,
                AgentMode::Apply,
                AgentMode::Yolo,
            ],
            reasoning_effort: ReasoningEffort::Off,
            reasoning_cycle: Vec::new(),
            settings,
            context_window: None,
        };

        let app = TuiApp::new(
            config,
            engine,
            permissions,
            shared_session,
            None, // startup_auth_error
            lua,
            smelt_core::trust::TrustState::NoContent,
        );

        TestApp {
            app,
            cmd_rx,
            event_tx,
            actions: Vec::new(),
            quit: false,
        }
    }
}

impl TestApp {
    pub(crate) fn builder() -> TestAppBuilder {
        TestAppBuilder::default()
    }

    /// Feed a single event. Drains any engine commands the dispatch
    /// produced into the action log.
    pub(crate) fn feed_one(&mut self, ev: SourceEvent) {
        {
            // Install the TLS app pointer so Lua bindings (e.g. `:quit`
            // setting `pending_quit`) can call back into the app, mirroring
            // the main loop's `install_app_ptr` boundary.
            let _guard = crate::lua::install_app_ptr(&mut self.app);
            match ev {
                SourceEvent::Term(ev) => {
                    let quit = self.app.dispatch_terminal_event(ev);
                    if quit {
                        self.quit = true;
                        self.actions.push(Action::Quit);
                    }
                }
                SourceEvent::Engine(ev) => {
                    if let Some(mut ag) = self.app.agent.take() {
                        let ctrl = self
                            .app
                            .handle_engine_event(ev, ag.turn_id, &mut ag.pending);
                        let cont = self.app.dispatch_control(ctrl, &ag.pending);
                        self.app.agent = Some(ag);
                        if !cont {
                            self.app.discard_turn(false);
                        }
                    } else {
                        self.app.handle_idle_engine_event(ev);
                    }
                }
                SourceEvent::Tick(_ms) => {
                    // No-op until FUZZING_PLAN Phase 1 injects a virtual Clock.
                }
            }
        }
        self.drain_cmd();
    }

    pub(crate) fn feed<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = SourceEvent>,
    {
        for ev in events {
            self.feed_one(ev);
        }
    }

    /// Type a single character key with no modifiers.
    pub(crate) fn type_char(&mut self, c: char) {
        self.press_mod(KeyCode::Char(c), KeyModifiers::NONE);
    }

    /// Type each char of `s` as a separate keystroke.
    pub(crate) fn type_text(&mut self, s: &str) {
        for c in s.chars() {
            self.type_char(c);
        }
    }

    pub(crate) fn press(&mut self, code: KeyCode) {
        self.press_mod(code, KeyModifiers::NONE);
    }

    pub(crate) fn press_mod(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ev = Event::Key(KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        self.feed_one(SourceEvent::Term(ev));
    }

    pub(crate) fn inject_engine(&self, ev: EngineEvent) -> Result<(), Box<EngineEvent>> {
        self.event_tx.send(ev).map_err(|e| Box::new(e.0))
    }

    /// Drain `UiCommand`s buffered on the engine channel into the action log.
    fn drain_cmd(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.actions.push(Action::EngineSend(cmd));
        }
    }

    pub(crate) fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub(crate) fn clear_actions(&mut self) {
        self.actions.clear();
    }

    pub(crate) fn quit_requested(&self) -> bool {
        self.quit
    }

    /// Snapshot the public-facing state at this instant.
    pub(crate) fn state(&self) -> AppSnapshot {
        let cmdline_open = self.app.well_known.cmdline.is_some();
        let cmdline_text = if cmdline_open {
            cmdline_text(&self.app)
        } else {
            String::new()
        };
        let prompt_text = self
            .app
            .ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .map(|b| b.source().to_string())
            .unwrap_or_default();
        let vim_mode = self
            .app
            .ui
            .win(self.app.well_known.prompt)
            .map(|w| w.vim_mode)
            .unwrap_or(VimMode::Insert);
        AppSnapshot {
            app_focus: self.app.app_focus,
            vim_mode,
            cmdline_open,
            cmdline_text,
            focused_overlay: self.app.ui.focused_overlay(),
            prompt_text,
            queued_messages: self.app.queued_messages.clone(),
            agent_running: self.app.agent.is_some(),
            term_focused: self.app.term_focused,
            quit_requested: self.quit,
            notification: self.app.notification,
            pending_quit: self.app.pending_quit,
        }
    }
}

/// Read the cmdline payload (stripped of the `:` prefix). Mirrors the
/// private `TuiApp::cmdline_text` so suites can assert against it.
fn cmdline_text(app: &TuiApp) -> String {
    let Some(win) = app.well_known.cmdline else {
        return String::new();
    };
    let buf_id = app.ui.win(win).map(|w| w.buf);
    let line = buf_id
        .and_then(|b| app.ui.buf(b))
        .and_then(|b| b.get_line(0).map(|s| s.to_string()))
        .unwrap_or_default();
    line.strip_prefix(':').unwrap_or(&line).to_string()
}

// ── Process-wide tempdir for $HOME and XDG vars ─────────────────────

static TEST_HOME: OnceLock<TempDir> = OnceLock::new();

fn ensure_test_home() {
    let dir = TEST_HOME.get_or_init(|| TempDir::new().expect("create test $HOME tempdir"));
    let home = dir.path();
    // SAFETY: tests share the process-wide tempdir; this is set once
    // and never mutated, so concurrent reads from other threads are safe.
    std::env::set_var("HOME", home);
    std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    std::env::set_var("XDG_STATE_HOME", home.join("state"));
    std::env::set_var("XDG_CACHE_HOME", home.join("cache"));
    std::env::set_var("XDG_DATA_HOME", home.join("data"));
}

// ── Suites ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_fresh_test_app() {
        let app = TestApp::builder().build();
        let s = app.state();
        assert!(!s.cmdline_open);
        assert!(!s.quit_requested);
        assert!(!s.agent_running);
        assert_eq!(s.app_focus, AppFocus::Prompt);
        assert!(s.queued_messages.is_empty());
    }

    // ── Ctrl-C semantics ───────────────────────────────────────────

    #[test]
    fn ctrl_c_on_empty_buffer_when_idle_quits() {
        let mut app = TestApp::builder().build();
        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit_requested());
    }

    #[test]
    fn ctrl_c_with_text_in_buffer_clears_buffer_without_quitting() {
        let mut app = TestApp::builder().build();
        app.type_text("hello");
        assert_eq!(app.state().prompt_text, "hello");

        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.state().prompt_text, "");
        assert!(!app.quit_requested());
    }

    #[test]
    fn ctrl_c_twice_clears_then_quits() {
        let mut app = TestApp::builder().build();
        app.type_text("hi");
        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.quit_requested(), "first Ctrl-C clears, doesn't quit");

        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit_requested(), "second Ctrl-C on empty buffer quits");
    }

    // ── Cmdline open/close (vim-gated) ──────────────────────────────

    #[test]
    fn colon_in_non_vim_mode_does_not_open_cmdline() {
        let mut app = TestApp::builder().build();
        app.type_char(':');
        let s = app.state();
        assert!(!s.cmdline_open);
        assert_eq!(s.prompt_text, ":");
    }

    #[test]
    fn fresh_vim_prompt_starts_in_normal_mode() {
        let app = TestApp::builder().with_vim(true).build();
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn colon_in_vim_insert_mode_does_not_open_cmdline() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Enter Insert mode.
        app.type_char('i');
        assert_eq!(app.state().vim_mode, VimMode::Insert);

        app.type_char(':');
        let s = app.state();
        assert!(!s.cmdline_open);
        assert_eq!(s.prompt_text, ":");
    }

    #[test]
    fn colon_in_vim_normal_mode_opens_cmdline() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Fresh vim window starts in Normal.
        app.type_char(':');
        let s = app.state();
        assert!(s.cmdline_open);
        assert_eq!(s.cmdline_text, "");
    }

    #[test]
    fn typing_into_cmdline_appends_to_payload() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char(':');
        app.type_text("help");
        assert_eq!(app.state().cmdline_text, "help");
    }

    #[test]
    fn esc_closes_cmdline() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char(':');
        assert!(app.state().cmdline_open);

        app.press(KeyCode::Esc);
        assert!(!app.state().cmdline_open);
    }

    #[test]
    fn cmdline_quit_command_requests_quit() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char(':');
        app.type_text("quit");
        app.press(KeyCode::Enter);
        assert!(app.state().pending_quit);
    }

    // ── Picker open/filter/select ───────────────────────────────────

    fn open_test_picker(app: &mut TestApp, labels: &[&str], selected: usize) -> WinId {
        let items: Vec<crate::picker::PickerItem> = labels
            .iter()
            .map(|s| crate::picker::PickerItem::new(*s))
            .collect();
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        crate::picker::open(
            &mut app.app,
            items,
            selected,
            crate::picker::PickerPlacement::ScreenCenter,
            true,  // focusable
            false, // blocks_agent
            10,    // z
        )
        .expect("picker leaf created")
    }

    fn picker_buffer_lines(app: &TestApp, leaf: WinId) -> Vec<String> {
        let Some(buf_id) = app.app.ui.win(leaf).map(|w| w.buf) else {
            return Vec::new();
        };
        let Some(buf) = app.app.ui.buf(buf_id) else {
            return Vec::new();
        };
        (0..buf.line_count())
            .filter_map(|i| buf.get_line(i).map(String::from))
            .collect()
    }

    #[test]
    fn picker_open_focuses_overlay() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["one", "two", "three"], 0);
        let s = app.state();
        assert!(s.focused_overlay.is_some());
        assert_eq!(app.app.ui.focus(), Some(leaf));
    }

    #[test]
    fn picker_open_renders_items_into_buffer() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["alpha", "beta", "gamma"], 0);
        let lines = picker_buffer_lines(&app, leaf);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("alpha"));
        assert!(lines[1].contains("beta"));
        assert!(lines[2].contains("gamma"));
    }

    #[test]
    fn picker_set_items_replaces_buffer_contents() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["foo", "bar"], 0);
        let new_items: Vec<_> = ["x", "y", "z"]
            .iter()
            .map(|s| crate::picker::PickerItem::new(*s))
            .collect();
        crate::picker::set_items(&mut app.app, leaf, new_items, 0);
        let lines = picker_buffer_lines(&app, leaf);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("x"));
        assert!(lines[2].contains("z"));
    }

    #[test]
    fn picker_set_selected_moves_cursor() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["a", "b", "c", "d"], 0);
        let initial_cpos = app.app.ui.win(leaf).map(|w| w.cpos).unwrap_or(0);

        crate::picker::set_selected(&mut app.app, leaf, 2);
        let new_cpos = app.app.ui.win(leaf).map(|w| w.cpos).unwrap_or(0);
        assert_ne!(initial_cpos, new_cpos, "cursor moved with selection");
    }

    #[test]
    fn picker_forget_drops_state() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["a", "b"], 0);
        assert!(app.app.picker_state.contains_key(&leaf));

        crate::picker::forget(&mut app.app, leaf);
        assert!(!app.app.picker_state.contains_key(&leaf));
    }

    // ── Vim mode transitions ────────────────────────────────────────

    #[test]
    fn vim_i_enters_insert_from_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.type_char('i');
        assert_eq!(app.state().vim_mode, VimMode::Insert);
    }

    #[test]
    fn vim_a_enters_insert_after_cursor() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char('a');
        assert_eq!(app.state().vim_mode, VimMode::Insert);
    }

    #[test]
    fn vim_esc_returns_insert_to_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char('i');
        app.type_text("hello");
        assert_eq!(app.state().vim_mode, VimMode::Insert);
        assert_eq!(app.state().prompt_text, "hello");

        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_v_enters_visual_from_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char('i');
        app.type_text("abc");
        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.type_char('v');
        assert_eq!(app.state().vim_mode, VimMode::Visual);
    }

    #[test]
    fn vim_shift_v_enters_visual_line() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char('i');
        app.type_text("abc");
        app.press(KeyCode::Esc);

        app.press_mod(KeyCode::Char('V'), KeyModifiers::SHIFT);
        assert_eq!(app.state().vim_mode, VimMode::VisualLine);
    }

    #[test]
    fn vim_esc_from_visual_returns_to_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char('i');
        app.type_text("abc");
        app.press(KeyCode::Esc);
        app.type_char('v');
        assert_eq!(app.state().vim_mode, VimMode::Visual);

        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_full_cycle_normal_insert_normal_visual_normal() {
        let mut app = TestApp::builder().with_vim(true).build();
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.type_char('i');
        assert_eq!(app.state().vim_mode, VimMode::Insert);

        app.type_text("foo");
        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);

        app.type_char('v');
        assert_eq!(app.state().vim_mode, VimMode::Visual);

        app.press(KeyCode::Esc);
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_typing_in_normal_mode_does_not_append_to_buffer() {
        let mut app = TestApp::builder().with_vim(true).build();
        // Normal-mode 'h' / 'l' are motions, not characters — should not
        // land in the prompt buffer.
        app.type_text("hl");
        assert_eq!(app.state().prompt_text, "");
        assert_eq!(app.state().vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_typing_in_insert_mode_appends_to_buffer() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char('i');
        app.type_text("hello world");
        assert_eq!(app.state().prompt_text, "hello world");
    }

    #[test]
    fn vim_dd_in_normal_deletes_line() {
        let mut app = TestApp::builder().with_vim(true).build();
        app.type_char('i');
        app.type_text("line one");
        app.press(KeyCode::Esc);
        assert_eq!(app.state().prompt_text, "line one");

        // `dd` deletes the current line.
        app.type_char('d');
        app.type_char('d');
        assert_eq!(app.state().prompt_text, "");
    }

    // ── Original picker suite continues ─────────────────────────────

    #[test]
    fn picker_filter_workflow_via_set_items() {
        let mut app = TestApp::builder().build();
        let leaf = open_test_picker(&mut app, &["apple", "apricot", "banana", "cherry"], 0);
        assert_eq!(picker_buffer_lines(&app, leaf).len(), 4);

        // Simulate "filter as user types": narrow set_items, then narrow again.
        let filtered: Vec<_> = ["apple", "apricot"]
            .iter()
            .map(|s| crate::picker::PickerItem::new(*s))
            .collect();
        crate::picker::set_items(&mut app.app, leaf, filtered, 0);
        assert_eq!(picker_buffer_lines(&app, leaf).len(), 2);

        let single: Vec<_> = ["apple"]
            .iter()
            .map(|s| crate::picker::PickerItem::new(*s))
            .collect();
        crate::picker::set_items(&mut app.app, leaf, single, 0);
        let lines = picker_buffer_lines(&app, leaf);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("apple"));
    }
}
