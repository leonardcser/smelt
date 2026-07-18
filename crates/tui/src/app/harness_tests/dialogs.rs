use super::*;

fn open_root_test_dialog(app: &mut TestApp) {
    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Review this action" })
        smelt.dialog.open_handle({
          panels = { { leaf = leaf, height = "fit" } },
          blocks_agent = true,
        })
        "#
    ));
}

fn assert_safe_dialog_fallback(app: &TestApp) {
    assert!(app.app.ui.split_rect(crate::app::PROMPT_WIN).is_none());
    let statusline = app
        .app
        .ui
        .named_win("smelt.statusline")
        .expect("statusline window");
    let status_rect = app
        .app
        .ui
        .split_rect(statusline)
        .expect("fallback keeps statusline mounted");
    assert_eq!(
        status_rect.top + status_rect.height,
        app.app.ui.terminal_size().1
    );
}

#[test]
fn root_dialog_replaces_composer_and_restores_prompt_state() {
    let mut app = TestApp::builder().build();
    app.type_text("draft response");
    assert_eq!(app.state().prompt_text, "draft response");

    open_root_test_dialog(&mut app);
    app.render_silent();

    assert!(app.state().active_modal.is_some());
    assert!(app.app.ui.split_rect(crate::app::PROMPT_WIN).is_none());
    assert_eq!(app.state().prompt_text, "draft response");
    assert!(!app.render_to_frame().text().contains("draft response"));

    assert!(app.run_lua("smelt.dialog.current().close()"));
    app.render_silent();

    assert!(app.state().active_modal.is_none());
    assert!(app.app.ui.split_rect(crate::app::PROMPT_WIN).is_some());
    assert_eq!(app.state().prompt_text, "draft response");
    assert!(app.render_to_frame().text().contains("draft response"));
}

#[test]
fn failed_root_dialog_open_does_not_pollute_dialog_stack() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Review this action" })
        local ok = pcall(function()
          smelt.dialog.open_handle({
            panels = { { leaf = leaf, height = "fit" } },
            min_height = "invalid",
          })
        end)
        assert(not ok, "invalid dialog constraint should fail")
        assert(smelt.dialog.current() == nil, "failed dialog leaked into stack")
        "#
    ));
    assert!(app.app.active_docked_dialog().is_none());
}

#[test]
fn root_dialog_can_disable_top_edge_resize() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Fixed dialog" })
        smelt.dialog.open_handle({
          panels = { { leaf = leaf, height = "fit" } },
          resizable = false,
        })
        "#
    ));

    let dialog = app.app.active_docked_dialog().expect("active dialog");
    assert!(
        !app.app
            .ui
            .docked_surface(dialog)
            .expect("docked surface")
            .resize_config()
            .top
    );
}

#[test]
fn root_dialog_uses_safe_fallback_when_custom_composer_omits_it() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function()
          return smelt.ui.layout.leaf(smelt.win.PROMPT)
        end)
        "#
    ));

    app.render_silent();

    assert_safe_dialog_fallback(&app);
    let dialog = app.app.active_docked_dialog().expect("active dialog");
    let root = app
        .app
        .ui
        .modal_leaves(
            app.app
                .ui
                .docked_surface(dialog)
                .expect("docked surface")
                .modal(),
        )
        .and_then(|leaves| leaves.first())
        .copied()
        .expect("dialog root");
    assert!(app.app.ui.split_rect(root).is_some());
}

#[test]
fn root_dialog_uses_safe_fallback_when_custom_composer_duplicates_it() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.win.PROMPT)
          end
          return smelt.ui.layout.vbox({
            { state.dialog, height = "fill" },
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));

    app.render_silent();

    assert_safe_dialog_fallback(&app);
}

#[test]
fn root_dialog_rejects_retained_stage_from_nested_dialog() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        local retained
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.win.PROMPT)
          end
          if not retained then
            retained = state.dialog
            return state.dialog
          end
          return smelt.ui.layout.vbox({
            { retained, height = "fill" },
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));

    open_root_test_dialog(&mut app);
    app.render_silent();

    assert_safe_dialog_fallback(&app);
}

#[test]
fn root_dialog_hides_and_restores_queued_input_chrome() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.type_text("queued follow-up");
    app.press(KeyCode::Enter);
    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(app.render_to_frame().text().contains("queued follow-up"));

    open_root_test_dialog(&mut app);

    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(!app.render_to_frame().text().contains("queued follow-up"));

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(app.render_to_frame().text().contains("queued follow-up"));
}

#[test]
fn expanding_root_dialog_preserves_focused_panel() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local first = smelt.dialog.content({ text = "First panel", interactive = true })
        local second = smelt.dialog.content({ text = "Second panel", interactive = true })
        smelt.dialog.open_handle({
          panels = {
            { leaf = first, height = "fit" },
            { leaf = second, height = "fit" },
          },
        })
        second:focus()
        "#
    ));
    app.render_silent();
    let focused = app.app.ui.focus().expect("second dialog panel focused");

    app.press_mod(KeyCode::Char('o'), KeyModifiers::CONTROL);

    assert_eq!(app.app.ui.focus(), Some(focused));
    assert!(app
        .app
        .active_docked_dialog()
        .and_then(|dialog| app.app.ui.docked_surface(dialog))
        .is_some_and(|dialog| dialog.expanded()));
}

#[test]
fn root_dialog_pauses_notification_visibility_and_ttl() {
    let mut app = TestApp::builder().build();
    app.app.notify_error("deferred notification".into());
    assert!(app.app.notification_win().is_some());

    open_root_test_dialog(&mut app);

    assert!(app.app.notification_win().is_none());
    assert!(app.app.suspended_notification.is_some());
    app.clock.advance(std::time::Duration::from_millis(
        crate::app::NOTIFICATION_TTL_MS * 2,
    ));
    assert!(!app.app.dismiss_expired_notification());

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert!(app.app.suspended_notification.is_none());
    assert!(app.app.notification_win().is_some());
}

#[test]
fn root_dialog_preserves_notification_ownership() {
    let mut app = TestApp::builder().build();
    let session_id = app.app.core.session.id.clone();
    app.app
        .notify_session_save_failure(&session_id, "database busy");

    open_root_test_dialog(&mut app);

    assert!(app
        .app
        .suspended_notification
        .as_ref()
        .is_some_and(|notification| {
            matches!(
                notification.owner.as_ref(),
                Some(crate::app::NotificationOwner::SessionPersistence(owner_session_id))
                    if owner_session_id == &session_id
            )
        }));

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert!(app.app.notification.as_ref().is_some_and(|notification| {
        matches!(
            notification.owner.as_ref(),
            Some(crate::app::NotificationOwner::SessionPersistence(owner_session_id))
                if owner_session_id == &session_id
        )
    }));
}

#[test]
fn readonly_dialog_arrows_move_without_vim() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha", "bravo", "charlie" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.open_handle({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.app.ui.focus().expect("dialog leaf focused");
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 0);

    app.press(KeyCode::Down);
    app.press(KeyCode::Right);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 1);

    app.press(KeyCode::Up);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 1);

    app.press(KeyCode::Left);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 0);
}

#[test]
fn vim_readonly_dialog_arrows_and_vim_motions_move_cursor() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha one", "bravo two", "charlie three", "delta four", "echo five", "foxtrot six" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.open_handle({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.app.ui.focus().expect("dialog leaf focused");

    app.press(KeyCode::Down);
    app.press(KeyCode::Right);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 1);

    app.type_char('j');
    app.type_char('l');
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_row(), 2);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 2);

    app.press(KeyCode::Up);
    app.press(KeyCode::Left);
    app.type_char('k');
    app.type_char('h');
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 0);

    app.type_char('w');
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 6);
    app.type_char('j');
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 6);
}

#[test]
fn vim_readonly_dialog_visual_modes_copy_character_and_whole_buffer() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma", "delta" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.open_handle({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.app.ui.focus().expect("dialog leaf focused");

    app.type_char('v');
    app.type_char('e');
    assert_eq!(
        app.app.ui.win(win_id).expect("window").vim_mode(),
        VimMode::Visual
    );
    app.type_char('y');
    assert_eq!(app.app.core.clipboard.kill_ring.current(), "alpha");
    assert_eq!(
        app.app.core.clipboard.kill_ring.last_clipboard_write(),
        Some("alpha")
    );
    assert_eq!(
        app.app.ui.win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );

    app.type_char('g');
    app.type_char('g');
    app.type_char('V');
    assert_eq!(
        app.app.ui.win(win_id).expect("window").vim_mode(),
        VimMode::VisualLine
    );
    app.type_char('G');
    app.type_char('y');

    assert_eq!(
        app.app.core.clipboard.kill_ring.current(),
        "alpha beta\ngamma\ndelta"
    );
    assert_eq!(
        app.app.core.clipboard.kill_ring.last_clipboard_write(),
        Some("alpha beta\ngamma\ndelta")
    );
    assert_eq!(
        app.app.ui.win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );
    assert!(
        app.app.ui.active_modal().is_some(),
        "copying must keep the dialog open"
    );
}

#[test]
fn vim_readonly_dialog_escape_exits_visual_before_dialog_keymap() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.open_handle({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
          keymaps = {
            { key = "<Esc>", on_press = function(ctx) ctx.close() end },
          },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.app.ui.focus().expect("dialog leaf focused");
    app.type_char('v');
    app.type_char('e');
    assert_eq!(
        app.app.ui.win(win_id).expect("window").vim_mode(),
        VimMode::Visual
    );
    assert!(
        app.app
            .ui
            .win(win_id)
            .and_then(|win| app
                .app
                .ui
                .buf(win.buf)
                .and_then(|buf| win.selection_range(buf)))
            .is_some(),
        "visual-mode dialog viewer should have a selection range"
    );

    app.press(KeyCode::Esc);

    assert!(
        app.app.ui.active_modal().is_some(),
        "Esc from Visual should not close the dialog"
    );
    assert_eq!(
        app.app.ui.win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );
}

#[test]
fn vim_readonly_dialog_visual_selection_is_painted() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.open_handle({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.app.ui.focus().expect("dialog leaf focused");
    app.type_char('v');
    app.type_char('e');
    assert!(
        !app.app
            .ui
            .win(win_id)
            .expect("window")
            .has_materialized_rows(),
        "dialog viewer should stay byte-backed after visual motions"
    );
    let frame = app.render_to_frame();
    let win = app.app.ui.win(win_id).expect("window");
    let rect = win.viewport.expect("dialog viewport").rect;
    let selection_bg = app.app.ui.theme().get("Visual").bg;
    let painted = (rect.top..rect.top + rect.height).any(|row| {
        (rect.left..rect.left + rect.width)
            .any(|col| frame.styles[row as usize][col as usize].bg == selection_bg)
    });

    assert!(painted, "visual selection should be painted inside dialog");
}
fn tool_result<'a>(app: &'a TestApp, call_id: &str) -> Option<(&'a str, bool)> {
    app.actions().iter().rev().find_map(|action| match action {
        Action::EngineSend(cmd) => match cmd.as_ref() {
            protocol::UiCommand::ToolResult {
                call_id: id,
                content,
                is_error,
                ..
            } if id == call_id => Some((content.as_str(), *is_error)),
            _ => None,
        },
        _ => None,
    })
}

fn confirm_test_permissions() -> smelt_core::permissions::Permissions {
    use smelt_core::permissions::rules::{RawModePerms, RawPerms, RawRuleSet, ToolDefaults};
    let mut modes = std::collections::HashMap::new();
    modes.insert(
        "apply".to_string(),
        RawModePerms {
            tools: RawRuleSet {
                allow: vec!["test_tool".into()],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    modes.insert(
        "deny".to_string(),
        RawModePerms {
            tools: RawRuleSet {
                deny: vec!["test_tool".into()],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    smelt_core::permissions::Permissions::from_raw(
        &RawPerms {
            default: RawModePerms::default(),
            modes,
        },
        &ToolDefaults::default(),
    )
}

fn confirm_req(request_id: u64) -> smelt_core::ConfirmRequest {
    smelt_core::ConfirmRequest {
        call_id: format!("call-{request_id}"),
        tool_name: "test_tool".into(),
        args: std::collections::HashMap::new(),
        approval_candidates: Vec::new(),
        grant_options: Vec::new(),
        summary: protocol::StyledLines::from_plain("test tool"),
        request_id,
    }
}

fn install_confirm_test_permissions(app: &mut TestApp) {
    app.app.core.permissions.replace(confirm_test_permissions());
}

fn permission_decisions(cmds: Vec<protocol::UiCommand>) -> Vec<(u64, bool)> {
    cmds.into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::PermissionDecision {
                request_id,
                approved,
                ..
            } => Some((request_id, approved)),
            _ => None,
        })
        .collect()
}

fn dispatch_confirm_request(
    app: &mut TestApp,
    req: smelt_core::ConfirmRequest,
    pending: &mut Vec<crate::app::PendingTool>,
) -> crate::app::SessionControl {
    let mut turn = app.app.agent.take().expect("test turn is active");
    turn.pending = std::mem::take(pending);
    let ctrl = app.app.dispatch_control(
        crate::app::SessionControl::NeedsConfirm(Box::new(req)),
        &mut turn,
    );
    *pending = std::mem::take(&mut turn.pending);
    app.app.agent = Some(turn);
    ctrl
}

fn actions_permission_decisions(actions: &[Action]) -> Vec<(u64, bool)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::PermissionDecision {
                    request_id,
                    approved,
                    ..
                } => Some((*request_id, *approved)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn request_permission_auto_allows_when_applied_turn_mode_allows() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.app.applied_agent_mode = protocol::AgentMode::parse("apply").unwrap();

    let mut pending = Vec::new();
    let ctrl = dispatch_confirm_request(&mut app, confirm_req(10), &mut pending);

    assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(10, true)]
    );
}

#[test]
fn request_permission_auto_denies_when_applied_turn_mode_denies() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.app.applied_agent_mode = protocol::AgentMode::parse("deny").unwrap();

    let mut pending = vec![crate::app::PendingTool {
        call_id: "call-11".into(),
        name: "test_tool".into(),
    }];
    let ctrl = dispatch_confirm_request(&mut app, confirm_req(11), &mut pending);

    assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    assert_eq!(app.pending_confirm_count(), 0);
    assert!(pending.is_empty());
    assert!(app.agent_running());
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(11, false)]
    );
}

#[test]
fn tool_evaluation_uses_the_mode_carried_by_the_turn_event() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.app.core.config.mode = protocol::AgentMode::parse("deny").unwrap();
    let _ = app.drain_engine_sends();

    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::ToolEvaluationRequest {
            request_id: 91,
            call_id: "call-91".into(),
            tool_name: "test_tool".into(),
            args: std::collections::HashMap::new(),
            mode: protocol::AgentMode::parse("apply").unwrap(),
        },
    ));

    let decision = app.actions().iter().rev().find_map(|action| match action {
        Action::EngineSend(command) => match command.as_ref() {
            protocol::UiCommand::ToolEvaluationResponse {
                request_id: 91,
                evaluation,
            } => Some(evaluation.decision.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(decision, Some(protocol::Decision::Allow));
}

#[test]
fn open_confirm_recheck_keeps_dialog_when_mode_still_asks() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(12), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    let handle_id = app.first_pending_confirm().unwrap();
    assert!(!app.app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 1);
    assert!(permission_decisions(app.drain_engine_sends()).is_empty());
}

#[test]
fn public_status_open_confirm_needs_attention() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(13), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    assert_eq!(app.pending_confirm_count(), 1);
    assert!(app.state().active_modal.is_some());

    let (state, reason) = app.app.public_status_state_reason();
    assert_eq!(
        state,
        smelt_core::public_status::PublicState::NeedsAttention
    );
    assert_eq!(
        reason,
        Some(smelt_core::public_status::PublicReason::Permission)
    );
}

#[test]
fn open_confirm_dialog_does_not_show_permission_pending_in_statusline() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(130), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    assert_eq!(app.pending_confirm_count(), 1);
    assert!(app.state().active_modal.is_some());
    let frame = app.render_to_frame();
    let text = frame.text();
    let statusline = text.lines().last().expect("rendered statusline");
    assert!(!statusline.contains("permission pending"), "{statusline:?}");
}

#[test]
fn deferred_confirm_dialog_shows_permission_pending_in_statusline() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.type_text("draft response");

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(131), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(app.pending_deferred_dialog_count(), 1);
    assert!(app.state().active_modal.is_none());
    let frame = app.render_to_frame();
    let text = frame.text();
    let statusline = text.lines().last().expect("rendered statusline");
    assert!(statusline.contains("permission pending"), "{statusline:?}");
}

#[test]
fn confirm_down_moves_selection_without_scrolling_options() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(14), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    app.render_silent();

    let options = app.app.ui.focus().expect("confirm options focused");
    let before = app.app.ui.win(options).expect("confirm options window");
    assert_eq!(before.cursor_abs_row(), 0);
    assert_eq!(before.scroll_top(), 0);
    assert!(
        before
            .viewport
            .expect("confirm options viewport")
            .rect
            .height
            >= 2,
        "both confirm options should be visible"
    );

    app.press(KeyCode::Down);

    let after = app.app.ui.win(options).expect("confirm options window");
    assert_eq!(after.cursor_abs_row(), 1, "Down should select deny");
    assert_eq!(
        after.scroll_top(),
        0,
        "the options viewport should stay fixed while deny is visible"
    );
}

#[test]
fn confirm_top_border_drag_resizes_dialog() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(15), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    app.render_silent();

    let dialog = app.app.active_docked_dialog().expect("confirm dialog");
    let first_leaf = app
        .app
        .ui
        .modal_leaves(
            app.app
                .ui
                .docked_surface(dialog)
                .expect("docked surface")
                .modal(),
        )
        .and_then(|leaves| leaves.first())
        .copied()
        .expect("confirm dialog leaf");
    let before_top = app
        .app
        .ui
        .split_rect(first_leaf)
        .expect("confirm dialog leaf rect")
        .top
        .saturating_sub(1);
    let target_top = before_top.saturating_sub(3);

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: if matches!(kind, MouseEventKind::Down(_)) {
                before_top
            } else {
                target_top
            },
            column: 10,
            modifiers: KeyModifiers::empty(),
        })));
    }
    app.render_silent();

    let after_top = app
        .app
        .ui
        .split_rect(first_leaf)
        .expect("resized confirm dialog leaf rect")
        .top
        .saturating_sub(1);
    assert_eq!(after_top, target_top);
}

#[test]
fn custom_composer_dialog_chrome_resizes_and_reflows_with_terminal() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.win.PROMPT)
          end
          return smelt.ui.layout.vbox({
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));
    app.render_silent();

    let dialog = app.app.active_docked_dialog().expect("active dialog");
    let before = app
        .app
        .ui
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("custom-composed dialog rect");
    let target_top = before.top.saturating_sub(2);

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: if matches!(kind, MouseEventKind::Down(_)) {
                before.top
            } else {
                target_top
            },
            column: 10,
            modifiers: KeyModifiers::empty(),
        })));
    }
    app.render_silent();

    let resized = app
        .app
        .ui
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("resized custom-composed dialog rect");
    assert_eq!(resized.top, target_top);
    assert_eq!(resized.height, before.height + 2);

    app.set_terminal_size(80, 3);
    app.render_silent();
    let constrained = app
        .app
        .ui
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("constrained dialog rect");
    assert!(constrained.height < resized.height);
    assert!(constrained.bottom() <= 3);

    app.set_terminal_size(80, 24);
    app.render_silent();
    let restored = app
        .app
        .ui
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("restored dialog rect");
    assert_eq!(restored.height, resized.height);
}

#[test]
fn shift_tab_on_open_confirm_waits_for_turn_mode_boundary_before_allowing() {
    let mut app = TestApp::builder()
        .with_mode_cycle(vec![
            protocol::AgentMode::normal(),
            protocol::AgentMode::parse("apply").unwrap(),
        ])
        .build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(13), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    assert_eq!(app.pending_confirm_count(), 1);
    app.clear_actions();

    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);

    assert_eq!(app.pending_confirm_count(), 1);
    assert_eq!(app.app.core.config.mode.as_str(), "apply");
    assert!(app.app.mode_pending());
    assert!(actions_permission_decisions(app.actions()).is_empty());

    app.app.sync_agent_mode_applied();
    let handle_id = app.first_pending_confirm().unwrap();
    assert!(app.app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(13, true)]
    );
}

#[test]
fn open_confirm_recheck_auto_denies_without_stopping_turn() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let _guard = crate::lua::install_app_ptr(&mut app.app);
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(14), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    let handle_id = app.first_pending_confirm().unwrap();
    app.app.applied_agent_mode = protocol::AgentMode::parse("deny").unwrap();

    assert!(app.app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 0);
    assert!(app.agent_running());
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(14, false)]
    );
}

#[test]
fn present_plan_save_draft_writes_artifact_and_manifest() {
    let home_guard = test_home_guard();
    let mut app = TestApp::builder()
        .with_vim(false)
        .build_with_test_home_guard(&home_guard);
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Goal\nShip the parser change.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        request_id: 91,
        call_id: "plan-draft".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    assert!(app.run_lua(r#"smelt.dialog.current().resolve("draft")"#));
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-draft").expect("present_plan result");
    assert!(!is_error, "{content}");
    let plan_path = content
        .lines()
        .find_map(|line| line.strip_prefix("wrote plan to "))
        .expect("result includes plan path");
    assert!(plan_path.ends_with("/plan.md"));
    assert_eq!(
        std::fs::read_to_string(plan_path).unwrap(),
        "# Goal\nShip the parser change.\n"
    );

    let artifact_dir = std::path::Path::new(plan_path).parent().unwrap();
    let artifact_dir = std::fs::canonicalize(artifact_dir).unwrap();
    let grants = app.app.session_path_grants();
    assert!(grants.iter().any(|grant| {
        grant.mode.is_none()
            && grant.tool == "read_file"
            && grant.access == smelt_core::permissions::PathAccess::Read
            && grant.dir == artifact_dir
    }));
    assert!(grants.iter().any(|grant| {
        grant.mode.is_none()
            && grant.tool == "edit_file"
            && grant.access == smelt_core::permissions::PathAccess::Write
            && grant.dir == artifact_dir
    }));
    assert!(grants.iter().any(|grant| {
        grant
            .mode
            .as_ref()
            .is_some_and(|mode| mode.as_str() == "plan")
            && grant.tool == "edit_file"
            && grant.access == smelt_core::permissions::PathAccess::Write
            && grant.dir == artifact_dir
    }));

    let manifest_path = artifact_dir.join("manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["kind"], "smelt.plan");
    assert_eq!(manifest["status"], "draft");
    assert_eq!(manifest["title"], "Parser plan");
    assert_eq!(manifest["slug"], "parser-plan");
}

#[test]
fn present_plan_existing_path_approves_without_overwriting_plan_body() {
    let home_guard = test_home_guard();
    let mut app = TestApp::builder()
        .with_vim(false)
        .build_with_test_home_guard(&home_guard);
    app.start_turn(1);

    let session_dir = smelt_core::session::dir_for(&app.app.core.session);
    let artifact_dir = session_dir.join("plans/20260101-000000-parser-plan");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let plan_path = artifact_dir.join("plan.md");
    std::fs::write(&plan_path, "# Revised\nUse the existing draft.\n").unwrap();
    std::fs::write(
        artifact_dir.join("manifest.json"),
        serde_json::json!({
            "version": 1,
            "kind": "smelt.plan",
            "title": "Parser plan",
            "slug": "parser-plan",
            "status": "draft",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "plan_path": "plan.md"
        })
        .to_string(),
    )
    .unwrap();

    let mut args = std::collections::HashMap::new();
    args.insert(
        "plan_path".into(),
        serde_json::json!(plan_path.to_string_lossy().to_string()),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        request_id: 92,
        call_id: "plan-approve".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    app.press(KeyCode::Char('2'));
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-approve").expect("present_plan result");
    assert!(!is_error, "{content}");
    assert!(content.contains("wrote plan to "));
    assert_eq!(
        std::fs::read_to_string(&plan_path).unwrap(),
        "# Revised\nUse the existing draft.\n"
    );

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(artifact_dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["status"], "approved");
    assert_eq!(manifest["created_at"], "2026-01-01T00:00:00Z");
}

#[test]
fn present_plan_dismiss_does_not_echo_plan_body() {
    let home_guard = test_home_guard();
    let mut app = TestApp::builder()
        .with_vim(false)
        .build_with_test_home_guard(&home_guard);
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Secret draft\nDo not keep this transcript copy.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        request_id: 94,
        call_id: "plan-dismiss".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-dismiss").expect("present_plan result");
    assert!(is_error);
    assert!(content.contains("plan dismissed"));
    assert!(!content.contains("Secret draft"));
    assert!(!content.contains("Do not keep this transcript copy"));
}

#[test]
fn present_plan_dialog_tracks_terminal_width_on_resize() {
    let home_guard = test_home_guard();
    let mut app = TestApp::builder()
        .with_vim(false)
        .build_with_test_home_guard(&home_guard);
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Goal\nShip the parser change.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        request_id: 93,
        call_id: "plan-resize".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    let before = app.render_to_frame();
    assert!(before
        .text()
        .lines()
        .any(|line| line.starts_with("─ plan ")));

    app.set_terminal_size(100, 24);
    let after = app.render_to_frame();
    let text = after.text();
    let title_row = text
        .lines()
        .find(|line| line.starts_with("─ plan "))
        .expect("dialog title row after resize");
    assert_eq!(title_row.chars().count(), 100);
}

#[test]
fn public_status_open_question_needs_attention() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([
            {
                "header": "Choice",
                "question": "Pick one?",
                "options": [
                    { "label": "One", "description": "first option" },
                    { "label": "Two", "description": "second option" }
                ],
                "multiSelect": false
            }
        ]),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        request_id: 77,
        call_id: "status-question".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    let (state, reason) = app.app.public_status_state_reason();
    assert_eq!(
        state,
        smelt_core::public_status::PublicState::NeedsAttention
    );
    assert_eq!(
        reason,
        Some(smelt_core::public_status::PublicReason::Question)
    );
}

#[test]
fn ask_user_question_multiple_questions_wakes_between_dialogs() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([
            {
                "header": "First",
                "question": "Pick first?",
                "options": [
                    { "label": "One", "description": "first option" },
                    { "label": "Two", "description": "second option" }
                ],
                "multiSelect": false
            },
            {
                "header": "Second",
                "question": "Pick second?",
                "options": [
                    { "label": "Three", "description": "third option" },
                    { "label": "Four", "description": "fourth option" }
                ],
                "multiSelect": false
            }
        ]),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        request_id: 77,
        call_id: "aq-questions".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));

    let first = app
        .state()
        .active_modal
        .expect("first question dialog should open");

    app.press(KeyCode::Enter);
    assert!(
        app.app.lua_wakeup_rx.try_recv().is_ok(),
        "resolving the first dialog should wake the Lua task runtime"
    );
    app.feed_one(SourceEvent::LuaWakeup);

    let second = app
        .state()
        .active_modal
        .expect("second question dialog should open after first answer");
    assert_ne!(first, second);

    app.press(KeyCode::Char('2'));
    assert!(
        app.app.lua_wakeup_rx.try_recv().is_ok(),
        "resolving the final dialog should wake the Lua task runtime"
    );
    app.feed_one(SourceEvent::LuaWakeup);

    assert!(app.state().active_modal.is_none());
    let result = app
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                    ..
                } => Some((*request_id, call_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            },
            _ => None,
        })
        .next_back()
        .expect("ask_user_question should send a tool result");

    assert_eq!(result.0, 77);
    assert_eq!(result.1, "aq-questions");
    assert_eq!(
        result.2,
        "q: Pick first?\na: One\n\nq: Pick second?\na: Four"
    );
    assert!(!result.3);
}

#[test]
fn colon_in_non_vim_mode_does_not_open_cmdline() {
    let mut app = TestApp::builder().build();
    app.type_char(':');
    let s = app.state();
    assert!(!s.cmdline_open);
    assert_eq!(s.prompt_text, ":");
}

#[test]
fn colon_in_vim_insert_mode_does_not_open_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Fresh prompt is already in Insert.
    assert_eq!(app.state().vim_mode, VimMode::Insert);

    app.type_char(':');
    let s = app.state();
    assert!(!s.cmdline_open);
    assert_eq!(s.prompt_text, ":");
}

#[test]
fn colon_in_vim_normal_mode_opens_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Drop to Normal first since the prompt starts in Insert.
    app.press(KeyCode::Esc);
    app.type_char(':');
    let s = app.state();
    assert!(s.cmdline_open);
    assert_eq!(s.cmdline_text, "");
}

#[test]
fn esc_closes_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    assert!(app.state().cmdline_open);

    app.press(KeyCode::Esc);
    assert!(!app.state().cmdline_open);
}

#[test]
fn cmdline_quit_command_requests_quit() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.type_text("quit");
    app.press(KeyCode::Enter);
    assert!(app.state().pending_quit);
}

#[test]
fn cmdline_paste_inserts_single_line_payload() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');

    app.feed_one(SourceEvent::Term(Event::Paste("hello\nworld".into())));

    let s = app.state();
    assert!(s.cmdline_open);
    assert_eq!(s.cmdline_text, "hello world");
}

#[test]
fn cmdline_unicode_editing_uses_byte_safe_cursor() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.feed_one(SourceEvent::Term(Event::Paste("a日本b".into())));
    app.press(KeyCode::Left);
    app.press(KeyCode::Backspace);

    let s = app.state();
    assert_eq!(s.cmdline_text, "a日b");
    app.assert_invariants();
}

#[test]
fn cmdline_typed_payload_does_not_trip_cursor_invariant() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    // Type more than any single-line buffer could ever encode in
    // source byte arithmetic from cell position alone.
    app.type_text("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    app.assert_invariants();
}

#[test]
fn btw_command_preserves_model_history_prefix_and_appends_question() {
    let mut app = TestApp::builder().build();
    stub_btw_ui(&mut app);
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

    let expected_prefix = protocol::history_to_messages(&app.app.model_history());

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.apply_lua_command("btw what changed?");
        app.app.drive_lua_tasks();
    }

    let asks = ask_messages(app.drain_engine_sends());
    assert_eq!(asks.len(), 1, "/btw should issue one inherited ask");
    let (system, messages) = &asks[0];
    assert_eq!(system, &app.app.assemble_system_prompt());
    assert_eq!(
        &messages[..expected_prefix.len()],
        expected_prefix.as_slice(),
        "/btw must preserve the exact model-visible prefix"
    );
    let last_text = messages
        .last()
        .and_then(|m| m.content.as_ref())
        .map(|c| c.text_content())
        .expect("/btw question");
    assert!(last_text.contains("Under no circumstances use tools"));
    assert!(last_text.contains("Question: what changed?"));

    respond_pending_ask_with_text(&mut app, "done");
    app.app.core.timers.clear();
}

#[test]
fn btw_command_denies_tool_calls_then_retries_same_request_shape() {
    let mut app = TestApp::builder().build();
    stub_btw_ui(&mut app);
    app.app
        .core
        .session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    let expected_prefix = protocol::history_to_messages(&app.app.model_history());

    {
        let _g = crate::lua::install_app_ptr(&mut app.app);
        app.app.apply_lua_command("btw quick summary");
        app.app.drive_lua_tasks();
    }

    let first = ask_messages(app.drain_engine_sends());
    assert_eq!(first.len(), 1);
    let first_messages = first[0].1.clone();

    respond_pending_ask_with_tool_call(&mut app, "call-1", "grep");

    let second = ask_messages(app.drain_engine_sends());
    assert_eq!(second.len(), 1);
    let second_messages = &second[0].1;
    assert_eq!(
        &second_messages[..first_messages.len()],
        first_messages.as_slice(),
        "/btw tool denial retry must keep the same request prefix"
    );
    assert_eq!(
        &second_messages[..expected_prefix.len()],
        expected_prefix.as_slice(),
        "/btw tool denial retry must keep the same inherited conversation prefix"
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

    respond_pending_ask_with_text(&mut app, "done");
    app.app.core.timers.clear();
}
