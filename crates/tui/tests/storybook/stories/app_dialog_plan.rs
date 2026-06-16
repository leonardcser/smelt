//! Plan-mode dialogs. These stories drive the real `present_plan` tool
//! execute callback until it yields at `smelt.dialog.open`, pinning the
//! custom review dialog users see before choosing draft/approval.

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;
use crate::storybook::args;

app_story!(present_plan_dialog, |ctx| {
    ctx.set_viewport(80, 24);
    ctx.engine(EngineEvent::ToolDispatch {
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
    ctx.assert_snapshot();
});
