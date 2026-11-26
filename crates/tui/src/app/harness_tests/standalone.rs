//! A Lua subcommand such as `smelt diff`: its view is the process's only UI,
//! so the process needs one, ends with it and never reloads. The diff and
//! file finder tests cover a view that opens and closes.

use super::*;

#[test]
fn standalone_process_without_a_view_exits_with_an_error() {
    let app = TestApp::builder().with_cli_command("probe", &[]).build();
    assert!(app.tui.pending_quit);
    let exit = app.tui.quit_exit().unwrap();
    assert_eq!(exit.code, 1);
    assert_eq!(exit.message.as_deref(), Some("smelt probe: no view opened"));
}

#[test]
fn standalone_process_never_reloads() {
    let mut app = TestApp::builder().with_cli_command("probe", &[]).build();
    assert!(!app.schedule_lua_reload());
    assert!(!app.tui.lua_reload_pending());
}

#[test]
fn agent_session_stays_open_without_a_view() {
    let mut app = TestApp::builder().build();
    app.settle_frame();
    assert!(!app.tui.pending_quit);
}

/// `smelt files`: the picker is the whole screen. The root lays out none of
/// the session's windows, and once the picker closes no other frame is
/// painted before smelt quits.
#[test]
fn standalone_view_is_the_only_ui() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alpha.rs"), "fn alpha_preview() {}\n").unwrap();
    let mut app = TestApp::builder()
        .with_cwd(dir.path())
        .with_cli_command("files", &[])
        .build();
    app.set_terminal_size(120, 20);
    app.render_silent();
    app.dispatch_ui_window_events(false);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let picker = loop {
        app.feed_one(SourceEvent::LuaWakeup);
        app.advance_animation(100);
        let frame = app.render_to_frame().text();
        if frame.contains("alpha_preview") {
            break frame;
        }
        assert!(std::time::Instant::now() < deadline, "no picker:\n{frame}");
        std::thread::yield_now();
    };
    for win in [crate::app::PROMPT_WIN, crate::app::TRANSCRIPT_WIN] {
        assert_eq!(app.tui.ui.split_rect(win), None);
    }

    app.press(KeyCode::Esc);
    assert_eq!(app.render_to_frame().text(), picker);
    app.settle_frame();
    assert!(app.tui.pending_quit);
}
