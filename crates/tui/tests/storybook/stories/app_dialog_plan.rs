//! Plan-mode dialogs. These stories drive the real `present_plan` tool
//! execute callback until it yields at `smelt.dialog.open`, pinning the
//! custom review dialog users see before choosing draft/approval.

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;
use crate::storybook::app_ctx::AppStoryCtx;
use crate::storybook::args;

fn open_present_plan_dialog(ctx: &mut AppStoryCtx) {
    ctx.engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(1),
        request_id: 1,
        call_id: "plan-dialog".into(),
        tool_name: "present_plan".into(),
        args: args([
            ("title", json!("Parser refactor")),
            ("slug", json!("parser-refactor")),
            (
                "plan",
                json!("# Goal\nRefactor parser state.\n\n# Sequence\n1. Add tests.\n2. Move state handling."),
            ),
        ]),
    });
}

app_story!(present_plan_dialog, |ctx| {
    ctx.set_viewport(80, 24);
    open_present_plan_dialog(ctx);
    ctx.assert_snapshot();
});

app_story!(present_plan_dialog_expanded_max_height, |ctx| {
    ctx.set_viewport(80, 24);
    open_present_plan_dialog(ctx);
    ctx.expand_active_dialog_to_max_height();
    ctx.assert_snapshot();
});
