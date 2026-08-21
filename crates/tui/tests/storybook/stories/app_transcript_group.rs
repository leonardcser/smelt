//! Built-in transcript group rendering. These stories drive adjacent terminal
//! tool calls through the real app pipeline so virtual tool-group nodes are
//! planned by Rust and rendered through the bundled Lua root renderer.

use serde_json::json;

use crate::app_story;

app_story!(explore_tool_group_states, |ctx| {
    ctx.set_viewport(90, 24);
    ctx.run_lua("require('smelt.plugins.lsp').setup({ servers = {} })");
    ctx.tool_call(
        "read_file",
        &[("file_path", json!("src/lib.rs"))],
        "pub mod api;\npub mod runtime;\n",
        Some(3),
    );
    ctx.tool_call_with_metadata(
        "grep",
        &[
            ("pattern", json!("render_group")),
            ("path", json!("crates/tui/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/tui/src/content/display_layout.rs\ncrates/tui/src/content/transcript_scene.rs",
        json!({ "display_count": { "value": 2, "unit": "file" } }),
        Some(8),
    );
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
        "outline",
        &[
            ("file_path", json!("crates/tui/src/app.rs")),
            ("kind", json!("function")),
            ("name_contains", json!("render")),
            ("max_depth", json!(2)),
        ],
        "2 symbols\n- fn render 40:1-72:2\n- fn render_prompt 74:1-91:2",
        json!({ "display_count": { "value": 2, "unit": "symbol" } }),
        Some(7),
    );
    ctx.tool_call_with_metadata(
        "find_symbol",
        &[
            ("query", json!("RenderNode")),
            ("kind", json!("enum")),
            ("path_glob", json!("crates/tui/**/*.rs")),
        ],
        "1 symbol\n- enum RenderNode - crates/tui/src/content/transcript_scene.rs:26:1",
        json!({ "display_count": { "value": 1, "unit": "symbol" } }),
        Some(4),
    );
    ctx.assert_snapshot_named("collapsed");

    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot_named("expanded");
});

app_story!(lsp_tool_group_states, |ctx| {
    ctx.set_viewport(90, 24);
    ctx.run_lua("require('smelt.plugins.lsp').setup({ servers = {} })");
    ctx.tool_call(
        "inspect_symbol",
        &[
            ("query", json!("RenderNode")),
            ("kind", json!("enum")),
            ("path_glob", json!("crates/tui/**/*.rs")),
        ],
        "enclosing: enum RenderNode\ndefinitions: 1 location",
        Some(6),
    );
    ctx.tool_call(
        "inspect_symbol_at",
        &[
            (
                "file_path",
                json!("crates/tui/src/content/transcript_scene.rs"),
            ),
            ("line", json!(26)),
            ("column", json!(17)),
        ],
        "enclosing: enum RenderNode\nreferences: 12 references",
        Some(5),
    );
    ctx.tool_call(
        "find_definition",
        &[
            (
                "file_path",
                json!("crates/tui/src/content/transcript_scene.rs"),
            ),
            ("line", json!(44)),
            ("column", json!(19)),
        ],
        "1 definition\n- crates/core/src/transcript_model.rs:118:1",
        Some(4),
    );
    ctx.tool_call(
        "find_references",
        &[
            ("file_path", json!("crates/tui/src/content/transcript_scene.rs")),
            ("line", json!(26)),
            ("column", json!(17)),
        ],
        "2 references\n- crates/tui/src/content/transcript_scene.rs:44:13\n- crates/tui/src/content/transcript_buf.rs:812:9",
        Some(8),
    );
    ctx.tool_call(
        "diagnostics",
        &[(
            "file_path",
            json!("crates/tui/src/content/transcript_scene.rs"),
        )],
        "no diagnostics",
        Some(3),
    );
    ctx.assert_snapshot_named("collapsed");

    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot_named("expanded");
});

app_story!(web_tool_group_states, |ctx| {
    ctx.set_viewport(90, 24);
    ctx.tool_call(
        "web_search",
        &[("query", json!("rust unicode width crate"))],
        "1. unicode-width - crates.io\n2. unicode-segmentation - crates.io",
        Some(330),
    );
    ctx.tool_call(
        "web_fetch",
        &[
            (
                "url",
                json!("https://docs.rs/unicode-width/latest/unicode_width/"),
            ),
            ("prompt", json!("Find the latest release version")),
        ],
        "The latest release is 0.2.2.",
        Some(820),
    );
    ctx.assert_snapshot_named("collapsed");

    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot_named("expanded");
});

app_story!(background_process_group_states, |ctx| {
    ctx.set_viewport(76, 14);
    ctx.push_background_process_completed("4210", Some(0));
    ctx.push_background_process_completed("4211", Some(1));
    ctx.push_background_process_completed("4212", None);
    ctx.assert_snapshot_named("collapsed");

    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot_named("expanded");
});
