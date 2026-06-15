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
        let app = TestApp::builder().build();
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

    /// Set the configured context window so prompt-bar token percentages
    /// render in stories.
    pub fn set_context_window(&mut self, context_window: Option<u32>) {
        self.app.set_context_window(context_window);
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
            .feed_one(tui::app::test_harness::SourceEvent::Engine(ev));
    }

    /// Push a `Block::Compacted` summary block - the same block the
    /// bundled compact plugin emits between turns. Use this to
    /// snapshot the compaction chrome without driving a real
    /// `engine.ask` round-trip.
    pub fn push_compacted(&mut self, summary: &str) {
        self.app.push_compacted(summary);
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

    /// Write a `meta.json` fixture for a persisted session so
    /// `smelt.session.list()` returns it. Goes under
    /// `<XDG_STATE_HOME>/smelt/sessions/<id>/meta.json` to match the
    /// production `state_dir()` layout used by `list_sessions`.
    pub fn write_session_meta(&self, id: &str, meta_json: &str) {
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .expect("XDG_STATE_HOME set by test harness");
        let dir = state_home.join("smelt").join("sessions").join(id);
        std::fs::create_dir_all(&dir).expect("create session fixture dir");
        std::fs::write(dir.join("meta.json"), meta_json).expect("write session meta fixture");
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

    /// Execute a Lua snippet against the embedded runtime. Used by
    /// dialog stories that need to seed state or invoke a primitive
    /// directly.
    pub fn run_lua(&mut self, snippet: &str) {
        let ok = self.app.run_lua(snippet);
        assert!(ok, "story Lua snippet failed");
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

    pub fn assert_snapshot(&mut self) {
        let frame = self.frame();
        let suffix = if self.snapshot_index == 0 {
            String::new()
        } else {
            format!(".step-{}", self.snapshot_index)
        };
        self.snapshot_index += 1;
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
        #[test]
        fn $name() {
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
