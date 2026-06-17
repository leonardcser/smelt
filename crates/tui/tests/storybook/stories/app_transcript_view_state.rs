//! Transcript block view-state stories. Tool and tool-group states live in
//! their own story modules so block-level collapsed/peek/expanded behavior is
//! easy to inspect without duplicate tool coverage.

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;

const THINKING: &str = "**Inspecting the renderer**\nRead the transcript model first.\nCheck the Lua defaults next.\nCompare the rendered rows.\nUpdate the stories last.\nRun the snapshot tests.\nReview the final diff.";
const THINKING_UNTITLED: &str = "Read the transcript model first.\nCheck the Lua defaults next.\nCompare the rendered rows.\nUpdate the stories last.\nRun the snapshot tests.\nReview the final diff.";
const COMPACTED_SUMMARY: &str =
    "Compacted 8 earlier turns: parser refactor, renderer wiring, three bug fixes.";
const COMPACTION_PREVIEW_SUMMARY: &str = concat!(
    "# Goal\nSummarize earlier turns while preserving the active task.\n\n",
    "# Progress\n- Read the transcript model.\n- Added a streaming preview block.\n",
    "- Kept the final checkpoint marker separate.\n\n",
    "# Next steps\nUpdate stories and run focused tests."
);

app_story!(user_message_block_states, |ctx| {
    ctx.set_viewport(64, 14);
    ctx.push_user_turn("Plan the parser refactor.\nKeep the API stable.\nAdd regression tests.");
    ctx.assert_snapshot_named("expanded");

    ctx.run_lua("smelt.transcript.fold_kind('user', 'close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(assistant_text_block_states, |ctx| {
    ctx.set_viewport(64, 14);
    ctx.engine(EngineEvent::Text {
        content: "I'll inspect the parser, update the call sites, and then run the focused regression tests.".into(),
    });
    ctx.assert_snapshot_named("expanded");

    ctx.run_lua("smelt.transcript.fold_kind('assistant', 'close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(code_line_block_states, |ctx| {
    ctx.set_viewport(64, 10);
    ctx.push_code_line("fn parse(input: &str) -> Result<Ast>", "rust");
    ctx.assert_snapshot_named("expanded");

    ctx.run_lua("smelt.transcript.fold_kind('code', 'close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(mode_block_states, |ctx| {
    ctx.set_viewport(64, 10);
    ctx.push_mode_block("Plan mode enabled", "◆ ", "SmeltAccent");
    ctx.assert_snapshot_named("expanded");

    ctx.run_lua("smelt.transcript.fold_kind('mode', 'close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(process_status_block_states, |ctx| {
    ctx.set_viewport(64, 10);
    ctx.push_process_status_text("background process 4210 completed successfully");
    ctx.assert_snapshot_named("expanded");

    ctx.run_lua("smelt.transcript.fold_kind('process_status', 'close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(exec_block_states, |ctx| {
    ctx.set_viewport(64, 14);
    ctx.exec_with_output("ls -1 src", "lib.rs\nmain.rs\nparser.rs", Some(0));
    ctx.assert_snapshot_named("expanded");

    ctx.run_lua("smelt.transcript.fold_kind('exec', 'close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(thinking_block_states, |ctx| {
    ctx.set_viewport(56, 18);
    ctx.engine(EngineEvent::Thinking {
        content: THINKING.into(),
    });
    ctx.engine(EngineEvent::Thinking {
        content: THINKING_UNTITLED.into(),
    });

    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'close')");
    ctx.assert_snapshot_named("collapsed");

    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'peek')");
    ctx.assert_snapshot_named("peek");

    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'open')");
    ctx.assert_snapshot_named("expanded");
});

app_story!(compacted_block_states, |ctx| {
    ctx.set_viewport(60, 9);
    ctx.push_compacted(COMPACTED_SUMMARY);
    ctx.assert_snapshot_named("collapsed");

    ctx.run_lua("smelt.transcript.fold_kind('compacted', 'open')");
    ctx.assert_snapshot_named("expanded");
});

app_story!(compaction_preview_block_states, |ctx| {
    ctx.set_viewport(60, 16);
    ctx.push_compaction_preview(COMPACTION_PREVIEW_SUMMARY);
    ctx.assert_snapshot_named("peek");

    ctx.run_lua("smelt.transcript.fold_kind('compaction_preview', 'open')");
    ctx.assert_snapshot_named("expanded");

    ctx.run_lua("smelt.transcript.fold_kind('compaction_preview', 'close')");
    ctx.assert_snapshot_named("collapsed");
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

app_story!(bash_multiline_header_capped, |ctx| {
    ctx.set_viewport(86, 12);
    ctx.run_lua(
        r#"
        smelt.settings.transcript = {
          limits = { tool_header_rows = 3 },
        }
        smelt.transcript.invalidate_renderer()
        "#,
    );
    ctx.tool_call(
        "bash",
        &[(
            "command",
            json!("line one\nline two\nline three\nline four\nline five\nline six"),
        )],
        "done",
        Some(42_000),
    );
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
