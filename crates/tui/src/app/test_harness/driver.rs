use super::*;

impl TestApp {
    /// Feed a single event. Drains any engine commands the dispatch
    /// produced into the action log. Captures the per-thread allocation
    /// delta for this event into [`Self::last_alloc_delta`].
    pub fn feed_one(&mut self, ev: SourceEvent) {
        self.feed_one_with_transient_observer(ev, |_| {});
    }

    fn feed_one_with_transient_observer<F>(&mut self, ev: SourceEvent, on_transient_frame: F)
    where
        F: FnOnce(&mut TuiApp),
    {
        let (a0, b0) = smelt_perf::alloc::thread_snapshot();
        {
            match ev {
                SourceEvent::Term(ev) => {
                    let quit = self.app.dispatch_terminal_event(ev);
                    if quit {
                        self.quit = true;
                        self.actions.push(Action::Quit);
                    }
                }
                SourceEvent::Engine { event } => {
                    let mut sink = std::io::sink();
                    self.app.dispatch_engine_event_in_render_loop_to(
                        *event,
                        &mut sink,
                        on_transient_frame,
                    );
                }
                SourceEvent::Tick(ms) => {
                    self.clock.advance(Duration::from_millis(ms));
                }
                SourceEvent::LuaWakeup => {
                    self.app.flush_lua_callbacks();
                    self.app.drive_lua_tasks();
                }
                SourceEvent::ExecOutput(line) => {
                    self.app.append_exec_output(line);
                }
                SourceEvent::ExecDone(_) => {
                    self.app.finish_exec(None);
                    self.app.overlays.finish_execution();
                }
                SourceEvent::Resize { width, height } => {
                    self.app.handle_resize(width, height);
                }
            }
            self.app.pump_lua();
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
        self.app.render_normal();
        self.settle_transcript_hydration(&mut std::io::stdout());
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
        self.app.render_normal_to(&mut sink);
        self.settle_transcript_hydration(&mut sink);
        self.assert_render_layout_invariants();
        self.assert_prompt_cursor_projection();
    }

    /// Render exactly one production frame without waiting for asynchronous
    /// transcript hydration. Tests use this to inspect the viewport users see
    /// while a sparse seek is still pending.
    pub fn render_unsettled_silent(&mut self) -> crate::smelt_edit::SnapshotFrame {
        self.app.render_normal_to(&mut std::io::sink());
        self.assert_render_layout_invariants();
        self.assert_prompt_cursor_projection();
        self.app.ui.flushed_snapshot()
    }

    /// Render one frame and return the resulting `SnapshotFrame`. Used
    /// by the app-level storybook harness; `render_normal_to` updates the
    /// compositor snapshot buffer as a side effect of composing layers, so
    /// `flushed_snapshot()` captures that exact frame without running a second,
    /// callback-free UI render. ANSI bytes are written to a sink so tests stay quiet.
    pub fn render_to_frame(&mut self) -> crate::smelt_edit::SnapshotFrame {
        // The main loop refreshes diff signals once per tick before
        // rendering; storybook drives the render path directly without
        // that loop, so we have to publish here or Lua renderers see
        // stale `work_*` / `vim_mode` / `now` values.
        self.app.publish_diff_signals();
        let mut sink = std::io::sink();
        self.app.render_normal_to(&mut sink);
        self.settle_transcript_hydration(&mut sink);
        self.app.ui.flushed_snapshot()
    }

    fn settle_transcript_hydration(&mut self, out: &mut impl std::io::Write) {
        const HYDRATION_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
        const HYDRATION_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

        let settle_deadline = std::time::Instant::now() + HYDRATION_SETTLE_TIMEOUT;
        while self.app.conversation.transcript_hydration_is_pending()
            || self.app.session_preview_is_pending()
        {
            assert!(
                std::time::Instant::now() < settle_deadline,
                "background transcript hydration did not settle within {HYDRATION_SETTLE_TIMEOUT:?}"
            );
            let idle_deadline = std::time::Instant::now() + HYDRATION_IDLE_TIMEOUT;
            let event = loop {
                if let Some(event) = self.app.platform.try_recv_app_event() {
                    break event;
                }
                assert!(
                    std::time::Instant::now() < idle_deadline,
                    "background transcript hydration did not make progress within {HYDRATION_IDLE_TIMEOUT:?}"
                );
                std::thread::sleep(Duration::from_millis(1));
            };
            self.app.handle_app_event(event);
            if self.app.frame_scheduler.has_pending() {
                self.app.render_normal_to(out);
            }
        }
    }

    pub(crate) fn ui_snapshot(&mut self) -> crate::smelt_edit::SnapshotFrame {
        self.app.ui.snapshot()
    }

    /// Consume a scripted source through the event and compositor paths used
    /// by the main loop, recording both pre-response transient frames and the
    /// normal frame produced after every event-loop turn.
    pub async fn run_scripted_render_loop(
        &mut self,
        source: &mut impl crate::event_source::EventSource,
    ) -> Vec<RenderLoopFrame> {
        let mut frames = vec![RenderLoopFrame {
            kind: RenderLoopFrameKind::Normal,
            snapshot: self.render_to_frame(),
        }];
        while let Some(event) = source.next().await {
            let mut transient_frame = None;
            self.feed_one_with_transient_observer(event, |app| {
                transient_frame = Some(app.ui.snapshot());
            });
            if let Some(snapshot) = transient_frame {
                frames.push(RenderLoopFrame {
                    kind: RenderLoopFrameKind::Transient,
                    snapshot,
                });
            }
            frames.push(RenderLoopFrame {
                kind: RenderLoopFrameKind::Normal,
                snapshot: self.render_to_frame(),
            });
        }
        frames
    }

    /// Resize the app's surface to `(width, height)`. Used by replay
    /// drivers that own a real terminal and need to match the app's
    /// internal grid to the OS-reported size.
    pub fn set_terminal_size(&mut self, width: u16, height: u16) {
        self.app.handle_resize(width, height);
    }

    pub(crate) fn render_frame_to<W: std::io::Write>(&mut self, out: &mut W) {
        self.app.render_frame_to(out);
    }

    pub(crate) fn render_normal_to<W: std::io::Write>(&mut self, out: &mut W) {
        self.app.render_normal_to(out);
    }

    pub(crate) fn dispatch_engine_event(&mut self, event: EngineEvent) -> bool {
        self.app.dispatch_engine_event(event)
    }

    pub(crate) fn dispatch_engine_event_in_render_loop_to<
        W: std::io::Write,
        F: FnOnce(crate::smelt_edit::SnapshotFrame),
    >(
        &mut self,
        event: EngineEvent,
        out: &mut W,
        on_transient_frame: F,
    ) -> bool {
        self.app
            .dispatch_engine_event_in_render_loop_to(event, out, |app| {
                on_transient_frame(app.ui.snapshot());
            })
    }

    pub(crate) fn dispatch_engine_output_in_render_loop_to<
        W: std::io::Write,
        F: FnOnce(crate::smelt_edit::SnapshotFrame),
    >(
        &mut self,
        output: engine::EngineOutput,
        out: &mut W,
        on_transient_frame: F,
    ) -> bool {
        self.app
            .dispatch_engine_output_in_render_loop_to(output, out, |app| {
                on_transient_frame(app.ui.snapshot());
            })
    }

    pub(crate) fn drain_ready_engine_outputs_for_frame_to<
        W: std::io::Write,
        F: FnMut(crate::smelt_edit::SnapshotFrame),
    >(
        &mut self,
        out: &mut W,
        mut on_transient_frame: F,
    ) -> crate::app::render_loop::EngineOutputDrainOutcome {
        self.app
            .drain_ready_engine_outputs_for_frame_to(out, |app| {
                on_transient_frame(app.ui.snapshot());
            })
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
        let Some(injector) = self.output_injector.as_ref() else {
            return Err(Box::new(ev));
        };
        injector.send(ev)
    }

    pub fn inject_host_call(&self, call: engine::HostCall) -> Result<(), Box<engine::HostCall>> {
        let Some(injector) = self.output_injector.as_ref() else {
            return Err(Box::new(call));
        };
        injector.send_host_call(call)
    }

    pub(crate) fn try_receive_engine_output(
        &mut self,
    ) -> Result<engine::EngineOutput, tokio::sync::mpsc::error::TryRecvError> {
        self.app.core.engine.try_recv_output()
    }

    /// Drain `UiCommand`s buffered on the engine channel into the action log.
    pub(super) fn drain_cmd(&mut self) {
        let Some(cmd_rx) = self.cmd_rx.as_mut() else {
            return;
        };
        while let Ok(cmd) = cmd_rx.try_recv() {
            self.actions.push(Action::EngineSend(Box::new(cmd)));
        }
    }

    /// Drain queued `UiCommand`s from the engine channel and return them.
    /// Useful for host-hook tests that need to inspect background
    /// `EngineAsk` requests directly without going through `feed_one`.
    pub fn drain_engine_sends(&mut self) -> Vec<UiCommand> {
        let Some(cmd_rx) = self.cmd_rx.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            out.push(cmd);
        }
        out
    }

    pub fn disconnect_engine_commands(&mut self) {
        let (_, replacement) = tokio::sync::mpsc::unbounded_channel();
        self.cmd_rx = Some(replacement);
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
            picker_count: self.app.overlays.picker_count(),
            prompt_text,
            queued_inputs: self.app.prompt.queued_texts(),
            agent_running: self.app.agent_is_running(),
            term_focused: self.app.terminal_is_focused(),
            quit_requested: self.quit,
            notification: self.app.notification_win(),
            pending_quit: self.app.pending_quit,
        }
    }
}
