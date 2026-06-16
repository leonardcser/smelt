//! Transcript block/tool view-state stories. These pin the Lua-owned
//! collapsed/peek/expanded presentations independent of the grouping stories.

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;

const THINKING: &str = "**Inspecting the renderer**\nRead the transcript model first.\nCheck the Lua defaults next.\nUpdate the stories last.";

app_story!(thinking_block_collapsed, |ctx| {
    ctx.set_viewport(56, 10);
    ctx.engine(EngineEvent::Thinking {
        content: THINKING.into(),
    });
    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'close')");
    ctx.assert_snapshot();
});

app_story!(thinking_block_peek, |ctx| {
    ctx.set_viewport(56, 10);
    ctx.engine(EngineEvent::Thinking {
        content: THINKING.into(),
    });
    ctx.assert_snapshot();
});

app_story!(thinking_block_expanded, |ctx| {
    ctx.set_viewport(56, 12);
    ctx.engine(EngineEvent::Thinking {
        content: THINKING.into(),
    });
    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'open')");
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
