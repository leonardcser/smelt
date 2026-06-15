use super::test_harness::*;
use crate::app::AppFocus;
use crate::smelt_edit::{VimMode, WinId};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use protocol::{AgentMode, EngineEvent};
use std::time::Duration;

mod compaction;
mod dialogs;
mod lua_reload;
mod misc;
mod mouse;
mod picker;
mod process_status;
mod prompt;
mod resources;
mod search;
mod transcript_bench;
mod vim;

fn ask_messages(cmds: Vec<protocol::UiCommand>) -> Vec<(String, Vec<protocol::Message>)> {
    cmds.into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::EngineAsk {
                system, messages, ..
            } => Some((system, messages)),
            _ => None,
        })
        .collect()
}

fn user_message(text: &str) -> protocol::Message {
    protocol::Message::user(protocol::Content::text(text))
}

fn assistant_message(text: &str) -> protocol::Message {
    protocol::Message::assistant(Some(protocol::Content::text(text)), None, None)
}

fn replacement_from_decision(
    decision: engine::HostRequestDecision,
    context: &str,
) -> Vec<protocol::Message> {
    match decision {
        engine::HostRequestDecision::Replace(messages) => messages,
        other => panic!("{context}: expected replacement, got {other:?}"),
    }
}

fn drive_lua_tasks(app: &mut TestApp) {
    for _ in 0..4 {
        app.feed_one(SourceEvent::LuaWakeup);
    }
}

fn respond_ask_with_text(app: &mut TestApp, id: u64, text: &str) {
    let _g = crate::lua::install_app_ptr(&mut app.app);
    app.app
        .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
            id,
            message: Some(protocol::Message::assistant(
                Some(protocol::Content::text(text)),
                None,
                None,
            )),
            error: None,
        });
    app.app.drive_lua_tasks();
}

fn respond_pending_ask_with_text(app: &mut TestApp, text: &str) {
    respond_ask_with_text(app, app.pending_ask_id().expect("pending ask id"), text);
}

fn publish_input_submit(app: &mut TestApp, text: &str) {
    let _g = crate::lua::install_app_ptr(&mut app.app);
    app.app.bump_epoch("input_epoch");
    app.app
        .core
        .cells
        .set_dyn("input_submit", std::rc::Rc::new(text.to_string()));
    app.app.pump_lua();
}

fn publish_turn_end(app: &mut TestApp) {
    let _g = crate::lua::install_app_ptr(&mut app.app);
    app.app.core.cells.set_dyn(
        "turn_end",
        std::rc::Rc::new(smelt_core::cells::TurnEnd { cancelled: false }),
    );
    app.app.pump_lua();
}

fn publish_history_delta(app: &mut TestApp, kind: &str) {
    let _g = crate::lua::install_app_ptr(&mut app.app);
    app.app.publish_history_delta(kind);
    app.app.pump_lua();
}

fn engine_ask_ids(cmds: Vec<protocol::UiCommand>) -> Vec<u64> {
    cmds.into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::EngineAsk { id, .. } => Some(id),
            _ => None,
        })
        .collect()
}

fn respond_pending_ask_with_tool_call(app: &mut TestApp, call_id: &str, name: &str) {
    let _g = crate::lua::install_app_ptr(&mut app.app);
    app.app
        .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
            id: app.pending_ask_id().expect("pending ask id"),
            message: Some(protocol::Message::assistant(
                None,
                None,
                Some(vec![protocol::ToolCall::new(
                    call_id.into(),
                    protocol::FunctionCall {
                        name: name.into(),
                        arguments: "{}".into(),
                    },
                )]),
            )),
            error: None,
        });
    app.app.drive_lua_tasks();
}

fn stub_btw_ui(app: &mut TestApp) {
    let _g = crate::lua::install_app_ptr(&mut app.app);
    app.app
        .lua
        .lua
        .load(
            r#"
                smelt.buf.new = function()
                  return {
                    source = function() end,
                  }
                end
                smelt.timer.set = function() end
                smelt.dialog.content = function() return {} end
                smelt.dialog.open = function() end
                smelt.spinner.glyph = function() return "*" end
                smelt.spinner.period_ms = function() return 1 end
                "#,
        )
        .exec()
        .expect("stub /btw ui");
}

// ── Resource invariants: per-event allocation tracking ────────────

// `feed_one` captures a non-negative allocation delta on every event,
// and a `Tick` (pure clock advance) allocates next to nothing - the
// floor sanity-check that the counting allocator is actually wired
// into the test binary. If `Counting` regresses to `System`, the
// snapshots stay zero and this still passes; pair with the keystroke
// budget test below to catch that.

// One keystroke through the dispatch chain stays well under the default
// budget. If this trips, either we have a real per-keystroke regression
// or the budget needs revisiting - both worth noticing.

// ── Escape sequence semantics ────────────────────────────────────

// ── Ctrl-C semantics ───────────────────────────────────────────

// ── Cmdline open/close (vim-gated) ──────────────────────────────

// Regression: typing into the cmdline grows its line-based buffer
// and the cmdline window's `cpos` past `source.len()` (the cmdline
// stays empty because content lives in `lines`). The invariant
// scoping must recognize this as a line-based buffer and skip the
// source-bounded cursor check rather than fire spuriously.

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

// Regression: a prompt-docked picker whose `scroll_top` lands at
// `max_scroll` (cursor at the bottom in reversed mode) was getting
// clobbered by tail-scroll resolution on the first frame - the new
// leaf has no viewport rect yet, so `max_scroll = total_rows - 0`
// snapped `scroll_top` past the end and the picker rendered blank
// until the user typed a character to force a re-layout.

// ── Vim mode transitions ────────────────────────────────────────

fn prompt_content_cell(app: &mut TestApp) -> (u16, u16) {
    app.app.render_normal(false);
    let vp = app
        .app
        .ui
        .win(crate::app::PROMPT_WIN)
        .and_then(|w| w.viewport)
        .expect("prompt viewport after render");
    let pad_left = app
        .app
        .ui
        .win(crate::app::PROMPT_WIN)
        .map(|w| w.config.gutters.pad_left)
        .unwrap_or_default();
    (
        vp.rect.top,
        vp.rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(pad_left),
    )
}

fn row_document_transcript_app(rows: usize, vim: bool) -> TestApp {
    let mut app = TestApp::builder().with_vim(vim).build();
    app.app.handle_resize(80, 16);
    for i in 0..rows {
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: format!("row {i:03} alpha beta"),
            });
    }
    app.render_silent();
    app.app.app_focus = AppFocus::Content;
    app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    let win = app.app.transcript_win_mut();
    win.set_vim_enabled(vim);
    win.set_vim_mode(VimMode::Normal);
    app
}

fn transcript_row_cursor_row(app: &TestApp) -> crate::smelt_edit::RowIndex {
    app.app
        .transcript_win()
        .row_cursor()
        .expect("row-document transcript cursor")
        .row
}

fn transcript_total_rows(app: &TestApp) -> crate::smelt_edit::RowIndex {
    let win = app.app.transcript_win();
    let buf = app.app.ui.buf(win.buf).expect("transcript buffer");
    win.scroll_row_total(buf)
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_prepare_probe_completes_and_preserves_turn() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.probe_compaction_prepare_request(1);
}

// ── Original picker suite continues ─────────────────────────────

// ── Named-resource hot-reload refresh ───────────────────────────

// Reproduces the perf_panel hot-reload flow: re-call `overlay.open`
// with the same `name` and a different `title` and assert the chrome
// title is updated in place (no close+reopen).

// `apply_window_opts` should only mutate fields that are present in
// opts. A named refresh that omits `wrap` must NOT silently reset
// wrap to its default - that would clobber the prior value.

// `buf.create({ name = ... })` and `win.open(buf, { name = ... })`
// should hand back the SAME ids when called twice with the same name.

// Re-opening a named overlay with a structurally different layout
// (leaf → vbox split) should replace the tree in place - not silently
// keep the old one.

// `smelt.state` entries for plugins that no longer touch them on
// reload should be swept by `smelt.__sweep_state()`.

// ── Full-cycle /reload integration ──────────────────────────────
//
// These tests drive `TuiApp::reload_lua()` end-to-end with a real
// `init.lua` on disk. Each test edits the file between reloads so
// the new module body re-runs and we can observe the surfaces that
// *should* survive (named bufs/wins/overlays, `smelt.state`) vs.
// the ones that should be replaced (titles, layout structure) vs.
// the ones that should be reaped (anonymous overlays).

fn read_overlay_title(app: &TestApp, name: &str) -> Option<String> {
    let id = app.app.ui.named_overlay(name)?;
    let ov = app.app.ui.overlay(id)?;
    Some(
        ov.layout
            .chrome()
            .title
            .as_ref()?
            .spans
            .iter()
            .map(|s| s.text.as_ref())
            .collect::<String>(),
    )
}

// Editing `init.lua` to change the overlay title and calling
// `reload_lua` should update the chrome title in place without
// destroying the OverlayId.

// Nested tables stashed in `smelt.state` must keep their identity
// (deep values intact) across `/reload`.

// `_bootstrap.lua` wraps `smelt.tools.register` to inject a default
// `summary`. The wrap must remain a *single* layer across many
// reloads - never re-wrap the previous wrap.

// Anonymous overlays (no `name`) must be reaped on reload; named
// ones survive.

// Named paint slots (`smelt.paint.register(fn, { name = "..." })`)
// must keep the same `PaintId` across `/reload` so surviving
// overlays / layouts that reference the id keep painting with the
// fresh closure. Anonymous slots get reaped.

// Find the single anonymous paint id (no name binding) currently
// registered. Used by paint-reload tests to track the throwaway
// slot across `/reload` without needing Lua-side reflection.
fn find_anon_paint(app: &crate::app::TuiApp) -> crate::smelt_edit::layout::PaintId {
    let reg = &app.paint_registry;
    let named: std::collections::HashSet<crate::smelt_edit::layout::PaintId> = ["probe.named"]
        .iter()
        .filter_map(|n| reg.id_by_name(n))
        .collect();
    reg.all_ids()
        .into_iter()
        .find(|id| !named.contains(id))
        .expect("anonymous paint id present")
}

// `lifecycle.on("ready", fn)` hooks must re-drain on `/reload` so
// plugins that subscribe to cells / open splash overlays / etc.
// re-wire themselves on every Lua-context bring-up. The fire
// passes `ctx = { kind = "launch" | "reload" }`.

// A `smelt.state(...)` slot that the new init.lua no longer
// references must be pruned by `smelt.__sweep_state` (called by
// `reload()` at the end of the cycle).

// **Single ledger** for "what does `/reload` clear?" Touches every
// Lua-side surface, triggers reload, asserts each is in the expected
// post-reload state. New `LuaShared` registries or TUI-side caches
// that hold Lua handles MUST add a check here - otherwise the
// reload contract is silently broken.

// In-flight `smelt.spawn` coroutines must be cancelled before
// `clear_lua_handles` wipes the registries they reference. After
// reload, the parked task should never resume - driving tasks
// produces nothing, the post-sleep side effect never runs.

// `/reload` (`smelt.engine.reload()`) used to refuse with
// "cannot reload while a modal dialog is open". We now dismiss
// the modal first so the parked dialog coroutine joins the rest
// of the in-flight tasks `clear_for_reload` cancels - symmetric
// with how reload already drops any other `smelt.spawn`. After
// reload, no modal is open and a fresh dialog opens cleanly.

#[tokio::test(flavor = "current_thread")]
async fn compaction_prepare_request_preserves_session_prefix_and_appends_summary_instruction() {
    let mut app = TestApp::builder().build();
    app.app.core.config.context_window = Some(100);
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u2")));

    let full_history = protocol::history_to_messages(&app.app.model_history());
    let expected_prefix = &full_history[..2];
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::PrepareRequest {
                messages: full_history.clone(),
                estimated_tokens: 200,
                reply: tx,
            });
    }

    let asks = ask_messages(app.drain_engine_sends());
    assert_eq!(asks.len(), 1, "compaction should issue one EngineAsk");
    let (system, messages) = &asks[0];
    assert_eq!(system, &app.app.assemble_system_prompt());
    assert_eq!(
            &messages[..expected_prefix.len()],
            expected_prefix,
            "initial compaction attempt must preserve the exact session prefix up to the current boundary"
        );
    let last_text = messages
        .last()
        .and_then(|m| m.content.as_ref())
        .map(|c| c.text_content())
        .expect("summary task");
    assert!(last_text.contains("CONTEXT CHECKPOINT COMPACTION"));
    assert!(last_text.contains("Under no circumstances use tools"));
    assert!(last_text.contains("# Goal"));

    respond_pending_ask_with_text(&mut app, "# Goal\nok");
    let replacement =
        replacement_from_decision(rx.await.expect("prepare_request reply"), "prepare_request");
    let replacement_text = replacement
        .first()
        .and_then(|m| m.content.as_ref())
        .map(|c| c.text_content());
    let expected = format!("{}\n# Goal\nok", engine::SUMMARY_PREFIX.trim_end());
    assert_eq!(replacement_text.as_deref(), Some(expected.as_str()));
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_prepare_request_keeps_active_turn_guard_current() {
    let mut app = TestApp::builder().build();
    app.app.core.config.context_window = Some(100);
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u2")));
    app.start_turn(42);

    let full_history = protocol::history_to_messages(&app.app.model_history());
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::PrepareRequest {
                messages: full_history,
                estimated_tokens: 200,
                reply: tx,
            });
    }

    assert_eq!(app.app.working.phase_label(), Some("compacting"));
    assert_eq!(ask_messages(app.drain_engine_sends()).len(), 1);

    respond_pending_ask_with_text(&mut app, "# Goal\nok");
    let replacement = replacement_from_decision(
        rx.await.expect("prepare_request reply"),
        "active-turn guard",
    );
    let replacement_text = replacement
        .first()
        .and_then(|m| m.content.as_ref())
        .map(|c| c.text_content());
    let expected = format!("{}\n# Goal\nok", engine::SUMMARY_PREFIX.trim_end());
    assert_eq!(replacement_text.as_deref(), Some(expected.as_str()));
    assert_eq!(app.app.working.phase_label(), Some("working"));
    assert!(app.agent_running());
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_context_limit_moves_boundary_earlier_on_context_window() {
    let mut app = TestApp::builder().build();
    let messages = vec![
        user_message("u1"),
        assistant_message("a1"),
        user_message("u2"),
        assistant_message("a2"),
        user_message("u3"),
    ];
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::RecoverFromContextLimit {
                messages: messages.clone(),
                reply: tx,
            });
    }

    let first = ask_messages(app.drain_engine_sends());
    assert_eq!(first.len(), 1);
    let first_messages = &first[0].1;
    assert_eq!(
        &first_messages[..4],
        &messages[..4],
        "keep_recent_groups=1 should compact everything before the last group"
    );

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id: app.pending_ask_id().expect("pending ask id"),
                message: None,
                error: Some(protocol::EngineAskError {
                    kind: protocol::EngineAskErrorKind::ContextWindow,
                    message: "too large".into(),
                }),
            });
    }

    let second = ask_messages(app.drain_engine_sends());
    assert_eq!(second.len(), 1);
    let second_messages = &second[0].1;
    assert_eq!(
        &second_messages[..3],
        &messages[..3],
        "retry should move the boundary one group earlier"
    );

    respond_pending_ask_with_text(&mut app, "# Goal\nok");
    let replacement = replacement_from_decision(rx.await.expect("recovery reply"), "recovery");
    assert_eq!(replacement.len(), 3);
    assert_eq!(replacement[1], messages[3]);
    assert_eq!(replacement[2], messages[4]);
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_context_limit_denies_tool_calls_without_moving_boundary() {
    let mut app = TestApp::builder().build();
    let messages = vec![
        user_message("u1"),
        assistant_message("a1"),
        user_message("u2"),
    ];
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::RecoverFromContextLimit {
                messages: messages.clone(),
                reply: tx,
            });
    }

    let first = ask_messages(app.drain_engine_sends());
    assert_eq!(first.len(), 1);
    let first_messages = first[0].1.clone();

    respond_pending_ask_with_tool_call(&mut app, "call-1", "read_file");

    let second = ask_messages(app.drain_engine_sends());
    assert_eq!(second.len(), 1);
    let second_messages = &second[0].1;
    assert_eq!(
        &second_messages[..first_messages.len()],
        first_messages.as_slice(),
        "tool denial retry must keep the same boundary prefix"
    );
    assert_eq!(
        second_messages[first_messages.len()].role,
        protocol::Role::Assistant
    );
    assert_eq!(
        second_messages[first_messages.len() + 1].role,
        protocol::Role::Tool
    );
    assert!(second_messages[first_messages.len() + 1].is_error);

    respond_pending_ask_with_text(&mut app, "# Goal\nok");
    let replacement = replacement_from_decision(rx.await.expect("recovery reply"), "recovery");
    assert_eq!(replacement.first().unwrap().role, protocol::Role::User);
}

#[tokio::test(flavor = "current_thread")]
async fn compaction_context_limit_returns_none_when_no_earlier_boundary_fits() {
    let mut app = TestApp::builder().build();
    let messages = vec![user_message("u1"), user_message("u2")];
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_host_call(engine::HostCall::RecoverFromContextLimit {
                messages,
                reply: tx,
            });
    }

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id: app.pending_ask_id().expect("pending ask id"),
                message: None,
                error: Some(protocol::EngineAskError {
                    kind: protocol::EngineAskErrorKind::ContextWindow,
                    message: "too large".into(),
                }),
            });
    }

    assert!(matches!(
        rx.await.expect("recovery reply"),
        engine::HostRequestDecision::Continue
    ));
}
