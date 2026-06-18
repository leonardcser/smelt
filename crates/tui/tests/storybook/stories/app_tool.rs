//! Per-tool transcript rendering. Each story groups one block type's
//! lifecycle states together: drafting, pending, expanded success,
//! expanded error, then the same transcript collapsed in a second step.

use serde_json::json;

use crate::app_story;

app_story!(bash_tool_states, |ctx| {
    ctx.set_viewport(72, 20);
    ctx.tool_draft(
        "bash",
        r#"{"command":"cargo test -p smelt-tui app::drafts","description":"Run draft preview tests"}"#,
    );
    ctx.tool_started("bash", &[("command", json!("sleep 5"))]);
    ctx.tool_call(
        "bash",
        &[
            ("command", json!("git status --short")),
            ("description", json!("Check working tree status")),
        ],
        " M src/lib.rs\n?? src/new.rs",
        Some(42),
    );
    ctx.tool_call_error(
        "bash",
        &[
            ("command", json!("cat missing.txt")),
            ("description", json!("Read a missing file")),
        ],
        "cat: missing.txt: No such file or directory",
        Some(7),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(write_file_tool_states, |ctx| {
    ctx.set_viewport(78, 22);
    ctx.tool_draft(
        "write_file",
        r#"{"file_path":"src/greet.rs","content":"pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}"}"#,
    );
    ctx.tool_started(
        "write_file",
        &[
            ("file_path", json!("src/pending.rs")),
            ("content", json!("pending\n")),
        ],
    );
    ctx.tool_call(
        "write_file",
        &[
            ("file_path", json!("src/greet.rs")),
            (
                "content",
                json!("pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n"),
            ),
        ],
        "ok",
        Some(5),
    );
    ctx.tool_call_error(
        "write_file",
        &[
            ("file_path", json!("/root/forbidden.rs")),
            ("content", json!("ignored\n")),
        ],
        "permission denied: /root/forbidden.rs",
        Some(1),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(streaming_tool_draft_syntax_highlighting, |ctx| {
    ctx.set_viewport(78, 14);
    ctx.tool_draft_delta(
        "write_file",
        r#"{"file_path":"src/live.rs","content":"pub fn live(name: &str) -> String {\n    format!(\"hello, {name}\")"#,
    );
    ctx.tool_draft_delta(
        "bash",
        r#"{"command":"cargo test -p smelt-tui app::drafts -- --nocapture"#,
    );
    ctx.assert_snapshot();
});

app_story!(streaming_edit_file_draft_diff, |ctx| {
    ctx.set_viewport(78, 14);
    ctx.tool_draft_delta(
        "edit_file",
        r#"{"file_path":"src/live.rs","old_string":"pub fn live() -> i32 {\n    1\n}","new_string":"pub fn live() -> i32 {\n    2"#,
    );
    ctx.assert_snapshot();
});

app_story!(edit_file_tool_states, |ctx| {
    ctx.set_viewport(84, 30);
    ctx.tool_draft(
        "edit_file",
        r#"{"file_path":"src/lib.rs","old_string":"fn add(a: i32, b: i32) -> i32 {\n    a + b\n}","new_string":"fn add(a: i64, b: i64) -> i64 {\n    a.checked_add(b).expect(\"overflow\")\n}"}"#,
    );
    ctx.tool_started(
        "edit_file",
        &[
            ("file_path", json!("src/pending.rs")),
            ("old_string", json!("old")),
            ("new_string", json!("new")),
        ],
    );
    ctx.tool_call_with_metadata(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
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
        ],
        "ok",
        json!({
            "old_content": "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}",
            "new_content": "fn add(a: i64, b: i64) -> i64 {\n    a.checked_add(b).expect(\"overflow\")\n}",
            "path": "src/lib.rs",
        }),
        Some(7),
    );
    ctx.tool_call_with_metadata(
        "edit_file",
        &[
            ("file_path", json!("runtime/lua/smelt/plugins/compact.lua")),
            ("old_string", json!("Use them as evidence for task priority and return instructions.")),
            ("new_string", json!("Use them as evidence for task priority and return/resume instructions.")),
        ],
        "edited runtime/lua/smelt/plugins/compact.lua",
        json!({
            "old_content": "line 1\nUse them as evidence for task priority and return instructions.\nline 3\n",
            "new_content": "line 1\nUse them as evidence for task priority and return/resume instructions.\nline 3\n",
            "path": "runtime/lua/smelt/plugins/compact.lua"
        }),
        Some(5),
    );
    ctx.tool_call_error(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("fn missing()")),
            ("new_string", json!("fn replaced()")),
        ],
        "old_string not found in src/lib.rs",
        Some(4),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(edit_file_rejected_without_speculative_diff, |ctx| {
    ctx.set_viewport(84, 18);
    ctx.tool_rejected(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("fn missing()")),
            ("new_string", json!("fn replaced()")),
        ],
        "read_file must be called before edit_file",
        true,
        protocol::StyledLines::empty(),
        Some(1),
    );
    ctx.tool_rejected(
        "edit_file",
        &[
            ("file_path", json!("src/blocked.rs")),
            ("old_string", json!("old")),
            ("new_string", json!("new")),
        ],
        "The user's permission settings blocked this tool call.",
        false,
        protocol::StyledLines::empty(),
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(edit_file_success_uses_output_metadata_diff, |ctx| {
    ctx.set_viewport(84, 14);
    ctx.tool_call_with_metadata(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("old")),
            ("new_string", json!("new")),
        ],
        "ok",
        json!({
            "old_content": "old\n",
            "new_content": "new\n",
            "path": "src/lib.rs",
        }),
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(edit_notebook_tool_states, |ctx| {
    ctx.set_viewport(84, 22);
    ctx.tool_draft(
        "edit_notebook",
        r#"{"notebook_path":"analysis.ipynb","cell_number":0,"new_source":"import pandas as pd\ndf = pd.read_csv(\"data.csv\")\n"}"#,
    );
    ctx.tool_call_with_metadata(
        "edit_notebook",
        &[
            ("notebook_path", json!("analysis.ipynb")),
            ("cell_number", json!(0)),
            (
                "new_source",
                json!("import pandas as pd\ndf = pd.read_csv(\"data.csv\")\n"),
            ),
        ],
        "ok",
        json!({
            "edit_mode": "replace",
            "path": "analysis.ipynb#cell0",
            "old_source": "import pandas\ndf = pandas.read_csv('data.csv')\n",
            "new_source": "import pandas as pd\ndf = pd.read_csv(\"data.csv\")\n",
        }),
        Some(6),
    );
    ctx.tool_call_with_metadata(
        "edit_notebook",
        &[
            ("notebook_path", json!("analysis.ipynb")),
            ("edit_mode", json!("insert")),
            ("new_source", json!("print(\"new cell\")\n")),
        ],
        "ok",
        json!({
            "edit_mode": "insert",
            "path": "analysis.ipynb#cell1",
            "new_source": "print(\"new cell\")\n",
        }),
        Some(5),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(read_file_tool_states, |ctx| {
    ctx.set_viewport(68, 16);
    ctx.tool_started("read_file", &[("file_path", json!("pending.rs"))]);
    ctx.tool_call(
        "read_file",
        &[("file_path", json!("src/main.rs"))],
        "fn main() {\n    println!(\"hi\");\n}\n",
        Some(3),
    );
    ctx.tool_call_error(
        "read_file",
        &[("file_path", json!("missing.rs"))],
        "could not read file: missing.rs",
        Some(2),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(glob_tool_states, |ctx| {
    ctx.set_viewport(72, 18);
    ctx.tool_call_with_metadata(
        "glob",
        &[
            ("pattern", json!("**/*.rs")),
            ("path", json!("crates/term")),
        ],
        "crates/term/src/grid.rs\ncrates/term/src/lib.rs\ncrates/term/src/snapshot.rs",
        json!({ "display_count": { "value": 3, "unit": "file" } }),
        Some(4),
    );
    ctx.tool_call_with_metadata(
        "glob",
        &[("pattern", json!("missing/**/*.rs"))],
        "no matches found",
        json!({ "display_count": { "value": 0, "unit": "file" } }),
        Some(2),
    );
    ctx.tool_call_error(
        "glob",
        &[("pattern", json!("[invalid"))],
        "invalid glob pattern: missing ]",
        Some(1),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(grep_tool_states, |ctx| {
    ctx.set_viewport(76, 18);
    ctx.tool_call_with_metadata(
        "grep",
        &[
            ("pattern", json!("fn render")),
            ("path", json!("crates/tui/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/tui/src/render/markdown.rs\ncrates/tui/src/render/blocks/tool.rs",
        json!({ "display_count": { "value": 2, "unit": "file" } }),
        Some(11),
    );
    ctx.tool_call_with_metadata(
        "grep",
        &[
            ("pattern", json!("definitely_not_present")),
            ("path", json!("crates/tui/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "no matches found",
        json!({ "display_count": { "value": 0, "unit": "file" } }),
        Some(4),
    );
    ctx.tool_call_error(
        "grep",
        &[
            ("pattern", json!("(?P<bad")),
            ("path", json!("crates/tui/src")),
        ],
        "regex parse error: unterminated group",
        Some(3),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(workspace_tool_states, |ctx| {
    ctx.set_viewport(90, 18);
    ctx.tool_call_with_metadata(
        "enter_worktree",
        &[("name", json!("Native worktree support")), ("base", json!("main"))],
        "entered managed worktree native-worktree-support\npath: /repo/.worktrees/native-worktree-support\nbranch: native-worktree-support\nbase: main",
        json!({
            "name": "native-worktree-support",
            "path": "/repo/.worktrees/native-worktree-support",
            "branch": "native-worktree-support",
            "base": "main",
            "created": true,
        }),
        Some(1280),
    );
    ctx.tool_call_with_metadata(
        "switch_cwd",
        &[("path", json!("/repo/.worktrees/native-worktree-support"))],
        "cwd: /repo/.worktrees/native-worktree-support",
        json!({ "cwd": "/repo/.worktrees/native-worktree-support" }),
        Some(85),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(present_plan_tool_states, |ctx| {
    ctx.set_viewport(84, 22);
    ctx.tool_started(
        "present_plan",
        &[
            ("title", json!("Parser refactor")),
            ("slug", json!("parser-refactor")),
            (
                "plan",
                json!("# Goal\nRefactor parser state.\n\n# Verification\nRun parser tests."),
            ),
        ],
    );
    ctx.tool_call(
        "present_plan",
        &[
            ("title", json!("Parser refactor")),
            ("slug", json!("parser-refactor")),
            (
                "plan",
                json!("# Goal\nRefactor parser state.\n\n```rust\nfn parse(input: &str) -> Ast {\n    todo!()\n}\n```\n\n# Verification\nRun parser tests."),
            ),
        ],
        "wrote plan to /tmp/smelt/sessions/sess/plans/20260101-000000-parser-refactor/plan.md",
        Some(9),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(web_tool_states, |ctx| {
    ctx.set_viewport(76, 18);
    ctx.tool_call(
        "web_fetch",
        &[
            ("url", json!("https://example.com/release")),
            ("prompt", json!("Summarise the release notes")),
        ],
        "0.6 ships streaming markdown and removes the legacy renderer.",
        Some(820),
    );
    ctx.tool_call(
        "web_search",
        &[("query", json!("rust unicode width crate"))],
        "1. unicode-width - crates.io\n2. unicode-segmentation - crates.io",
        Some(330),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(parallel_pending_tool_states, |ctx| {
    ctx.set_viewport(64, 12);
    ctx.tool_started("read_file", &[("file_path", json!("a.rs"))]);
    ctx.tool_started("read_file", &[("file_path", json!("b.rs"))]);
    ctx.assert_snapshot();
});

app_story!(tool_header_wrapping_for_bash_glob_and_grep, |ctx| {
    ctx.set_viewport(62, 18);
    ctx.tool_call(
        "bash",
        &[
            (
                "command",
                json!("cargo test -p smelt-tui content::display_renderers::layout_ir::tests::runs_continuation_indent_aligns_soft_wrapped_rows"),
            ),
            (
                "description",
                json!("Run the focused transcript layout wrapping regression"),
            ),
        ],
        "test result: ok",
        Some(6100),
    );
    ctx.tool_call_with_metadata(
        "glob",
        &[
            ("pattern", json!("crates/tui/tests/storybook/snapshots/app_tool::*.snap")),
            ("path", json!("/Users/leo/dev/rust/smelt/.worktrees/transcript-layout-rewrite")),
        ],
        "crates/tui/tests/storybook/snapshots/app_tool::grep_tool_block.snap\ncrates/tui/tests/storybook/snapshots/app_tool::glob_tool_block.snap",
        json!({ "display_count": { "value": 2, "unit": "file" } }),
        Some(34),
    );
    ctx.tool_call_with_metadata(
        "grep",
        &[
            ("pattern", json!("wrap_fragments_with_widths|continuation_indent|ToolSummaryResolver")),
            ("path", json!("crates/tui/src crates/buffer/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/buffer/src/inline_line.rs\ncrates/tui/src/app/history.rs\ncrates/tui/src/content/display_renderers/layout_ir.rs",
        json!({ "display_count": { "value": 3, "unit": "file" } }),
        Some(89),
    );
    ctx.assert_snapshot();
});

app_story!(mcp_tool_json_args_wrap_in_header, |ctx| {
    ctx.set_viewport(82, 10);
    ctx.tool_call(
        "nvim_lsp_read_lints",
        &[
            (
                "files",
                json!(["/Users/leo/dev/rust/smelt/.worktrees/transcript-layout-rewrite/PLAN-transcript-layout-rewrite.md"]),
            ),
            (
                "workspace",
                json!("/Users/leo/dev/rust/smelt/.worktrees/transcript-layout-rewrite"),
            ),
        ],
        "[]",
        Some(6000),
    );
    ctx.assert_snapshot();
});

app_story!(lsp_tool_states, |ctx| {
    ctx.set_viewport(96, 86);
    ctx.run_lua("require('smelt.plugins.lsp').setup({ servers = {} })");
    ctx.tool_call_with_metadata(
        "lsp_status",
        &[("file_path", json!("crates/core/src/lsp.rs"))],
        r#"{
  "servers": [
    {
      "name": "rust",
      "state": "ready",
      "root": "/repo/smelt"
    }
  ]
}"#,
        json!({ "syntax": "json" }),
        Some(2),
    );
    ctx.tool_call_with_metadata(
        "lsp_document_symbols",
        &[("file_path", json!("crates/core/src/lsp.rs"))],
        r#"[
  {
    "name": "LspManager",
    "kind": 5,
    "range": {
      "start": { "line": 70, "character": 0 },
      "end": { "line": 260, "character": 1 }
    }
  }
]"#,
        json!({ "syntax": "json" }),
        Some(18),
    );
    ctx.tool_call_with_metadata(
        "lsp_definition",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("line", json!(111)),
            ("column", json!(12)),
        ],
        r#"[
  {
    "uri": "file:///repo/smelt/crates/core/src/lsp.rs",
    "range": {
      "start": { "line": 70, "character": 11 },
      "end": { "line": 70, "character": 21 }
    }
  }
]"#,
        json!({ "syntax": "json" }),
        Some(9),
    );
    ctx.tool_call_with_metadata(
        "lsp_references",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("line", json!(111)),
            ("column", json!(12)),
            ("include_declaration", json!(false)),
        ],
        r#"[
  {
    "uri": "file:///repo/smelt/crates/core/src/lua/api/lsp.rs",
    "range": {
      "start": { "line": 43, "character": 22 },
      "end": { "line": 43, "character": 32 }
    }
  }
]"#,
        json!({ "syntax": "json" }),
        Some(27),
    );
    ctx.tool_call_with_metadata(
        "lsp_diagnostics",
        &[("file_path", json!("crates/core/src/lsp.rs"))],
        r#"[
  {
    "range": {
      "start": { "line": 210, "character": 8 },
      "end": { "line": 210, "character": 17 }
    },
    "severity": 2,
    "message": "unused variable: settings"
  }
]"#,
        json!({ "syntax": "json" }),
        Some(6),
    );
    ctx.tool_call_with_metadata(
        "lsp_rename_preview",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("line", json!(111)),
            ("column", json!(12)),
            ("new_name", json!("manager")),
        ],
        r#"{
  "changes": {
    "file:///repo/smelt/crates/core/src/lsp.rs": [
      {
        "range": {
          "start": { "line": 111, "character": 11 },
          "end": { "line": 111, "character": 21 }
        },
        "newText": "manager"
      }
    ]
  }
}"#,
        json!({ "syntax": "json" }),
        Some(15),
    );
    ctx.tool_call_with_metadata(
        "lsp_rename",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("line", json!(111)),
            ("column", json!(12)),
            ("new_name", json!("manager")),
        ],
        r#"{
  "applied": true,
  "files": ["/repo/smelt/crates/core/src/lsp.rs"],
  "edits": 1
}"#,
        json!({ "syntax": "json" }),
        Some(21),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});
