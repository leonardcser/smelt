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

app_story!(glob_group_collapsed, |ctx| {
    ctx.set_viewport(84, 12);
    ctx.tool_call_with_metadata(
        "glob",
        &[
            ("pattern", json!("**/*.rs")),
            ("path", json!("crates/tui/src")),
        ],
        "crates/tui/src/app.rs\ncrates/tui/src/commands.rs\ncrates/tui/src/theme.rs",
        json!({ "display_count": { "value": 3, "unit": "file" } }),
        Some(5),
    );
    ctx.tool_call_with_metadata(
        "glob",
        &[
            ("pattern", json!("**/*.lua")),
            ("path", json!("runtime/lua/smelt")),
        ],
        "runtime/lua/smelt/tools/bash.lua\nruntime/lua/smelt/tools/glob.lua",
        json!({ "display_count": { "value": 2, "unit": "file" } }),
        Some(6),
    );
    ctx.assert_snapshot();
});

app_story!(glob_group_expanded, |ctx| {
    ctx.set_viewport(84, 18);
    ctx.tool_call_with_metadata(
        "glob",
        &[
            ("pattern", json!("**/*.rs")),
            ("path", json!("crates/tui/src")),
        ],
        "crates/tui/src/app.rs\ncrates/tui/src/commands.rs\ncrates/tui/src/theme.rs",
        json!({ "display_count": { "value": 3, "unit": "file" } }),
        Some(5),
    );
    ctx.tool_call_with_metadata(
        "glob",
        &[
            ("pattern", json!("**/*.lua")),
            ("path", json!("runtime/lua/smelt")),
        ],
        "runtime/lua/smelt/tools/bash.lua\nruntime/lua/smelt/tools/glob.lua",
        json!({ "display_count": { "value": 2, "unit": "file" } }),
        Some(6),
    );
    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot();
});

app_story!(background_process_group_collapsed, |ctx| {
    ctx.set_viewport(76, 10);
    ctx.push_background_process_completed("4210", Some(0));
    ctx.push_background_process_completed("4211", Some(1));
    ctx.push_background_process_completed("4212", None);
    ctx.assert_snapshot();
});

app_story!(background_process_group_expanded, |ctx| {
    ctx.set_viewport(76, 14);
    ctx.push_background_process_completed("4210", Some(0));
    ctx.push_background_process_completed("4211", Some(1));
    ctx.push_background_process_completed("4212", None);
    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot();
});
