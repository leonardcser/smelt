use super::*;

fn next_synthetic_invocation_id() -> protocol::InvocationId {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    protocol::InvocationId::new(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

impl TestApp {
    pub fn push_context_recalculation(&mut self, label: impl Into<String>) {
        let _token = self
            .tui
            .busy_stack
            .push_context_recalculation_token(label.into());
    }

    pub(crate) fn push_transcript_block(
        &mut self,
        block: smelt_transcript::transcript_model::Block,
    ) {
        self.tui.push_block(block);
    }

    pub(crate) fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: protocol::StyledLines,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) -> protocol::InvocationId {
        let invocation_id = next_synthetic_invocation_id();
        let called_at_ms = self.tool_called_at_ms();
        self.tui
            .start_tool_at(invocation_id, call_id, name, summary, args, called_at_ms);
        invocation_id
    }

    pub(crate) fn finish_tool(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: smelt_transcript::transcript_model::ToolStatus,
        output: Option<smelt_transcript::transcript_model::ToolOutputRef>,
        engine_elapsed: Option<std::time::Duration>,
    ) {
        self.tui
            .finish_tool(invocation_id, status, output, engine_elapsed);
    }

    pub(crate) fn dispatch_host_call(&mut self, call: engine::HostCall) {
        self.tui.dispatch_host_call(call);
        self.wait_for_project_context();
    }

    pub(crate) fn dispatch_confirm_request(
        &mut self,
        request: smelt_core::ConfirmRequest,
        pending: &mut Vec<crate::app::PendingTool>,
    ) -> crate::app::SessionControl {
        let mut turn = self
            .tui
            .conversation
            .clear_active()
            .expect("test turn is active");
        turn.pending = std::mem::take(pending);
        let control = self.tui.dispatch_control(
            crate::app::SessionControl::NeedsConfirm(Box::new(request)),
            &mut turn,
        );
        *pending = std::mem::take(&mut turn.pending);
        self.tui.conversation.set_active(Some(turn));
        control
    }

    pub(crate) fn start_custom_command_turn(
        &mut self,
        command: smelt_core::custom_commands::CustomCommand,
    ) -> bool {
        self.ensure_writer_ready();
        let sent_at_ms = engine::clock::unix_time_ms(self.tui.core.clock.as_ref());
        let turn = self.tui.begin_custom_command_turn(command, sent_at_ms);
        self.tui.conversation.set_active(turn);
        self.wait_for_turn_dispatch();
        self.agent_running()
    }

    pub(crate) fn start_command_request_turn(
        &mut self,
        display: String,
        evaluated: String,
        overrides: smelt_core::custom_commands::CommandOverrides,
        start: crate::app::CommandTurnStart,
    ) -> bool {
        self.ensure_writer_ready();
        let turn = self.tui.begin_command_request_turn(
            display,
            evaluated,
            crate::app::CommandScope {
                name: None,
                overrides,
            },
            start,
            engine::clock::unix_time_ms(self.tui.core.clock.as_ref()),
        );
        self.tui.conversation.set_active(turn);
        self.wait_for_turn_dispatch();
        self.agent_running()
    }

    pub(crate) fn open_picker(
        &mut self,
        items: Vec<crate::picker::PickerItem>,
        selected: usize,
        placement: crate::picker::PickerPlacement,
        focusable: bool,
        blocks_agent: bool,
    ) -> Option<WinId> {
        crate::picker::open(
            &mut self.tui,
            items,
            selected,
            placement,
            focusable,
            blocks_agent,
        )
    }

    pub(crate) fn set_picker_items(
        &mut self,
        leaf: WinId,
        items: Vec<crate::picker::PickerItem>,
        selected: usize,
    ) {
        crate::picker::set_items(&mut self.tui, leaf, items, selected);
    }

    pub(crate) fn set_picker_selected(&mut self, leaf: WinId, selected: usize) {
        crate::picker::set_selected(&mut self.tui, leaf, selected);
    }

    pub(crate) fn forget_picker(&mut self, leaf: WinId) {
        crate::picker::forget(&mut self.tui, leaf);
    }

    pub(crate) fn scroll_at(&mut self, x: u16, y: u16, rows: isize) -> bool {
        self.tui.ui.scroll_at(x, y, rows)
    }

    pub(crate) fn open_readonly_overlay_fixture(
        &mut self,
        lines: Vec<String>,
        unselectable_prefix_len: Option<usize>,
    ) -> WinId {
        let buf = self
            .tui
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());
        {
            let buffer = self.tui.ui.buf_mut(buf).expect("overlay buffer");
            buffer.readonly = true;
            buffer.set_all_lines(lines);
            if let Some(end) = unselectable_prefix_len {
                buffer.add_highlight_group_with_meta(
                    0,
                    0,
                    end.min(u16::MAX as usize) as u16,
                    smelt_buffer::theme::intern("Normal"),
                    crate::smelt_edit::SpanMeta::unselectable(),
                );
            }
        }
        let leaf = self
            .tui
            .ui
            .win_open_split(
                buf,
                crate::smelt_edit::SplitConfig {
                    gutters: Default::default(),
                },
            )
            .expect("overlay leaf");
        if let Some(win) = self.tui.ui.win_mut(leaf) {
            win.set_surface(crate::smelt_edit::WindowSurface::readonly_text());
            win.set_vim_enabled(true);
        }
        self.tui.ui.overlay_open(
            crate::smelt_edit::Overlay::new(
                crate::smelt_edit::LayoutTree::leaf(leaf),
                crate::smelt_edit::Anchor::ScreenCenter,
            )
            .with_size((40, 5))
            .modal(true),
        );
        leaf
    }

    pub(crate) fn set_window_lines(&mut self, win: WinId, lines: Vec<String>) {
        let buf = self.tui.ui.win(win).expect("window").buf;
        self.tui
            .ui
            .buf_mut(buf)
            .expect("window buffer")
            .set_all_lines(lines);
    }

    pub(crate) fn set_window_cursor(&mut self, win: WinId, cpos: usize) {
        self.tui.ui.win_mut(win).expect("window").set_cpos(cpos);
    }

    pub(crate) fn set_overlay_size_override(
        &mut self,
        overlay: crate::smelt_edit::OverlayId,
        size: (u16, u16),
    ) {
        if let Some(overlay) = self.tui.ui.overlay_mut(overlay) {
            overlay.size_override = Some(size);
        }
    }

    /// Side-channel: insert a synthetic image attachment at the prompt
    /// cursor. Mirrors clipboard-image paste / `:image` paths without
    /// needing a real terminal clipboard. Exercises the
    /// attachment_ids ↔ marker invariant under interleaved mutations.
    pub fn insert_attachment(&mut self, label: String) {
        self.insert_image_attachment(label, "data:image/png;base64,FUZZ-0".to_string());
    }

    pub(crate) fn insert_image_attachment(&mut self, label: String, data_url: String) {
        let mut ctx = crate::input::prompt_ctx_mut(&mut self.tui.ui);
        self.tui
            .prompt
            .insert_image_for_harness(&mut ctx, label, data_url);
    }

    /// Side-channel: flip pane focus between Prompt and Content. In
    /// production this requires a Ctrl-W chord inside `PANE_CHORD_WINDOW`;
    /// the harness bypasses the timing gate so coverage doesn't depend on
    /// random key collisions.
    pub fn toggle_pane_focus(&mut self) {
        self.tui.toggle_pane_focus();
    }

    /// Side-channel: install a placeholder on the prompt window with given
    /// accept / dismiss chords. Mirrors what Lua's `Win:placeholder(text, opts)`
    /// does; the dispatch path then runs on the next matching key. Without a
    /// side channel the placeholder is reachable only through Lua, which
    /// limits coverage of the accept/dismiss key-routing branches.
    pub fn install_prompt_placeholder(
        &mut self,
        text: String,
        accept: Vec<crate::smelt_edit::KeyBind>,
        dismiss: Vec<crate::smelt_edit::KeyBind>,
    ) {
        let win = self.tui.well_known.prompt;
        if text.is_empty() {
            self.tui.clear_placeholder(win);
            return;
        }
        self.tui.set_placeholder(win, text);
        self.tui.set_placeholder_options(
            win,
            crate::app::PlaceholderOpts {
                accept_keys: accept,
                dismiss_keys: dismiss,
            },
        );
    }

    /// Side-channel: clear the prompt placeholder (both extmark and opts).
    pub fn clear_prompt_placeholder(&mut self) {
        let win = self.tui.well_known.prompt;
        self.tui.clear_placeholder(win);
    }

    /// Side-channel: open a synthetic overlay via `smelt.overlay.new`.
    /// `variant % N` picks from a small fixed set spanning the new
    /// surface area (leaf, vbox, with static measure, with keymap,
    /// named vs anonymous). Same-variant repeats land on the same
    /// NamedSlot name so the dedup path runs; different variants
    /// allocate fresh slots. Best-effort: a Lua failure is swallowed
    /// (the next op still runs against a consistent app).
    pub fn open_synthetic_overlay(&mut self, variant: u8) {
        const VARIANTS: &[&str] = &[
            // 0: named leaf
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.0.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.0.win" })
            smelt.overlay.new({
                name = "fuzz.ov.0", anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w),
            })
            "#,
            // 1: anonymous leaf - reaped on reload
            r#"
            local b = smelt.buf.new()
            local w = smelt.win.new(b, {})
            smelt.overlay.new({
                anchor = "screen_at", corner = "ne",
                row = 0, col = 0, width = 15, height = 4,
                layout = smelt.ui.layout.leaf(w),
            })
            "#,
            // 2: leaf with static measure (drives the per-leaf measure hook)
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.2.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.2.win" })
            smelt.overlay.new({
                name = "fuzz.ov.2", anchor = "screen_at", corner = "sw",
                row = 0, col = 0, width = 25, height = 6,
                layout = smelt.ui.layout.leaf(w, { measure = { w = 18, h = 4 } }),
            })
            "#,
            // 3: vbox of two leaves
            r#"
            local b1 = smelt.buf.new({ name = "fuzz.ov.3.buf1" })
            local w1 = smelt.win.new(b1, { name = "fuzz.ov.3.win1" })
            local b2 = smelt.buf.new({ name = "fuzz.ov.3.buf2" })
            local w2 = smelt.win.new(b2, { name = "fuzz.ov.3.win2" })
            smelt.overlay.new({
                name = "fuzz.ov.3", anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 22, height = 8,
                layout = smelt.ui.layout.vbox({
                    { node = smelt.ui.layout.leaf(w1), height = 3 },
                    { node = smelt.ui.layout.leaf(w2), height = 3 },
                }),
            })
            "#,
            // 4: leaf with overlay-level keymap (deferred-safe path)
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.4.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.4.win" })
            smelt.overlay.new({
                name = "fuzz.ov.4", anchor = "screen_at", corner = "nw",
                row = 1, col = 1, width = 18, height = 5,
                layout = smelt.ui.layout.leaf(w),
                keymaps = {
                    { key = "<C-x>", on_press = function() end },
                },
            })
            "#,
        ];
        let snippet = VARIANTS[(variant as usize) % VARIANTS.len()];
        let lua = self.tui.lua.lua().clone();
        let _ = crate::lua::scope_app(&mut self.tui, || lua.load(snippet).exec());
    }

    /// Append a canonical user turn and its transcript block without a real
    /// engine roundtrip.
    pub fn push_user_block(&mut self, text: &str) {
        let sent_at_ms = engine::clock::unix_time_ms(self.clock.as_ref());
        self.tui.stage_request_history_item(
            protocol::HistoryItem::user(protocol::Content::text(text)).with_sent_at_ms(sent_at_ms),
            Some(smelt_transcript::transcript_model::Block::User {
                text: text.to_string(),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: Some(sent_at_ms),
            }),
        );
    }

    /// Append a command-marked user block to the transcript history.
    pub fn push_command_block(&mut self, text: &str) {
        let sent_at_ms = engine::clock::unix_time_ms(self.clock.as_ref());
        self.tui
            .push_block(smelt_transcript::transcript_model::Block::User {
                text: text.to_string(),
                image_labels: Vec::new(),
                command: true,
                sent_at_ms: Some(sent_at_ms),
            });
    }

    /// Push a `Block::Compacted` summary block into the transcript -
    /// the same committed marker installed after a successful compaction
    /// checkpoint. Stories use this to snapshot the final compaction chrome
    /// without running a real `engine.ask` round-trip.
    pub fn push_compacted(&mut self, summary: &str) {
        self.push_compacted_kept(summary, Vec::new());
    }

    /// Push a compacted marker that repeats the entries the model still
    /// receives verbatim after the checkpoint.
    pub fn push_compacted_kept(&mut self, summary: &str, kept: Vec<smelt_core::KeptHistoryItem>) {
        self.tui
            .push_block(smelt_transcript::transcript_model::Block::Compacted {
                summary: summary.to_string(),
                kept,
            });
    }

    /// Push or rewrite the transient compaction preview block shown while
    /// the compact plugin streams a checkpoint summary.
    pub fn push_compaction_preview(&mut self, summary: &str) {
        self.tui.update_compaction_preview(summary.to_string());
    }

    /// Push a process-status block without spawning a subprocess.
    pub fn push_process_status(&mut self, event: protocol::ProcessStatusEvent, output: &str) {
        let note = protocol::HistoryNote::process_status_with_output(event, output);
        let block = self
            .history_note_to_block(&note)
            .expect("process-status note has a transcript block");
        self.tui.push_block(block);
    }

    /// Push a `Block::Mode` into the transcript without restoring a fixture.
    pub fn push_mode_block(&mut self, text: &str, icon: &str, hl_group: &str) {
        self.tui
            .push_block(smelt_transcript::transcript_model::Block::Mode {
                text: text.to_string(),
                icon: icon.to_string(),
                hl_group: hl_group.to_string(),
            });
    }

    /// Push a `Block::CodeLine` into the transcript.
    pub fn push_code_line(&mut self, content: &str, lang: &str) {
        self.tui
            .push_block(smelt_transcript::transcript_model::Block::CodeLine {
                content: content.to_string(),
                lang: lang.to_string(),
            });
    }

    /// Open a `Block::Exec` shell-escape block in the transcript with
    /// `command` as the header. Pair with
    /// `SourceEvent::ExecOutput`/`ExecDone` to stream output and close
    /// the block. The production path is `start_shell_escape`, which
    /// also spawns a real `sh -c`; stories don't want a subprocess, so
    /// the harness invokes the transcript hook and input-submit event
    /// directly.
    pub fn start_exec(&mut self, command: &str) {
        self.tui.start_exec(command.to_string());
        self.tui.publish_input_submit(format!("!{command}"));
        self.tick_signals();
    }

    /// Stream one line into the active synthetic shell-escape block.
    pub fn append_exec_output(&mut self, line: impl Into<String>) {
        self.feed_one(SourceEvent::ExecOutput(line.into()));
    }

    /// Complete the active synthetic shell-escape block.
    pub fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.feed_one(SourceEvent::ExecDone(exit_code));
    }

    /// Feed one engine event through the active or idle dispatch path.
    pub fn engine_event(&mut self, event: EngineEvent) {
        self.feed_one(SourceEvent::engine(event));
    }

    fn tool_called_at_ms(&self) -> u64 {
        engine::clock::unix_time_ms(self.clock.as_ref())
    }

    pub fn tool_started(
        &mut self,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) -> protocol::InvocationId {
        let invocation_id = next_synthetic_invocation_id();
        let called_at_ms = self.tool_called_at_ms();
        self.engine_event(EngineEvent::ToolStarted {
            invocation_id,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            args,
            called_at_ms,
        });
        invocation_id
    }

    pub fn tool_finished(
        &mut self,
        invocation_id: protocol::InvocationId,
        call_id: impl Into<String>,
        result: protocol::ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        self.engine_event(EngineEvent::ToolFinished {
            invocation_id,
            call_id: call_id.into(),
            result,
            elapsed_ms,
        });
    }

    pub fn tool_rejected(
        &mut self,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: std::collections::HashMap<String, serde_json::Value>,
        summary: protocol::StyledLines,
        result: protocol::ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        let called_at_ms = self.tool_called_at_ms();
        self.engine_event(EngineEvent::ToolRejected {
            invocation_id: next_synthetic_invocation_id(),
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            args,
            summary,
            result,
            elapsed_ms,
            called_at_ms,
        });
    }

    pub fn request_permission(
        &mut self,
        request_id: u64,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: std::collections::HashMap<String, serde_json::Value>,
        approval_patterns: Vec<String>,
        summary: protocol::StyledLines,
    ) {
        let invocation_id = next_synthetic_invocation_id();
        let call_id = call_id.into();
        let tool_name = tool_name.into();
        self.engine_event(EngineEvent::ToolEvaluationRequest {
            request_id: invocation_id.get(),
            invocation_id,
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            args: args.clone(),
            mode: self.tui.core.config.mode.clone(),
        });
        self.wait_for_tool_evaluation(invocation_id.get());
        let called_at_ms = self.tool_called_at_ms();
        self.engine_event(EngineEvent::RequestPermission {
            request_id,
            invocation_id,
            call_id,
            tool_name,
            provider: None,
            args,
            approval_patterns,
            called_at_ms,
            summary,
        });
    }

    /// Cancel the active turn (or idle background tasks). Mirrors
    /// `EventOutcome::CancelAgent` → `discard_turn(true)`.
    pub fn cancel(&mut self) {
        self.tui.discard_turn(crate::app::TurnEnd::Cancelled);
        self.drain_cmd();
    }

    /// Push text that has already been promoted into the active turn's request queue.
    pub fn steer(&mut self, text: &str) {
        if !text.is_empty() {
            self.tui
                .prompt
                .try_queue_request(crate::app::QueuedInput::request_from_text(
                    text.to_string(),
                    text.to_string(),
                    engine::clock::unix_time_ms(self.tui.core.clock.as_ref()),
                ));
        }
    }

    /// Remove up to `count` request-queued messages from the front.
    pub fn unsteer(&mut self, count: usize) {
        self.tui.prompt.acknowledge_requests(count);
    }

    /// Send a `CallCoreTool` UiCommand to the engine channel.
    pub fn call_core_tool(
        &mut self,
        tool_name: &str,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        self.tui
            .core
            .engine
            .send(protocol::UiCommand::CallCoreTool {
                request_id: 1,
                parent_invocation_id: next_synthetic_invocation_id(),
                parent_call_id: String::new(),
                tool_name: tool_name.to_string(),
                args,
            });
        self.drain_cmd();
    }

    /// Change the active agent mode.
    pub fn set_agent_mode(&mut self, mode: AgentMode) {
        self.tui.core.config.mode = mode;
    }

    /// Push an `assistant` text block onto the transcript history so
    /// flows that read message history see a multi-turn conversation.
    pub fn push_assistant_text(&mut self, text: &str) {
        self.tui
            .conversation
            .append_history_item(protocol::HistoryItem::Assistant(
                protocol::AssistantStep::terminal(
                    Some(protocol::Content::Text(text.to_string())),
                    None,
                    Vec::new(),
                ),
            ));
    }

    /// Saves one session described by `meta` to the store.
    pub fn seed_session_meta(&self, meta: &smelt_core::session::SessionMeta) {
        let history = if let Some(text_bytes) = meta.text_bytes.filter(|bytes| *bytes > 0) {
            let mut remaining = usize::try_from(text_bytes).expect("fixture text size fits usize");
            let mut history = Vec::new();
            while remaining > 0 {
                let chunk = remaining.min(60_000);
                history.push(protocol::HistoryItem::user(protocol::Content::text(
                    "x".repeat(chunk),
                )));
                remaining -= chunk;
            }
            history
        } else {
            (0..meta.history_len.unwrap_or_default())
                .map(|index| {
                    protocol::HistoryItem::user(protocol::Content::text(format!(
                        "test session fixture row {index}"
                    )))
                })
                .collect::<Vec<_>>()
        };
        let mut session = smelt_core::session::Session::new(1, std::path::PathBuf::new());
        session.id = meta.id.clone();
        session.parent_id = meta.parent_id.clone();
        session.title = meta.title.clone();
        session.slug = meta.slug.clone();
        session.first_user_message = meta.first_user_message.clone();
        session.created_at_ms = meta.created_at_ms;
        session.updated_at_ms = meta.updated_at_ms;
        session.mode = meta.mode.clone();
        session.reasoning_effort = meta.reasoning_effort.clone();
        session.model = meta.model.clone();
        session.fast_mode = meta.fast_mode;
        session.cwd = meta.cwd.clone();
        session.checkpoint = meta.checkpoint.clone();
        session.checkpoint_events = meta.checkpoint_events.clone();
        if let Some(context) = &meta.authoritative_context_tokens {
            session.context_tokens = Some(context.tokens);
            session.context_tokens_history_len = Some(context.history_len);
            session.context_token_identity = Some(context.identity.clone());
        }
        if let Some(context) = &meta.display_context_tokens {
            session.display_context_tokens = Some(context.tokens);
            session.display_context_token_identity = context.identity.clone();
        }
        session.history = history;
        self.tui
            .core
            .sessions
            .save(&session)
            .expect("write session fixture");
    }
}
