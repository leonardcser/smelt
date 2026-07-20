//! App-level story context.
//!
//! `StoryCtx` (in `mod.rs`) drives the bare `Ui`; `AppStoryCtx` drives
//! the full `TuiApp` through `TestApp`, so stories can feed engine
//! events and snapshot the composed transcript / prompt / status frame
//! the user actually sees.
//!
//! Background: `TestApp::render_normal` writes ANSI to real stdout as a
//! side effect, but it also composes the layer tree and updates the
//! `Ui` snapshot buffer in the same pass. Snapshotting via
//! `Ui::snapshot` after `render_normal` therefore returns the rendered
//! frame; the stdout noise is captured (and discarded) by nextest.
//!
//! Snapshot files use the same `.snap` / `.styles.snap` shape and
//! `.step-N` sequence convention as `StoryCtx`, so the existing viewer
//! at `examples/stories.rs` lists them without changes.
//!
//! Naming: every app-level story file lives under `stories/app_*.rs`
//! so its snapshots are namespaced (`app_transcript::…`) and can't
//! collide with the Ui-level snapshot space.
#![allow(dead_code)]

use insta::{assert_snapshot, with_settings};
use protocol::{EngineEvent, TokenUsage, ToolOutcome};
use tui::app::test_harness::TestApp;
use tui::smelt_edit::SnapshotFrame;

pub struct AppStoryCtx {
    app: TestApp,
    name: String,
    snapshot_index: u32,
    turn_id: u64,
    call_counter: u64,
}

impl AppStoryCtx {
    pub fn new(name: &str) -> Self {
        let app = TestApp::builder().without_model().build();
        let cwd = std::path::Path::new(app.cwd_str());
        let _ = std::fs::create_dir_all(cwd);
        let _ = std::env::set_current_dir(cwd);
        Self {
            app,
            name: name.to_string(),
            snapshot_index: 0,
            turn_id: 0,
            call_counter: 0,
        }
    }

    pub fn set_viewport(&mut self, w: u16, h: u16) {
        self.app.set_terminal_size(w, h);
    }

    pub fn restrict_permissions_to_cwd(&mut self) {
        assert_eq!(
            self.turn_id, 0,
            "configure permissions before starting a turn"
        );
        let mut permissions = self.app.app.core.permissions.snapshot().as_ref().clone();
        permissions.set_workspace(std::path::PathBuf::from(self.app.cwd_str()));
        permissions.set_restrict_to_workspace(true);
        self.app.app.core.permissions.replace(permissions);
    }

    pub fn approve_tool_for_session(&mut self, tool: &str) {
        let approvals = self.app.app.core.permissions.approvals();
        approvals
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .add_session_tool(tool, Vec::new());
    }

    /// Expand the active root-docked dialog while retaining a small transcript
    /// viewport above it.
    pub fn expand_active_dialog_to_max_height(&mut self) {
        self.run_lua(
            "assert(smelt.dialog.current(), 'active docked dialog exists').toggle_expanded()",
        )
    }

    /// Set the configured context window so prompt-bar token percentages
    /// render in stories.
    pub fn set_context_window(&mut self, context_window: Option<u32>) {
        self.app.set_context_window(context_window);
    }

    /// Install a usable synthetic model for stories that exercise model-backed
    /// APIs. Other stories intentionally retain the no-model baseline.
    pub fn use_test_model(&mut self) {
        let model = smelt_core::config::ResolvedModel {
            key: "test/test-model".into(),
            provider_name: "test".into(),
            model_name: "test-model".into(),
            display_name: None,
            api_base: "https://example.invalid/v1".into(),
            api_key_env: String::new(),
            provider_type: "openai-compatible".into(),
            config: protocol::ModelConfig {
                name: Some("test-model".into()),
                ..Default::default()
            },
        };
        self.app.app.core.config.available_models = vec![model.clone()];
        self.app.app.core.config.model_selection = smelt_core::ModelSelectionState {
            requested_key: Some(model.key.clone()),
            requested_by: smelt_core::ModelSelectionSource::FirstAvailable,
            active: Some(smelt_core::ActiveModel::from_resolved(&model)),
        };
    }

    /// Begin an agent turn. Required before `engine(...)` events route
    /// through the active-turn dispatch path (transcript writes, tool
    /// tracking). Idempotent.
    pub fn start_turn(&mut self) {
        if self.turn_id == 0 {
            self.turn_id = 1;
            self.app.start_turn(self.turn_id);
        }
    }

    /// Feed one engine event. Auto-starts a turn so callers don't have
    /// to remember.
    pub fn engine(&mut self, ev: EngineEvent) {
        self.start_turn();
        self.app
            .feed_one(tui::app::test_harness::SourceEvent::engine(ev));
    }

    /// Push a `Block::Compacted` summary block - the same committed marker
    /// inserted after a successful compaction checkpoint. Use this to
    /// snapshot the final compaction chrome without driving a real
    /// `engine.ask` round-trip.
    pub fn push_compacted(&mut self, summary: &str) {
        self.app.push_compacted(summary);
    }

    /// Push the transient block shown while the compact plugin streams a
    /// checkpoint summary. The real plugin rewrites this block on each delta
    /// and `smelt.session.checkpoint` replaces it with a compacted marker.
    pub fn push_compaction_preview(&mut self, summary: &str) {
        self.app.push_compaction_preview(summary);
    }

    /// Push a typed background-process completion status block. This drives the
    /// same transcript block shape produced by the live background process
    /// registry without spawning a subprocess in the story.
    pub fn push_background_process_completed(&mut self, id: &str, exit_code: Option<i32>) {
        let event = protocol::ProcessStatusEvent::background_process_completed(id, exit_code);
        let text = event.display_text();
        self.app.push_process_status(&text, Some(event));
    }

    pub fn push_process_status_text(&mut self, text: &str) {
        self.app.push_process_status_text(text);
    }

    pub fn push_mode_block(&mut self, text: &str, icon: &str, hl_group: &str) {
        self.app.push_mode_block(text, icon, hl_group);
    }

    pub fn push_code_line(&mut self, content: &str, lang: &str) {
        self.app.push_code_line(content, lang);
    }

    /// Run a shell-escape (`Block::Exec`) lifecycle: open the block,
    /// stream `output` line by line, then close. Goes through the same
    /// `start_exec` / `append_exec_output` / `finish_exec` /
    /// `finalize_exec` pipeline the live `!cmd` flow uses, so the
    /// rendered block in the snapshot is byte-identical to what users
    /// see (chrome bar, `!` accent, captured output).
    pub fn exec_with_output(&mut self, command: &str, output: &str, exit_code: Option<i32>) {
        self.app.start_exec(command);
        for line in output.lines() {
            self.app
                .feed_one(tui::app::test_harness::SourceEvent::ExecOutput(
                    line.to_string(),
                ));
        }
        self.app
            .feed_one(tui::app::test_harness::SourceEvent::ExecDone(exit_code));
    }

    /// Drive a tool call lifecycle: `ToolStarted(args)` immediately
    /// followed by `ToolFinished(result)`. Hides the boilerplate around
    /// allocating the args map, picking a `call_id`, and assembling the
    /// `ToolOutcome`. Stories that need the pending-only state should
    /// use [`tool_started`] (and never call `tool_finished`); stories
    /// that need a custom `metadata` payload should use
    /// [`tool_call_with_metadata`].
    pub fn tool_call(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        elapsed_ms: Option<u64>,
    ) {
        self.tool_call_full(tool_name, args, content, false, None, elapsed_ms);
    }

    /// Tool call lifecycle with an `is_error = true` result. Same shape
    /// as [`tool_call`] but flips the error flag so tool render paths
    /// that branch on `output.is_error` get exercised.
    pub fn tool_call_error(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        elapsed_ms: Option<u64>,
    ) {
        self.tool_call_full(tool_name, args, content, true, None, elapsed_ms);
    }

    /// Tool call lifecycle with structured `metadata`. Used by tools
    /// whose `render` callback dispatches on metadata fields (e.g.
    /// `notebook_edit` reads `edit_mode`, `old_source`, `new_source`).
    pub fn tool_call_with_metadata(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        metadata: serde_json::Value,
        elapsed_ms: Option<u64>,
    ) {
        self.tool_call_full(tool_name, args, content, false, Some(metadata), elapsed_ms);
    }

    /// Emit only `ToolStarted` - the pending state. Use this when the
    /// snapshot should capture the streaming/spinner chrome before any
    /// result arrives.
    pub fn tool_started(&mut self, tool_name: &str, args: &[(&str, serde_json::Value)]) {
        let call_id = self.next_call_id(tool_name);
        self.engine(EngineEvent::ToolStarted {
            call_id,
            tool_name: tool_name.into(),
            args: args
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        });
    }

    /// Emit a display-only streaming tool-call draft. The final tool
    /// execution path is still driven by `tool_started` / `tool_finished`.
    pub fn tool_draft(&mut self, tool_name: &str, args_json: &str) {
        self.call_counter += 1;
        let stream_id = format!("draft-{}", self.call_counter);
        let call_id = format!("{tool_name}-draft-{}", self.call_counter);
        self.engine(EngineEvent::ToolCallDraftStarted {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
        });
        self.engine(EngineEvent::ToolCallDraftDelta {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
            delta: args_json.into(),
        });
        self.engine(EngineEvent::ToolCallDraftFinished {
            stream_id,
            call_id,
            tool_name: tool_name.into(),
            arguments: args_json.into(),
        });
    }

    /// Emit one in-flight streaming tool-call draft delta without the finished
    /// event. This captures the state users see while arguments are still
    /// arriving from the provider.
    pub fn tool_draft_delta(&mut self, tool_name: &str, args_json_delta: &str) {
        self.call_counter += 1;
        let stream_id = format!("draft-{}", self.call_counter);
        let call_id = format!("{tool_name}-draft-{}", self.call_counter);
        self.engine(EngineEvent::ToolCallDraftStarted {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
        });
        self.engine(EngineEvent::ToolCallDraftDelta {
            stream_id,
            call_id: Some(call_id),
            tool_name: Some(tool_name.into()),
            delta: args_json_delta.into(),
        });
    }

    fn tool_call_full(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        is_error: bool,
        metadata: Option<serde_json::Value>,
        elapsed_ms: Option<u64>,
    ) {
        let call_id = self.next_call_id(tool_name);
        self.engine(EngineEvent::ToolStarted {
            call_id: call_id.clone(),
            tool_name: tool_name.into(),
            args: args
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        });
        self.engine(EngineEvent::ToolFinished {
            call_id,
            result: ToolOutcome {
                content: content.into(),
                is_error,
                metadata,
            },
            elapsed_ms,
        });
    }

    pub fn tool_rejected(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        is_error: bool,
        summary: protocol::StyledLines,
        elapsed_ms: Option<u64>,
    ) {
        let call_id = self.next_call_id(tool_name);
        self.engine(EngineEvent::ToolRejected {
            call_id,
            tool_name: tool_name.into(),
            args: args
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            summary,
            result: ToolOutcome {
                content: content.into(),
                is_error,
                metadata: None,
            },
            elapsed_ms,
        });
    }

    fn next_call_id(&mut self, tool_name: &str) -> String {
        self.call_counter += 1;
        format!("{tool_name}-{}", self.call_counter)
    }

    /// Drive a real permission flow. Invokes the tool's `summary(args)`
    /// Lua callback (the same path the engine uses in production) so
    /// the dialog header carries the tool's syntax-highlighted summary
    /// instead of a hand-rolled plain string. The dialog then renders
    /// the tool's `preview(args)` body through the real Lua → buffer
    /// pipeline.
    pub fn request_permission(
        &mut self,
        tool_name: &str,
        args: std::collections::HashMap<String, serde_json::Value>,
        approval_patterns: Vec<String>,
    ) {
        let summary = self.app.app.lua.tool_summary(tool_name, &args);
        self.engine(EngineEvent::RequestPermission {
            request_id: 1,
            call_id: "call-1".into(),
            tool_name: tool_name.into(),
            args,
            approval_patterns,
            summary,
        });
    }

    /// Write `contents` to `<cwd>/<rel_path>` and return the relative
    /// path (`./<rel_path>` form), suitable for direct use as a tool
    /// argument. Use this from dialog stories that exercise tool
    /// previews that read the file (notebook_edit, edit_file with
    /// file-based staleness checks). The fixture lives under the test
    /// cwd (`<HOME>/cwd`) so `smelt.path.display` collapses it to a
    /// clean relative path in the dialog header and snapshots stay
    /// byte-stable across machines.
    pub fn write_fixture(&self, rel_path: &str, contents: &str) -> String {
        let cwd = std::path::PathBuf::from(self.app.cwd_str());
        let path = cwd.join(rel_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, contents).expect("write story fixture");
        path.to_string_lossy().into_owned()
    }

    /// Working directory the live `TuiApp` captured at construction.
    /// Stories that seed persisted-session fixtures must match this
    /// into `meta.json::cwd` so the resume dialog's default
    /// workspace filter keeps the seeded entries visible.
    pub fn app_cwd(&self) -> &str {
        self.app.cwd_str()
    }

    /// Write a canonical database-backed session fixture with a derived
    /// `meta.json` cache so `smelt.session.list()` exercises its production path.
    pub fn write_session_meta(&self, meta: &smelt_core::session::SessionMeta) {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .expect("XDG_STATE_HOME set by test harness");
        let dir = state_home.join("smelt").join("sessions").join(&meta.id);
        std::fs::create_dir_all(&dir).expect("create session fixture dir");
        let mut db = smelt_store::SessionDb::open(dir.join("session.db"))
            .expect("create session fixture database");
        let history_len = meta.history_len.unwrap_or_default();
        let history = (0..history_len)
            .map(|idx| {
                protocol::HistoryItem::user(protocol::Content::text(format!(
                    "storybook session fixture row {idx}"
                )))
            })
            .collect::<Vec<_>>();
        db.apply_session_commit(&smelt_store::SessionCommit {
            session_id: meta.id.clone(),
            expected: smelt_store::StoreHead::default(),
            identity: smelt_store::SessionIdentity {
                id: meta.id.clone(),
                created_at: i64::try_from(meta.created_at_ms).expect("fixture created_at fits i64"),
                parent_id: meta.parent_id.clone(),
            },
            metadata: smelt_store::SessionMetadata {
                title: meta.title.clone(),
                slug: meta.slug.clone(),
                first_user_message: meta.first_user_message.clone(),
                cwd: meta.cwd.clone(),
                mode: meta.mode.clone(),
                reasoning_effort: meta
                    .reasoning_effort
                    .map(|effort| effort.label().to_string()),
                model: meta.model.clone(),
                fast_mode: meta.fast_mode,
                accounting_json: None,
                checkpoint_json: None,
                context_tokens: meta.context_tokens.map(u64::from),
                context_tokens_history_len: None,
                display_context_tokens: meta.context_tokens.map(u64::from),
                session_cost_usd: smelt_store::SessionCostUsd::new(0.0)
                    .expect("valid fixture cost"),
                updated_at: i64::try_from(meta.updated_at_ms).expect("fixture updated_at fits i64"),
            },
            history: smelt_store::HistorySuffix {
                start: smelt_store::HistoryIndex::ZERO,
                final_len: smelt_store::HistoryLen::new(
                    u64::try_from(history_len).expect("fixture history length fits u64"),
                ),
                items: history,
            },
            side_tables: smelt_store::SideTableSuffixes::default(),
            descriptors: None,
        })
        .expect("write canonical session fixture");
        let meta_json = serde_json::to_string(meta).expect("serialize session fixture metadata");
        // COMPAT(session-derived-sidecar-exports): fixture for the alpha list cache reader.
        std::fs::write(dir.join("meta.json"), meta_json).expect("write session meta fixture");
    }

    /// Commit a fresh Lua generation through the production reload pipeline.
    pub fn reload_lua(&mut self) {
        self.app.reload_lua();
    }

    /// Type a string into the prompt as individual key events.
    pub fn type_prompt(&mut self, s: &str) {
        self.app.type_text(s);
    }

    /// Press `ctrl+s` to toggle the prompt stash. Use this to drive the
    /// stash chrome (the `◌ Stashed` row) from a story.
    pub fn stash_prompt(&mut self) {
        self.app.press_mod(
            crossterm::event::KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        );
    }

    /// Press Enter and pump Lua tasks so dialog submit handlers that resolve a
    /// coroutine can open their follow-up UI before the next snapshot.
    pub fn press_enter(&mut self) {
        self.app.press(crossterm::event::KeyCode::Enter);
        for _ in 0..4 {
            self.app
                .feed_one(tui::app::test_harness::SourceEvent::LuaWakeup);
        }
    }

    /// Press a plain character key.
    pub fn press_char(&mut self, ch: char) {
        self.app.press(crossterm::event::KeyCode::Char(ch));
        for _ in 0..4 {
            self.app
                .feed_one(tui::app::test_harness::SourceEvent::LuaWakeup);
        }
    }

    /// Press Tab and pump Lua callbacks so focus-changing dialog keymaps settle
    /// before the next synthetic input.
    pub fn press_tab(&mut self) {
        self.app.press(crossterm::event::KeyCode::Tab);
        self.pump_lua();
    }

    /// Promote the oldest queued next-turn message into the next-request queue.
    pub fn promote_next_queued_message(&mut self) {
        self.app.press(crossterm::event::KeyCode::Enter);
    }

    /// Push a synthetic queued user message. In production these arrive
    /// by pressing Enter on the prompt while a turn is active; the
    /// harness side-channels them straight onto `app.queued_inputs`.
    /// Auto-starts a turn so the top bar's `prompt.queued()` accessor
    /// (which gates on `agent.is_some() || busy`) surfaces the entries.
    pub fn push_queued_message(&mut self, text: &str) {
        self.start_turn();
        self.app.push_queued_message(text.to_string());
    }

    /// Seed a prompt-history entry so reverse-history picker stories can open
    /// without submitting a real engine turn.
    pub fn push_history_entry(&mut self, text: &str) {
        self.app.push_history_entry(text.to_string());
    }

    /// Seed model picker entries in the live config. Each tuple is
    /// `(provider, model_name, provider_type)`.
    pub fn seed_models(&mut self, models: &[(&str, &str, &str)]) {
        self.app.app.core.config.available_models = models
            .iter()
            .map(
                |(provider, model, provider_type)| smelt_core::config::ResolvedModel {
                    key: format!("{provider}/{model}"),
                    provider_name: (*provider).to_string(),
                    model_name: (*model).to_string(),
                    display_name: None,
                    api_base: format!("https://{provider}.example/v1"),
                    api_key_env: format!("{}_API_KEY", provider.to_ascii_uppercase()),
                    provider_type: (*provider_type).to_string(),
                    config: smelt_core::config::ModelConfig {
                        name: Some((*model).to_string()),
                        ..Default::default()
                    },
                },
            )
            .collect();
    }

    /// Execute a Lua snippet against the embedded runtime. Used by
    /// dialog stories that need to seed state or invoke a primitive
    /// directly.
    pub fn run_lua(&mut self, snippet: &str) {
        let ok = self.app.run_lua(snippet);
        assert!(ok, "story Lua snippet failed");
    }

    /// Pump spawned Lua coroutines to their next yield point.
    pub fn pump_lua(&mut self) {
        for _ in 0..4 {
            self.app
                .feed_one(tui::app::test_harness::SourceEvent::LuaWakeup);
        }
    }

    /// Seed the latest provider-reported context token count through the
    /// normal engine event path so prompt-bar stories exercise the live
    /// bookkeeping.
    pub fn set_context_tokens(&mut self, context_tokens: u32) {
        self.engine(EngineEvent::TokenUsage {
            usage: TokenUsage {
                context_tokens: Some(context_tokens),
                ..TokenUsage::default()
            },
            tokens_per_sec: None,
            cost_usd: None,
            background: false,
        });
    }

    /// Seed a user turn on the transcript. Required by `/rewind` (and
    /// any other flow that reads `smelt.session.turns()`).
    pub fn push_user_turn(&mut self, text: &str) {
        self.app.push_user_block(text);
    }

    /// Seed a command-marked user turn on the transcript.
    pub fn push_command_turn(&mut self, text: &str) {
        self.app.push_command_block(text);
    }

    /// Seed an assistant message on `session.messages`. Required by
    /// flows that read history (e.g. `/btw`).
    pub fn push_assistant_text(&mut self, text: &str) {
        self.app.push_assistant_text(text);
    }

    /// Smallest pending `smelt.engine.ask` callback id, if any. Used
    /// by stories that drive `/btw` (or any other ask-callback flow)
    /// to feed a matching `EngineAskResponse` once the spawned
    /// coroutine has yielded and the callback is registered.
    pub fn pending_ask_id(&self) -> Option<u64> {
        self.app.pending_ask_id()
    }

    /// Show a notification toast via `smelt.notify` and pump the render
    /// so the toast overlay is created before the next snapshot.
    pub fn notify(&mut self, body: &str, source: Option<&str>) {
        let source = source.unwrap_or("story");
        let snippet = format!("smelt.notify.info({body:?}, {source:?})");
        self.run_lua(&snippet);
        for _ in 0..2 {
            self.app
                .feed_one(tui::app::test_harness::SourceEvent::LuaWakeup);
        }
    }

    /// Run a slash command as if the user typed it in the cmdline.
    /// Goes through the real `smelt.cmd.run` path → command handler →
    /// `smelt.spawn` → `smelt.dialog.open` (yielding coroutine). After
    /// dispatch we pump the task runtime so the spawned coroutine
    /// progresses to its first yield (the dialog open), which is where
    /// stories want to snapshot.
    pub fn run_command(&mut self, line: &str) {
        // mlua's lua-to-string round-trip: `string.format("%q", line)`
        // would also work, but ASCII command lines don't need escaping
        // beyond `\"`. Stories pass plain ASCII so a debug-format quote
        // matches Lua's string literal syntax.
        let snippet = format!("smelt.cmd.run({line:?})");
        self.run_lua(&snippet);
        // The handler ran synchronously and called `smelt.spawn`, which
        // enqueued a coroutine on the task runtime. Pump the runtime to
        // run it until it yields at `smelt.dialog.open`.
        for _ in 0..4 {
            self.app
                .feed_one(tui::app::test_harness::SourceEvent::LuaWakeup);
        }
    }

    fn frame(&mut self) -> SnapshotFrame {
        self.app.render_to_frame()
    }

    pub fn frame_text(&mut self) -> String {
        self.frame().text()
    }

    pub fn assert_snapshot(&mut self) {
        let suffix = if self.snapshot_index == 0 {
            String::new()
        } else {
            format!(".step-{}", self.snapshot_index)
        };
        self.snapshot_index += 1;
        self.assert_snapshot_with_suffix(&suffix);
    }

    pub fn assert_snapshot_named(&mut self, name: &str) {
        self.assert_snapshot_with_suffix(&format!(".{name}"));
    }

    fn assert_snapshot_with_suffix(&mut self, suffix: &str) {
        let frame = self.frame();
        let text_name = format!("{}{}", self.name, suffix);
        let style_name = format!("{}{}.styles", self.name, suffix);
        with_settings!({
            prepend_module_to_snapshot => false,
            snapshot_path => "snapshots",
        }, {
            assert_snapshot!(text_name, frame.text());
            assert_snapshot!(style_name, frame.styles_text());
        });
    }
}

#[macro_export]
macro_rules! app_story {
    ($name:ident, |$ctx:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            let snapshot_id = format!(
                "{}::{}",
                module_path!().rsplit("::").next().unwrap_or("app_story"),
                stringify!($name),
            );
            let mut __sb_appctx = $crate::storybook::app_ctx::AppStoryCtx::new(&snapshot_id);
            let $ctx: &mut $crate::storybook::app_ctx::AppStoryCtx = &mut __sb_appctx;
            $body
        }
    };
}
