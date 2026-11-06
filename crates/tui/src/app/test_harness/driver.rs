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
                    let quit = self.tui.dispatch_terminal_event(ev);
                    if quit {
                        self.quit = true;
                        self.actions.push(Action::Quit);
                    }
                }
                SourceEvent::Engine { event } => {
                    let mut sink = std::io::sink();
                    self.tui.dispatch_engine_event_in_render_loop_to(
                        *event,
                        &mut sink,
                        on_transient_frame,
                    );
                }
                SourceEvent::Tick(ms) => {
                    self.clock.advance(Duration::from_millis(ms));
                }
                SourceEvent::LuaWakeup => {
                    self.tui.flush_lua_callbacks();
                    self.tui.drive_lua_tasks();
                }
                SourceEvent::ExecOutput(line) => {
                    self.tui.append_exec_output(line);
                }
                SourceEvent::ExecDone(_) => {
                    self.tui.finish_exec(None);
                    self.tui.overlays.finish_execution();
                }
                SourceEvent::Resize { width, height } => {
                    self.tui.handle_resize(width, height);
                }
            }
            self.tui.pump_lua();
            self.tui.drain_idle_work();
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
        self.tui.render_normal();
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
        self.tui.render_normal_to(&mut sink);
        self.settle_transcript_hydration(&mut sink);
        self.assert_render_layout_invariants();
        self.assert_prompt_cursor_projection();
    }

    /// Render exactly one production frame without waiting for asynchronous
    /// transcript hydration. Tests use this to inspect the viewport users see
    /// while a sparse seek is still pending.
    pub fn render_unsettled_silent(&mut self) -> crate::smelt_edit::SnapshotFrame {
        self.tui.render_normal_to(&mut std::io::sink());
        self.assert_render_layout_invariants();
        self.assert_prompt_cursor_projection();
        self.tui.ui.flushed_snapshot()
    }

    /// Render one frame and return the resulting `SnapshotFrame`. Used
    /// by the app-level storybook harness; `render_normal_to` updates the
    /// compositor snapshot buffer as a side effect of composing layers, so
    /// `flushed_snapshot()` captures that exact frame without running a second,
    /// callback-free UI render. ANSI bytes are written to a sink so tests stay quiet.
    pub fn render_to_frame(&mut self) -> crate::smelt_edit::SnapshotFrame {
        let mut sink = std::io::sink();
        self.tui.render_normal_to(&mut sink);
        self.settle_transcript_hydration(&mut sink);
        self.tui.ui.flushed_snapshot()
    }

    /// Run Lua animations against the virtual wall clock. The harness
    /// otherwise freezes `smelt.time.now_ms` at 0 for snapshot determinism.
    pub fn animate_with_virtual_clock(&mut self) {
        self.run_lua_result("smelt.time.now_ms = function() return _G.animation_now or 0 end")
            .unwrap();
    }

    /// Advance the virtual clock by `ms`, publish it to Lua animations, fire the
    /// timers that came due and render one frame without waiting for transcript
    /// hydration.
    pub fn advance_animation(&mut self, ms: u64) -> crate::smelt_edit::SnapshotFrame {
        self.feed_one(SourceEvent::Tick(ms));
        self.tui
            .lua
            .lua()
            .globals()
            .set(
                "animation_now",
                engine::clock::unix_time_ms(self.tui.core.clock.as_ref()),
            )
            .unwrap();
        self.tui.tick_timers();
        self.tui.publish_diff_signals();
        self.render_unsettled_silent()
    }

    /// Style of the working indicator's dot: the cell after the prompt bar's
    /// left edge on the first rule row of `frame`.
    pub fn working_dot_style(&self, frame: &crate::smelt_edit::SnapshotFrame) -> Option<String> {
        self.bar_cell_style(frame, 2)
    }

    /// Text of the cells painted with `group`'s background in a fresh frame,
    /// one line per screen row that has any, so tests assert what the user sees.
    pub fn highlighted_text(&mut self, group: &str) -> Vec<String> {
        let bg = self.tui.ui.theme().get(group).bg;
        assert!(bg.is_some(), "highlight group {group} has no background");
        let frame = self.render_to_frame();
        frame
            .rows
            .iter()
            .zip(&frame.styles)
            .filter_map(|(row, styles)| {
                let text: String = smelt_buffer::cell_width::grapheme_indices(row)
                    .zip(styles)
                    .filter(|(_, style)| style.bg == bg)
                    .map(|((_, grapheme), _)| grapheme)
                    .collect();
                (!text.is_empty()).then_some(text)
            })
            .collect()
    }

    /// Feed one mouse event at screen `(column, row)`, then repaint.
    pub fn mouse(&mut self, kind: crossterm::event::MouseEventKind, (column, row): (u16, u16)) {
        self.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        )));
        self.render_silent();
    }

    /// Screen `(column, row)` where `needle` starts on the last frame row
    /// that shows it.
    pub fn screen_position(&mut self, needle: &str) -> Option<(u16, u16)> {
        let frame = self.render_to_frame();
        frame.rows.iter().enumerate().rev().find_map(|(y, row)| {
            let byte = row.find(needle)?;
            let x = smelt_buffer::cell_width::grapheme_indices(&row[..byte]).count();
            Some((x as u16, y as u16))
        })
    }

    /// Style of the cell at `col` on the first rule row of `frame`.
    pub fn bar_cell_style(
        &self,
        frame: &crate::smelt_edit::SnapshotFrame,
        col: usize,
    ) -> Option<String> {
        let text = frame.text();
        let row = text.lines().position(|line| line.starts_with('─'))?;
        frame.styles_text().lines().skip(1).find_map(|line| {
            let mut parts = line.split_whitespace();
            let y: usize = parts.next()?.parse().ok()?;
            let x: usize = parts.next()?.parse().ok()?;
            let len: usize = parts.next()?.parse().ok()?;
            (y == row && x <= col && col < x + len).then(|| parts.collect::<Vec<_>>().join(" "))
        })
    }

    /// Top row of the scrollbar thumb painted in the last flushed frame,
    /// relative to `track.start`. Thumb cells carry the theme's
    /// `SmeltScrollbarThumb` background.
    pub fn painted_scrollbar_thumb_top(
        &self,
        col: u16,
        track: std::ops::Range<u16>,
    ) -> Option<u16> {
        let thumb = self.tui.ui.theme().get("SmeltScrollbarThumb");
        let thumb_bg = thumb.bg.or(thumb.fg)?;
        let frame = self.tui.ui.flushed_snapshot();
        track.clone().find_map(|row| {
            frame
                .style_at(col, row)
                .is_some_and(|style| style.bg == Some(thumb_bg))
                .then(|| row - track.start)
        })
    }

    fn settle_transcript_hydration(&mut self, out: &mut impl std::io::Write) {
        const HYDRATION_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
        const HYDRATION_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

        let settle_deadline = std::time::Instant::now() + HYDRATION_SETTLE_TIMEOUT;
        while self.tui.conversation.transcript_hydration_is_pending()
            || self.tui.session_preview_is_pending()
        {
            assert!(
                std::time::Instant::now() < settle_deadline,
                "background transcript hydration did not settle within {HYDRATION_SETTLE_TIMEOUT:?}"
            );
            let idle_deadline = std::time::Instant::now() + HYDRATION_IDLE_TIMEOUT;
            let event = loop {
                if let Some(event) = self.tui.platform.try_recv_app_event() {
                    break event;
                }
                assert!(
                    std::time::Instant::now() < idle_deadline,
                    "background transcript hydration did not make progress within {HYDRATION_IDLE_TIMEOUT:?}"
                );
                std::thread::sleep(super::POLL_INTERVAL);
            };
            self.tui.handle_app_event(event);
            if self.tui.frame_scheduler.has_pending() {
                self.tui.render_normal_to(out);
            }
        }
    }

    /// The frame last presented to the terminal.
    pub(crate) fn presented_frame(&self) -> crate::smelt_edit::SnapshotFrame {
        self.tui.ui.flushed_snapshot()
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
                transient_frame = Some(app.ui.flushed_snapshot());
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
        self.tui.handle_resize(width, height);
    }

    pub(crate) fn render_normal_to<W: std::io::Write>(&mut self, out: &mut W) {
        self.tui.render_normal_to(out);
    }

    pub(crate) fn dispatch_engine_event(&mut self, event: EngineEvent) -> bool {
        self.tui.dispatch_engine_event(event)
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
        self.tui
            .dispatch_engine_event_in_render_loop_to(event, out, |app| {
                on_transient_frame(app.ui.flushed_snapshot());
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
        self.tui
            .dispatch_engine_output_in_render_loop_to(output, out, |app| {
                on_transient_frame(app.ui.flushed_snapshot());
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
        self.tui
            .drain_ready_engine_outputs_for_frame_to(out, |app| {
                on_transient_frame(app.ui.flushed_snapshot());
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
        self.tui.core.engine.try_recv_output()
    }

    /// Drain `UiCommand`s buffered on the engine channel into the action log.
    pub(super) fn drain_cmd(&mut self) {
        self.wait_for_writer_startup();
        let Some(cmd_rx) = self.cmd_rx.as_mut() else {
            return;
        };
        while let Ok(cmd) = cmd_rx.try_recv() {
            self.actions.push(Action::EngineSend(Box::new(cmd)));
        }
    }

    /// Takes every `UiCommand` sent to the engine since the last drain, whether
    /// still buffered on the channel or already moved into the action log.
    pub fn drain_engine_sends(&mut self) -> Vec<UiCommand> {
        self.drain_cmd();
        let mut out = Vec::new();
        for action in std::mem::take(&mut self.actions) {
            match action {
                Action::EngineSend(cmd) => out.push(*cmd),
                action => self.actions.push(action),
            }
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
        let cmdline_open = self.tui.well_known.cmdline.is_some();
        let cmdline_text = if cmdline_open {
            cmdline_text(&self.tui)
        } else {
            String::new()
        };
        let prompt_text = self
            .tui
            .ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .map(|b| b.source().to_string())
            .unwrap_or_default();
        let vim_mode = self
            .tui
            .ui
            .win(self.tui.well_known.prompt)
            .map(|w| w.vim_mode())
            .unwrap_or(VimMode::Insert);
        AppSnapshot {
            app_focus: self.tui.app_focus,
            vim_mode,
            cmdline_open,
            cmdline_text,
            focused_overlay: self.tui.ui.focused_overlay(),
            active_modal: self.tui.ui.active_modal(),
            picker_count: self.tui.overlays.picker_count(),
            prompt_text,
            queued_inputs: self.tui.prompt.queued_texts(),
            agent_running: self.tui.agent_is_running(),
            term_focused: self.tui.terminal_is_focused(),
            quit_requested: self.quit,
            notification: self.tui.notification_win(),
            pending_quit: self.tui.pending_quit,
        }
    }
}
