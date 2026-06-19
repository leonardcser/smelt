use crate::app_story;

app_story!(goal_banner_states, |ctx| {
    ctx.set_viewport(64, 16);
    ctx.push_user_turn("Please keep working toward the parser migration.");
    ctx.push_assistant_text("I will keep the migration goal visible while working.");

    ctx.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("migrate parser state handling without regressing streaming edits", { auto_continue = true, summary = "Parser migration" }))
            assert(goal.update_status({ progress = "Step 2/5, wiring parser state" }))
        "#,
    );
    ctx.assert_snapshot_named("active_auto");

    ctx.run_lua(r#"assert(require("smelt.goal").pause())"#);
    ctx.assert_snapshot_named("paused");

    ctx.run_lua(r#"assert(require("smelt.goal").block("waiting for parser fixture approval"))"#);
    ctx.assert_snapshot_named("blocked");
});
