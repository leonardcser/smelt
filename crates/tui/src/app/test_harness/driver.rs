use super::*;

impl TestApp {
    /// Feed a single event. Drains any engine commands the dispatch
    /// produced into the action log. Captures the per-thread allocation
    /// delta for this event into [`Self::last_alloc_delta`].
    pub fn feed_one(&mut self, ev: SourceEvent) {
        let (a0, b0) = smelt_perf::alloc::thread_snapshot();
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
                SourceEvent::Engine { event } => {
                    self.app.dispatch_engine_event(*event);
                }
                SourceEvent::Tick(ms) => {
                    self.clock.advance(Duration::from_millis(ms));
                }
                SourceEvent::LuaWakeup => {
                    self.app.flush_lua_callbacks();
                    self.app.drive_lua_tasks();
                }
                SourceEvent::ExecOutput(line) => {
                    self.app.append_exec_output(&line);
                }
                SourceEvent::ExecDone(code) => {
                    self.app.finish_exec(code);
                    self.app.finalize_exec();
                    self.app.exec = None;
                }
                SourceEvent::Resize { width, height } => {
                    self.app.handle_resize(width, height);
                }
            }
            self.app.drain_idle_work();
        }
        self.drain_cmd();
        let (a1, b1) = smelt_perf::alloc::thread_snapshot();
        self.last_alloc = Some(AllocDelta {
            allocs: a1.saturating_sub(a0),
            bytes_grown: b1.saturating_sub(b0),
        });
    }

    /// Feed a single event and panic if the per-event allocation delta
    /// exceeds `budget`. Useful as a regression guard against accidental
    /// per-keystroke growth and as a hard cap for external scenario drivers.
    pub fn feed_one_within_budget(&mut self, ev: SourceEvent, budget: AllocBudget) {
        self.feed_one(ev);
        let delta = self.last_alloc.expect("feed_one populates last_alloc");
        assert!(
            delta.allocs <= budget.max_allocs,
            "event exceeded alloc-count budget: {} > {} ({} bytes grown)",
            delta.allocs,
            budget.max_allocs,
            delta.bytes_grown
        );
        assert!(
            delta.bytes_grown <= budget.max_bytes,
            "event exceeded bytes-grown budget: {} > {} ({} allocs)",
            delta.bytes_grown,
            budget.max_bytes,
            delta.allocs
        );
    }

    /// Allocation delta captured by the most recent [`Self::feed_one`].
    pub fn last_alloc_delta(&self) -> Option<AllocDelta> {
        self.last_alloc
    }

    /// Render one frame to real stdout. Drives the same compositor
    /// pipeline production uses (`TuiApp::render_normal`). The caller is
    /// responsible for terminal setup (raw mode, alternate screen).
    pub fn render(&mut self) {
        crate::lua::with_app_ptr(&mut self.app, |app| {
            app.render_normal();
        });
    }

    /// Render variant that exercises the full projection pipeline (layout,
    /// transcript/prompt/status sync, completer overlay) but throws the
    /// final compositor diff into a sink instead of stdout. Intended for
    /// the fuzz loop: every per-frame code path under `content/*` and the
    /// `compositor:*` perf scopes runs, so renderer bugs (cursor /
    /// scroll_top / tail-follow / parser projection) become reachable
    /// under fuzz without per-iteration megabytes of ANSI bytes hitting
    /// libFuzzer's log file.
    pub fn render_silent(&mut self) {
        let mut sink = std::io::sink();
        crate::lua::with_app_ptr(&mut self.app, |app| {
            app.render_normal_to(&mut sink);
        });
        self.assert_render_layout_invariants();
        self.assert_prompt_cursor_projection();
    }

    /// Render one frame and return the resulting `SnapshotFrame`. Used
    /// by the app-level storybook harness; `render_normal_to` updates the
    /// `Ui` snapshot buffer as a side effect of composing layers, so
    /// the post-render `ui.snapshot()` reflects the rendered frame. ANSI
    /// bytes are written to a sink so tests stay quiet.
    pub fn render_to_frame(&mut self) -> crate::smelt_edit::SnapshotFrame {
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        // The main loop refreshes diff signals once per tick before
        // rendering; storybook drives the render path directly without
        // that loop, so we have to publish here or Lua renderers see
        // stale `work_*` / `vim_mode` / `now` values.
        self.app.publish_diff_signals();
        let mut sink = std::io::sink();
        self.app.render_normal_to(&mut sink);
        self.app.ui.snapshot()
    }

    /// Resize the app's surface to `(width, height)`. Used by replay
    /// drivers that own a real terminal and need to match the app's
    /// internal grid to the OS-reported size.
    pub fn set_terminal_size(&mut self, width: u16, height: u16) {
        self.app.handle_resize(width, height);
    }

    pub fn feed<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = SourceEvent>,
    {
        for ev in events {
            self.feed_one(ev);
        }
    }

    /// Type a single character key with no modifiers.
    pub fn type_char(&mut self, c: char) {
        self.press_mod(KeyCode::Char(c), KeyModifiers::NONE);
    }

    /// Type each char of `s` as a separate keystroke.
    pub fn type_text(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.press_mod(KeyCode::Enter, KeyModifiers::SHIFT);
            } else {
                self.type_char(c);
            }
        }
    }

    pub fn press(&mut self, code: KeyCode) {
        self.press_mod(code, KeyModifiers::NONE);
    }

    pub fn press_mod(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ev = Event::Key(KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        self.feed_one(SourceEvent::Term(ev));
    }

    pub fn inject_engine(&self, ev: EngineEvent) -> Result<(), Box<EngineEvent>> {
        self.event_tx.send(ev).map_err(|e| Box::new(e.0))
    }

    /// Drain `UiCommand`s buffered on the engine channel into the action log.
    pub(super) fn drain_cmd(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.actions.push(Action::EngineSend(Box::new(cmd)));
        }
    }

    /// Drain queued `UiCommand`s from the engine channel and return them.
    /// Useful for host-hook tests that need to inspect background
    /// `EngineAsk` requests directly without going through `feed_one`.
    pub fn drain_engine_sends(&mut self) -> Vec<UiCommand> {
        let mut out = Vec::new();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub fn clear_actions(&mut self) {
        self.actions.clear();
    }

    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// Snapshot the public-facing state at this instant.
    pub fn state(&self) -> AppSnapshot {
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
            .map(|w| w.vim_mode())
            .unwrap_or(VimMode::Insert);
        AppSnapshot {
            app_focus: self.app.app_focus,
            vim_mode,
            cmdline_open,
            cmdline_text,
            focused_overlay: self.app.ui.focused_overlay(),
            active_modal: self.app.ui.active_modal(),
            picker_count: self.app.picker_state.len(),
            prompt_text,
            queued_inputs: self.app.queued_inputs.display_texts(),
            agent_running: self.app.agent.is_some(),
            term_focused: self.app.term_focused,
            quit_requested: self.quit,
            notification: self.app.notification_win(),
            pending_quit: self.app.pending_quit,
        }
    }
}
