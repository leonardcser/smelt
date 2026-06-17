use super::*;

impl TestApp {
    /// Side-channel: insert a synthetic image attachment at the prompt
    /// cursor. Mirrors clipboard-image paste / `:image` paths without
    /// needing a real terminal clipboard. Exercises the
    /// attachment_ids ↔ marker invariant under interleaved mutations.
    pub fn insert_attachment(&mut self, label: String) {
        let data_url = "data:image/png;base64,FUZZ-0".to_string();
        let mut ctx = crate::input::prompt_ctx_mut(&mut self.app.ui);
        self.app.input.insert_image(&mut ctx, label, data_url);
    }

    /// Side-channel: flip pane focus between Prompt and Content. In
    /// production this requires a Ctrl-W chord inside `PANE_CHORD_WINDOW`;
    /// the harness bypasses the timing gate so coverage doesn't depend on
    /// random key collisions.
    pub fn toggle_pane_focus(&mut self) {
        self.app.toggle_pane_focus();
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
        let win = self.app.well_known.prompt;
        if text.is_empty() {
            self.app.clear_placeholder(win);
            return;
        }
        self.app.set_placeholder(win, text);
        self.app.placeholder_opts.insert(
            win,
            crate::app::PlaceholderOpts {
                accept_keys: accept,
                dismiss_keys: dismiss,
            },
        );
    }

    /// Side-channel: clear the prompt placeholder (both extmark and opts).
    pub fn clear_prompt_placeholder(&mut self) {
        let win = self.app.well_known.prompt;
        self.app.clear_placeholder(win);
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
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        let _ = self.app.lua.lua.load(snippet).exec();
    }

    /// Append a `Block::User` to the transcript history so flows that
    /// read the user-turn list (rewind dialog, transcript projection)
    /// see a non-empty conversation without a real engine roundtrip.
    pub fn push_user_block(&mut self, text: &str) {
        self.app.show_user_message(text, Vec::new());
    }

    /// Push a `Block::Compacted` summary block into the transcript -
    /// the same block the live compact plugin produces between turns.
    /// Stories use this to snapshot the compaction chrome without
    /// running a real `engine.ask` round-trip.
    pub fn push_compacted(&mut self, summary: &str) {
        self.app
            .push_block(smelt_core::transcript_model::Block::Compacted {
                summary: summary.to_string(),
            });
    }

    /// Push a typed process-status block into the transcript. Mirrors the
    /// display block produced when a background process completion is
    /// committed to history, without needing to spawn a real process.
    pub fn push_process_status(&mut self, text: &str, event: Option<protocol::ProcessStatusEvent>) {
        self.app
            .push_block(smelt_core::transcript_model::Block::ProcessStatus {
                text: text.to_string(),
                event,
            });
    }

    /// Push a `Block::Mode` into the transcript without restoring a fixture.
    pub fn push_mode_block(&mut self, text: &str, icon: &str, hl_group: &str) {
        self.app
            .push_block(smelt_core::transcript_model::Block::Mode {
                text: text.to_string(),
                icon: icon.to_string(),
                hl_group: hl_group.to_string(),
            });
    }

    /// Push a `Block::CodeLine` into the transcript.
    pub fn push_code_line(&mut self, content: &str, lang: &str) {
        self.app
            .push_block(smelt_core::transcript_model::Block::CodeLine {
                content: content.to_string(),
                lang: lang.to_string(),
            });
    }

    /// Push an untyped process-status block into the transcript.
    pub fn push_process_status_text(&mut self, text: &str) {
        self.push_process_status(text, None);
    }

    /// Open a `Block::Exec` shell-escape block in the transcript with
    /// `command` as the header. Pair with
    /// `SourceEvent::ExecOutput`/`ExecDone` to stream output and close
    /// the block. The production path is `start_shell_escape`, which
    /// also spawns a real `sh -c`; stories don't want a subprocess, so
    /// the harness invokes the transcript hook directly.
    pub fn start_exec(&mut self, command: &str) {
        self.app.start_exec(command.to_string());
    }

    /// Cancel the active turn (or idle background tasks). Mirrors
    /// `EventOutcome::CancelAgent` → `discard_turn(true)`.
    pub fn cancel(&mut self) {
        self.app.discard_turn(crate::app::TurnEnd::Cancelled);
        self.drain_cmd();
    }

    /// Push text that has already been promoted into the active turn's request queue.
    pub fn steer(&mut self, text: &str) {
        if !text.is_empty() {
            self.app
                .queued_inputs
                .try_push_request(crate::app::QueuedInput::request_from_text(
                    text.to_string(),
                    text.to_string(),
                ));
        }
    }

    /// Remove up to `count` request-queued messages from the front.
    pub fn unsteer(&mut self, count: usize) {
        self.app.queued_inputs.drain_request_ack(count);
    }

    /// Send a `CallCoreTool` UiCommand to the engine channel.
    pub fn call_core_tool(
        &mut self,
        tool_name: &str,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        self.app
            .core
            .engine
            .send(protocol::UiCommand::CallCoreTool {
                request_id: 1,
                parent_call_id: String::new(),
                tool_name: tool_name.to_string(),
                args,
            });
        self.drain_cmd();
    }

    /// Change the active agent mode.
    pub fn set_agent_mode(&mut self, mode: AgentMode) {
        self.app.core.config.mode = mode;
    }

    /// Push an `assistant` text block onto the transcript history so
    /// flows that read message history see a multi-turn conversation.
    pub fn push_assistant_text(&mut self, text: &str) {
        self.app
            .core
            .session
            .history
            .push(protocol::HistoryItem::Assistant(
                protocol::AssistantStep::terminal(
                    Some(protocol::Content::Text(text.to_string())),
                    None,
                    Vec::new(),
                ),
            ));
    }
}
