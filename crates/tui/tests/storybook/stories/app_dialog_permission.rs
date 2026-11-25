//! Permission-confirm dialogs. `AppStoryCtx::request_permission`
//! calls the tool's real `summary(args)` Lua callback to populate the
//! `RequestPermission` event, then routes the event through
//! `dispatch_engine_event`. The app registers the confirm, fires
//! `smelt.confirm.open`, and renders the tool's `preview(args)`
//! layout (diff, file_view, notebook preview, …) through the live
//! tool-body IR preview pipeline.

use std::collections::HashMap;

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;
use crate::storybook::app_ctx::AppStoryCtx;
use crate::storybook::args;

// These stories render permission requests; they never run the command.
fn set_sandbox_available(ctx: &mut AppStoryCtx, available: bool) {
    ctx.run_lua(&format!(
        "smelt.sandbox.status = function() return {{ available = {available} }} end"
    ));
}

fn open_bash_permission_dialog(ctx: &mut AppStoryCtx, sandbox_available: bool) {
    set_sandbox_available(ctx, sandbox_available);
    ctx.request_permission(
        "bash",
        args([
            ("command", json!("ls -la /tmp/foo")),
            ("description", json!("List files in /tmp/foo")),
            ("access", json!("write")),
        ]),
    );
}

app_story!(bash_permission_dialog, |ctx| {
    ctx.set_viewport(80, 22);
    open_bash_permission_dialog(ctx, true);
    ctx.assert_snapshot();
});

app_story!(bash_permission_dialog_without_sandbox, |ctx| {
    open_bash_permission_dialog(ctx, false);
    ctx.wait_for_text("Run without the sandbox?");
});

app_story!(permission_request_deferred_while_typing, |ctx| {
    ctx.set_viewport(80, 14);
    ctx.start_turn();
    ctx.type_prompt("Continue drafting this response");
    open_bash_permission_dialog(ctx, true);
    ctx.assert_snapshot();
});

app_story!(bash_permission_preserves_reasoning_context, |ctx| {
    ctx.set_viewport(80, 22);
    ctx.engine(EngineEvent::Reasoning {
        kind: protocol::ReasoningKind::Raw,
        title: None,
        content: "I need to inspect the generated fixtures before choosing an implementation.\n\nThe temporary directory should reveal whether reusable outputs already exist.".into(),
    });
    ctx.run_lua("smelt.transcript.fold_all('open')");
    open_bash_permission_dialog(ctx, true);
    ctx.assert_snapshot();
});

app_story!(bash_permission_dialog_expanded_max_height, |ctx| {
    ctx.set_viewport(80, 22);
    open_bash_permission_dialog(ctx, true);
    ctx.expand_active_dialog_to_max_height();
    ctx.assert_snapshot();
});

app_story!(bash_permission_reason_input_wraps, |ctx| {
    ctx.set_viewport(58, 22);
    open_bash_permission_dialog(ctx, true);
    ctx.press_tab();
    ctx.type_prompt(
        "Needed to inspect the temporary directory before deciding whether the generated fixtures can be reused.",
    );
    ctx.assert_snapshot();
});

app_story!(bash_permission_dialog_tall_terminal, |ctx| {
    ctx.set_viewport(80, 36);
    set_sandbox_available(ctx, true);
    ctx.request_permission(
        "bash",
        args([
            ("command", json!("ls -la /tmp/foo")),
            ("description", json!("List files in /tmp/foo")),
        ]),
    );
    ctx.assert_snapshot();
});

app_story!(bash_multiline_command_summary, |ctx| {
    ctx.set_viewport(80, 22);
    set_sandbox_available(ctx, true);
    ctx.request_permission(
        "bash",
        args([(
            "command",
            json!("for f in *.rs; do\n  rustfmt \"$f\"\ndone"),
        )]),
    );
    ctx.assert_snapshot();
});

app_story!(bash_long_command_summary_wraps, |ctx| {
    ctx.set_viewport(70, 22);
    set_sandbox_available(ctx, true);
    ctx.request_permission(
        "bash",
        args([(
            "command",
            json!("ls -ld /Users/leo/.dotfiles; realpath /Users/leo/.dotfiles 2>/dev/null || true; cd /Users/leo/dev/rust/smelt/.worktrees/fix-path-parsing && cargo test -p smelt-core permissions::tests"),
        )]),
    );
    ctx.assert_snapshot();
});

app_story!(switch_cwd_outside_workspace_permission_dialog, |ctx| {
    ctx.set_viewport(80, 22);
    ctx.request_permission("switch_cwd", args([("path", json!("/tmp/electronics"))]));
    ctx.assert_snapshot();
});

app_story!(write_file_permission_dialog_with_file_view, |ctx| {
    ctx.set_viewport(80, 24);
    ctx.request_permission(
        "write_file",
        args([
            ("file_path", json!("src/main.rs")),
            (
                "content",
                json!("fn main() {\n    println!(\"hello, world\");\n}\n"),
            ),
        ]),
    );
    ctx.assert_snapshot();
});

app_story!(edit_file_permission_dialog_with_diff, |ctx| {
    ctx.set_viewport(80, 24);
    let content = "fn main() {\n    println!(\"hello, world\");\n}\n";
    let path = ctx.write_fixture("src/main.rs", content);
    ctx.cache_file_read(&path, content);
    ctx.request_permission(
        "edit_file",
        args([
            ("file_path", json!(path)),
            ("old_string", json!("println!(\"hello, world\");")),
            ("new_string", json!("println!(\"hello, smelt\");")),
        ]),
    );
    ctx.assert_snapshot();
});

fn open_edit_file_permission_after_draft(ctx: &mut AppStoryCtx) {
    ctx.set_viewport(80, 30);
    let content = "fn main() {\n    println!(\"hello, world\");\n}\n";
    let path = ctx.write_fixture("src/main.rs", content);
    ctx.cache_file_read(&path, content);
    let old_string = "println!(\"hello, world\");";
    let new_string = "println!(\"hello, smelt\");";
    ctx.engine(EngineEvent::ToolCallDraftStarted {
        stream_id: "draft-1".into(),
        call_id: Some("call-1".into()),
        tool_name: Some("edit_file".into()),
        native: false,
    });
    ctx.engine(EngineEvent::ToolCallDraftFinished {
        stream_id: "draft-1".into(),
        call_id: "call-1".into(),
        tool_name: "edit_file".into(),
        arguments: json!({
            "file_path": path,
            "old_string": old_string,
            "new_string": new_string,
        })
        .to_string(),
    });
    ctx.request_permission(
        "edit_file",
        args([
            ("file_path", json!(path)),
            ("old_string", json!(old_string)),
            ("new_string", json!(new_string)),
        ]),
    );
}

app_story!(
    edit_file_permission_dialog_collapses_transcript_diff,
    |ctx| {
        open_edit_file_permission_after_draft(ctx);
        ctx.assert_snapshot();
    }
);

app_story!(
    edit_file_permission_allowed_restores_transcript_diff,
    |ctx| {
        open_edit_file_permission_after_draft(ctx);
        ctx.press_char('1');
        ctx.assert_snapshot();
    }
);

app_story!(
    write_file_permission_dialog_collapses_transcript_body,
    |ctx| {
        ctx.set_viewport(80, 30);
        let content = "fn main() {\n    println!(\"hello, world\");\n}\n";
        ctx.engine(EngineEvent::ToolCallDraftStarted {
            stream_id: "draft-1".into(),
            call_id: Some("call-1".into()),
            tool_name: Some("write_file".into()),
            native: false,
        });
        ctx.engine(EngineEvent::ToolCallDraftFinished {
            stream_id: "draft-1".into(),
            call_id: "call-1".into(),
            tool_name: "write_file".into(),
            arguments: json!({ "file_path": "src/main.rs", "content": content }).to_string(),
        });
        ctx.request_permission(
            "write_file",
            args([
                ("file_path", json!("src/main.rs")),
                ("content", json!(content)),
            ]),
        );
        ctx.assert_snapshot();
    }
);

app_story!(edit_file_long_path_permission_dialog_wraps, |ctx| {
    ctx.set_viewport(70, 24);
    let content = "# Plan\n";
    let path = ctx.write_fixture(
        "smelt-storybook-session-93e4220e873c55398e6e22ff065d617e27efc1a9b326032e323d00a522764901/plans/20260618-083946-virtual-transcript-performance/plan.md",
        content,
    );
    ctx.cache_file_read(&path, content);
    ctx.request_permission(
        "edit_file",
        args([
            ("file_path", json!(path)),
            ("old_string", json!("# Plan\n")),
            ("new_string", json!("# Updated plan\n")),
        ]),
    );
    ctx.assert_snapshot();
});

app_story!(edit_file_permission_dialog_multiline_diff, |ctx| {
    ctx.set_viewport(80, 28);
    let content = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let path = ctx.write_fixture("src/lib.rs", content);
    ctx.cache_file_read(&path, content);
    ctx.request_permission(
        "edit_file",
        args([
            ("file_path", json!(path)),
            (
                "old_string",
                json!("fn add(a: i32, b: i32) -> i32 {\n    a + b\n}"),
            ),
            (
                "new_string",
                json!(
                    "fn add(a: i64, b: i64) -> i64 {\n    a.checked_add(b).expect(\"overflow\")\n}"
                ),
            ),
        ]),
    );
    ctx.assert_snapshot();
});

app_story!(notebook_edit_permission_dialog_with_diff, |ctx| {
    ctx.set_viewport(80, 28);
    ctx.run_lua("require('smelt.plugins.notebook')");
    // Prepare a read snapshot of a two-cell notebook for the cell-level diff.
    let nb_json = r##"{
      "nbformat": 4,
      "nbformat_minor": 5,
      "metadata": {},
      "cells": [
        { "cell_type": "markdown", "id": "intro",
          "metadata": {}, "source": ["# Analysis\n"] },
        { "cell_type": "code", "id": "load",
          "metadata": {}, "execution_count": null, "outputs": [],
          "source": ["import pandas as pd\n", "df = pd.read_csv('data.csv')\n"] }
      ]
    }"##;
    let path = ctx.write_fixture("analysis.ipynb", nb_json);
    ctx.cache_file_read(&path, nb_json);
    ctx.request_permission(
        "edit_notebook",
        args([
            ("notebook_path", json!(path)),
            ("cell_id", json!("load")),
            ("edit_mode", json!("replace")),
            (
                "new_source",
                json!("import pandas as pd\ndf = pd.read_csv('data.csv')\nprint(df.head())\n"),
            ),
        ]),
    );
    ctx.assert_snapshot();
});

app_story!(notebook_edit_permission_dialog_insert_cell, |ctx| {
    ctx.set_viewport(80, 22);
    ctx.run_lua("require('smelt.plugins.notebook')");
    let nb_json = r##"{
      "nbformat": 4,
      "nbformat_minor": 5,
      "metadata": {},
      "cells": [
        { "cell_type": "code", "id": "first",
          "metadata": {}, "execution_count": null, "outputs": [],
          "source": ["print('hello')\n"] }
      ]
    }"##;
    let path = ctx.write_fixture("insert.ipynb", nb_json);
    ctx.cache_file_read(&path, nb_json);
    ctx.request_permission(
        "edit_notebook",
        args([
            ("notebook_path", json!(path)),
            ("cell_id", json!("first")),
            ("edit_mode", json!("insert")),
            ("cell_type", json!("code")),
            (
                "new_source",
                json!("import numpy as np\nx = np.arange(10)\n"),
            ),
        ]),
    );
    ctx.assert_snapshot();
});

app_story!(web_fetch_permission_dialog_no_preview, |ctx| {
    ctx.set_viewport(70, 18);
    // `web_fetch` registers no `preview`; the dialog collapses the
    // preview panel and shows summary + options.
    ctx.request_permission(
        "web_fetch",
        args([
            ("url", json!("https://example.com/page")),
            ("prompt", json!("Extract the main heading.")),
        ]),
    );
    ctx.assert_snapshot();
});

app_story!(stop_process_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission("stop_process", args([("id", json!("4242"))]));
    ctx.assert_snapshot();
});

app_story!(smelt_reload_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission("smelt_reload", HashMap::new());
    ctx.assert_snapshot();
});
