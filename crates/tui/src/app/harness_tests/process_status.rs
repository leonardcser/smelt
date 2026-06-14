use super::*;

#[test]
fn smelt_work_busy_pushes_token_and_flips_work_cells() {
    let mut app = TestApp::builder().build();
    let lua_ok = app.run_lua(
        r#"
                _G._busy_handle = smelt.work.busy("syncing")
            "#,
    );
    assert!(lua_ok, "smelt.work.busy snippet failed");
    app.tick_cells();
    let _guard = crate::lua::install_app_ptr(&mut app.app);
    let state: String = app
        .app
        .lua
        .lua
        .load(r#"return smelt.cell("work_state"):get()"#)
        .eval()
        .expect("work_state");
    assert_eq!(state, "busy");
    let label: String = app
        .app
        .lua
        .lua
        .load(r#"return smelt.cell("work_label"):get()"#)
        .eval()
        .expect("work_label");
    assert_eq!(label, "syncing");
    let (count, first_label): (i64, String) = app
        .app
        .lua
        .lua
        .load(
            r#"
                local s = smelt.cell("work_busy"):get()
                return #s, s[1].label
                "#,
        )
        .eval()
        .expect("work_busy");
    assert_eq!(count, 1);
    assert_eq!(first_label, "syncing");
    drop(_guard);

    let ok = app.run_lua("_G._busy_handle:remove(); _G._busy_handle = nil");
    assert!(ok);
    app.tick_cells();
    let _guard = crate::lua::install_app_ptr(&mut app.app);
    let state_after: String = app
        .app
        .lua
        .lua
        .load(r#"return smelt.cell("work_state"):get()"#)
        .eval()
        .expect("work_state post-release");
    assert_eq!(state_after, "idle");
}

#[test]
fn tick_event_advances_virtual_clock() {
    let mut app = TestApp::builder().build();
    let before = app.app.core.clock.instant_now();
    app.feed_one(SourceEvent::Tick(500));
    let after = app.app.core.clock.instant_now();
    assert_eq!(after - before, Duration::from_millis(500));
}
