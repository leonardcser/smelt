//! Built-in transcript group rendering. These stories drive adjacent terminal
//! tool calls through the real app pipeline so the virtual group nodes are
//! planned by Rust and rendered by the bundled Lua group renderers.

use serde_json::json;

use crate::app_story;

app_story!(read_file_group_collapsed, |ctx| {
    ctx.set_viewport(72, 12);
    ctx.tool_call(
        "read_file",
        &[("file_path", json!("src/lib.rs"))],
        "pub mod api;\npub mod runtime;\n",
        Some(3),
    );
    ctx.tool_call(
        "read_file",
        &[("file_path", json!("src/main.rs"))],
        "fn main() {\n    smelt::run();\n}\n",
        Some(4),
    );
    ctx.assert_snapshot();
});

app_story!(read_file_group_expanded, |ctx| {
    ctx.set_viewport(72, 16);
    ctx.tool_call(
        "read_file",
        &[("file_path", json!("src/lib.rs"))],
        "pub mod api;\npub mod runtime;\n",
        Some(3),
    );
    ctx.tool_call(
        "read_file",
        &[("file_path", json!("src/main.rs"))],
        "fn main() {\n    smelt::run();\n}\n",
        Some(4),
    );
    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot();
});

app_story!(grep_group_collapsed, |ctx| {
    ctx.set_viewport(78, 12);
    ctx.tool_call(
        "grep",
        &[
            ("pattern", json!("render_group")),
            ("path", json!("crates/tui/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/tui/src/content/display_layout.rs\ncrates/tui/src/content/render_plan.rs",
        Some(8),
    );
    ctx.tool_call(
        "grep",
        &[
            ("pattern", json!("ViewState")),
            ("path", json!("crates/core/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/core/src/transcript_model.rs\ncrates/core/src/lua/runtime.rs",
        Some(9),
    );
    ctx.assert_snapshot();
});

app_story!(grep_group_expanded, |ctx| {
    ctx.set_viewport(78, 18);
    ctx.tool_call(
        "grep",
        &[
            ("pattern", json!("render_group")),
            ("path", json!("crates/tui/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/tui/src/content/display_layout.rs\ncrates/tui/src/content/render_plan.rs",
        Some(8),
    );
    ctx.tool_call(
        "grep",
        &[
            ("pattern", json!("ViewState")),
            ("path", json!("crates/core/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/core/src/transcript_model.rs\ncrates/core/src/lua/runtime.rs",
        Some(9),
    );
    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot();
});
