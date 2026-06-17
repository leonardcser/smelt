use super::*;

#[test]
fn cmdline_completion_stays_hidden_until_tab() {
    let mut app = TestApp::builder().with_vim(true).build();
    register_cmdline_test_commands(&mut app);

    app.press(KeyCode::Esc);
    app.press(KeyCode::Char(':'));
    assert!(app.state().cmdline_open);
    assert_eq!(app.state().picker_count, 0);

    app.type_text("zzban");
    assert_eq!(app.state().picker_count, 0);

    app.press(KeyCode::Tab);
    let state = app.state();
    assert_eq!(state.picker_count, 1);
    assert_eq!(state.cmdline_text, "zzban");
}

#[test]
fn cmdline_completion_renders_prompt_style_list_above_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 10);
    register_cmdline_test_commands(&mut app);

    app.press(KeyCode::Esc);
    app.press(KeyCode::Char(':'));
    app.type_text("zz");
    app.press(KeyCode::Tab);

    let leaf = *app
        .app
        .picker_state
        .keys()
        .next()
        .expect("cmdline completion picker opens");
    let lines = picker_buffer_lines(&app, leaf);
    assert!(lines.iter().any(|line| line.starts_with(' ')));
    assert!(
        lines.iter().any(|line| line.contains("test command")),
        "{lines:?}"
    );
    assert!(lines.iter().all(|line| !line.starts_with('>')), "{lines:?}");

    app.render_silent();
    let picker_rect = app
        .app
        .ui
        .paint_rect(crate::smelt_edit::PaintId::from(leaf))
        .expect("picker rect");
    let cmdline = app.app.well_known.cmdline.expect("cmdline win");
    let cmdline_rect = app
        .app
        .ui
        .paint_rect(crate::smelt_edit::PaintId::from(cmdline))
        .expect("cmdline rect");
    assert_eq!(picker_rect.top + picker_rect.height, cmdline_rect.top);
}

#[test]
fn cmdline_up_down_use_history_when_completion_closed() {
    let mut app = TestApp::builder().with_vim(true).build();
    register_cmdline_test_commands(&mut app);
    app.app.cmdline.history.push("older-command".to_string());

    app.press(KeyCode::Esc);
    app.press(KeyCode::Char(':'));
    app.type_text("zz");

    app.press(KeyCode::Up);
    let state = app.state();
    assert_eq!(state.picker_count, 0);
    assert_eq!(state.cmdline_text, "older-command");

    app.press(KeyCode::Down);
    let state = app.state();
    assert_eq!(state.cmdline_text, "zz");
}

#[test]
fn cmdline_up_down_select_completion_when_picker_is_open() {
    let mut app = TestApp::builder().with_vim(true).build();
    register_cmdline_test_commands(&mut app);
    app.app.cmdline.history.push("older-command".to_string());

    app.press(KeyCode::Esc);
    app.press(KeyCode::Char(':'));
    app.type_text("zz");
    app.press(KeyCode::Tab);
    assert_eq!(app.state().picker_count, 1);
    assert_eq!(app.state().cmdline_text, "zz");
    assert_eq!(cmdline_completion_selected(&app), Some(0));

    app.press(KeyCode::Up);
    assert_eq!(app.state().picker_count, 1);
    assert_eq!(app.state().cmdline_text, "zz");
    assert_eq!(cmdline_completion_selected(&app), Some(1));

    let selected_label = cmdline_completion_selected_label(&app).expect("selected label");
    app.press(KeyCode::Tab);
    let state = app.state();
    assert_eq!(state.picker_count, 0);
    assert_eq!(state.cmdline_text, selected_label);
}

fn cmdline_completion_selected(app: &TestApp) -> Option<usize> {
    app.app.cmdline.completer.as_ref().map(|c| c.selected)
}

fn cmdline_completion_selected_label(app: &TestApp) -> Option<String> {
    let comp = app.app.cmdline.completer.as_ref()?;
    comp.items.get(comp.selected).map(|item| item.label.clone())
}

fn register_cmdline_test_commands(app: &mut TestApp) {
    assert!(app.run_lua(
        r#"
        smelt.cmd.register("zzbanana", function() end, { desc = "test command banana" })
        smelt.cmd.register("zzband", function() end, { desc = "test command band" })
        smelt.cmd.register("zzhidden", function() end, { hidden = true })
        "#,
    ));
}
