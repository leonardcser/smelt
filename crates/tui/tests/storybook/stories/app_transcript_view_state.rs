//! Transcript block/tool view-state stories. These pin the Lua-owned
//! collapsed/peek/expanded presentations independent of the grouping stories.

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;

const THINKING: &str = "**Inspecting the renderer**\nRead the transcript model first.\nCheck the Lua defaults next.\nCompare the rendered rows.\nUpdate the stories last.\nRun the snapshot tests.\nReview the final diff.";
const THINKING_UNTITLED: &str = "Read the transcript model first.\nCheck the Lua defaults next.\nCompare the rendered rows.\nUpdate the stories last.\nRun the snapshot tests.\nReview the final diff.";
const COMPACTED_SUMMARY: &str =
    "Compacted 8 earlier turns: parser refactor, renderer wiring, three bug fixes.";

app_story!(thinking_block_collapsed, |ctx| {
    ctx.set_viewport(56, 10);
    ctx.engine(EngineEvent::Thinking {
        content: THINKING.into(),
    });
    ctx.engine(EngineEvent::Thinking {
        content: THINKING_UNTITLED.into(),
    });
    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'close')");
    ctx.assert_snapshot();
});

app_story!(thinking_block_peek, |ctx| {
    ctx.set_viewport(56, 16);
    ctx.engine(EngineEvent::Thinking {
        content: THINKING.into(),
    });
    ctx.engine(EngineEvent::Thinking {
        content: THINKING_UNTITLED.into(),
    });
    ctx.assert_snapshot();
});

app_story!(thinking_block_expanded, |ctx| {
    ctx.set_viewport(56, 18);
    ctx.engine(EngineEvent::Thinking {
        content: THINKING.into(),
    });
    ctx.engine(EngineEvent::Thinking {
        content: THINKING_UNTITLED.into(),
    });
    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'open')");
    ctx.assert_snapshot();
});

app_story!(compacted_block_collapsed, |ctx| {
    ctx.set_viewport(60, 9);
    ctx.push_compacted(COMPACTED_SUMMARY);
    ctx.assert_snapshot();
});

app_story!(compacted_block_expanded, |ctx| {
    ctx.set_viewport(60, 9);
    ctx.push_compacted(COMPACTED_SUMMARY);
    ctx.run_lua("smelt.transcript.fold_kind('compacted', 'open')");
    ctx.assert_snapshot();
});

app_story!(bash_multiline_header_indent, |ctx| {
    ctx.set_viewport(86, 18);
    ctx.tool_call(
        "bash",
        &[(
            "command",
            json!("set -euo pipefail\nrm -rf /tmp/smelt-fuzzy-check\nmkdir -p /tmp/smelt-fuzzy-check/src"),
        )],
        "done",
        Some(28_000),
    );
    ctx.assert_snapshot();
});

app_story!(tools_collapsed_summaries, |ctx| {
    ctx.set_viewport(86, 18);
    ctx.tool_call(
        "load_skill",
        &[("name", json!("customize"))],
        "# customize\nChange themes and keymaps.\nRegister commands and tools.\n",
        Some(2),
    );
    ctx.tool_call(
        "write_file",
        &[
            ("file_path", json!("src/new.rs")),
            ("content", json!("pub fn new() -> bool {\n    true\n}\n")),
        ],
        "ok",
        Some(4),
    );
    ctx.tool_call(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("pub fn old() {}\n")),
            ("new_string", json!("pub fn new() {}\n")),
        ],
        "ok",
        Some(5),
    );
    ctx.tool_call_with_metadata(
        "edit_notebook",
        &[
            ("notebook_path", json!("analysis.ipynb")),
            ("cell_number", json!(1)),
            ("new_source", json!("print('done')\n")),
        ],
        "ok",
        json!({
            "edit_mode": "replace",
            "path": "analysis.ipynb#cell1",
            "old_source": "print('todo')\n",
            "new_source": "print('done')\n",
        }),
        Some(6),
    );
    ctx.run_lua("smelt.transcript.fold_kind('tool', 'close')");
    ctx.assert_snapshot();
});

app_story!(tools_collapsed_errors, |ctx| {
    ctx.set_viewport(86, 18);
    ctx.tool_call_error(
        "read_file",
        &[("file_path", json!("missing.rs"))],
        "No such file or directory (os error 2)",
        Some(1),
    );
    ctx.tool_call_error(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("pub fn missing() {}\n")),
            ("new_string", json!("pub fn present() {}\n")),
        ],
        "old_string not found in src/lib.rs",
        Some(2),
    );
    ctx.tool_call_error(
        "grep",
        &[("pattern", json!("[unterminated")), ("path", json!("src"))],
        "regex parse error:\n    [unterminated\n    ^\nerror: unclosed character class",
        Some(3),
    );
    ctx.run_lua("smelt.transcript.fold_kind('tool', 'close')");
    ctx.assert_snapshot();
});

app_story!(transcript_settings_view_and_limits, |ctx| {
    ctx.set_viewport(86, 14);
    ctx.run_lua(
        r#"
        smelt.settings.transcript = {
          view = { tools = { bash = "collapsed" } },
          limits = { collapsed_error_rows = 2 },
        }
        smelt.transcript.invalidate_renderer()
        "#,
    );
    ctx.tool_call_error(
        "bash",
        &[("command", json!("cargo test --workspace"))],
        "error: test failed\nfailures:\n    app::tests::first\n    app::tests::second",
        Some(42),
    );
    ctx.assert_snapshot();
});

app_story!(tools_expanded_bodies, |ctx| {
    ctx.set_viewport(86, 30);
    ctx.tool_call(
        "load_skill",
        &[("name", json!("customize"))],
        "# customize\nChange themes and keymaps.\nRegister commands and tools.\n",
        Some(2),
    );
    ctx.tool_call(
        "write_file",
        &[
            ("file_path", json!("src/new.rs")),
            ("content", json!("pub fn new() -> bool {\n    true\n}\n")),
        ],
        "ok",
        Some(4),
    );
    ctx.tool_call(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("pub fn old() {}\n")),
            ("new_string", json!("pub fn new() {}\n")),
        ],
        "ok",
        Some(5),
    );
    ctx.tool_call_with_metadata(
        "edit_notebook",
        &[
            ("notebook_path", json!("analysis.ipynb")),
            ("cell_number", json!(1)),
            ("new_source", json!("print('done')\n")),
        ],
        "ok",
        json!({
            "edit_mode": "replace",
            "path": "analysis.ipynb#cell1",
            "old_source": "print('todo')\n",
            "new_source": "print('done')\n",
        }),
        Some(6),
    );
    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot();
});
