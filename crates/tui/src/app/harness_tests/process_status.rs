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
    app.tick_signals();
    let state: String = app
        .eval_lua(r#"return smelt.signal.get("work_state")"#)
        .expect("work_state");
    assert_eq!(state, "busy");
    let label: String = app
        .eval_lua(r#"return smelt.signal.get("work_label")"#)
        .expect("work_label");
    assert_eq!(label, "syncing");
    let (count, first_label): (i64, String) = app
        .eval_lua(
            r#"
                local s = smelt.signal.get("work_busy")
                return #s, s[1].label
                "#,
        )
        .expect("work_busy");
    assert_eq!(count, 1);
    assert_eq!(first_label, "syncing");

    let ok = app.run_lua("_G._busy_handle:remove(); _G._busy_handle = nil");
    assert!(ok);
    app.tick_signals();
    let state_after: String = app
        .eval_lua(r#"return smelt.signal.get("work_state")"#)
        .expect("work_state post-release");
    assert_eq!(state_after, "idle");
}

#[test]
fn custom_mode_without_highlight_group_uses_default_style() {
    let mut app = TestApp::builder().build();
    let group: String = app
        .eval_lua(
            r#"
            smelt.mode.register({ name = "review" })
            return smelt.mode.style("review").hl_group
            "#,
        )
        .expect("custom mode highlight group");

    assert_eq!(group, "SmeltModeDefault");
}

#[test]
fn statusline_can_truncate_items_in_the_middle() {
    let mut app = TestApp::builder().build();
    let row: String = app
        .eval_lua(
            r#"
            local bar = require("smelt._bar")
            local row = bar.compose_status({
              {
                text = "smelt/.worktrees/test",
                style = { fg = "Comment" },
                priority = 7,
                truncatable = true,
                truncate = "middle",
              },
            }, { width = 14, bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
            return row.text
            "#,
        )
        .expect("compose statusline");

    assert_eq!(row, "smelt/…/test  ");
}

#[test]
fn statusline_spacing_respects_text_and_block_items() {
    let mut app = TestApp::builder().build();
    let row: String = app
        .eval_lua(
            r#"
            local bar = require("smelt._bar")
            local row = bar.compose_status({
              { text = "repo", style = { fg = "Comment" } },
              { text = "tok/s", style = { fg = "Comment" } },
              { text = " INSERT ", style = { hl_group = "SmeltVimInsert" } },
              { text = " ⚡yolo ", style = { hl_group = "SmeltModeDefault" } },
              { text = "procs", style = { fg = "SmeltProcess" }, separated = true },
            }, { width = 80, bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
            return row.text
            "#,
        )
        .expect("compose statusline");

    assert!(
        row.starts_with("repo tok/s  INSERT  ⚡yolo  procs"),
        "{row:?}"
    );
}

#[test]
fn statusline_separates_first_inline_indicator_after_pills() {
    let mut app = TestApp::builder().build();
    let row: String = app
        .eval_lua(
            r#"
            local bar = require("smelt._bar")
            local row = bar.compose_status({
              { text = " INSERT ", style = { hl_group = "SmeltVimInsert" } },
              { text = " ⚡yolo ", style = { hl_group = "SmeltModeDefault" } },
              { text = "14 procs", style = { fg = "SmeltProcess" }, separated = true },
              { text = "permission pending", style = { fg = "SmeltAccent" }, separated = true },
            }, { width = 80, bg_group = "SmeltStatusBg", sep_group = "SmeltSeparator" })
            return row.text
            "#,
        )
        .expect("compose statusline");

    assert!(
        row.starts_with(" INSERT  ⚡yolo  14 procs · permission pending"),
        "{row:?}"
    );
}

#[test]
fn tick_event_advances_virtual_clock() {
    let mut app = TestApp::builder().build();
    let before = app.core_probe().clock.instant_now();
    app.feed_one(SourceEvent::Tick(500));
    let after = app.core_probe().clock.instant_now();
    assert_eq!(after - before, Duration::from_millis(500));
}
