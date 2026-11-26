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
