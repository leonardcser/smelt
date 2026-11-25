//! Per-tool transcript rendering. Each story groups one block type's
//! lifecycle states together: drafting, pending, expanded success,
//! expanded error, then the same transcript collapsed in a second step.

use crossterm::event::{KeyCode, KeyModifiers};
use protocol::EngineEvent;
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

app_story!(tool_header_parts_compose_chrome, |ctx| {
    ctx.set_viewport(56, 20);
    ctx.tool_call(
        "bash",
        &[
            ("command", json!("cargo test --workspace --features smelt-tui/harness --all-targets -- --nocapture --test-threads=1")),
            ("description", json!("Check header wrapping")),
        ],
        "all checks passed",
        Some(1234),
    );
    for pattern in ["alpha", "beta"] {
        ctx.tool_call(
            "grep",
            &[("pattern", json!(pattern))],
            "no matches",
            Some(12),
        );
    }
    ctx.tool_started("bash", &[("command", json!("cargo clippy --workspace"))]);
    ctx.advance_time(1250);
    ctx.run_lua(
        r#"
        local t, l = smelt.transcript, smelt.layout
        smelt.settings.transcript.limits.tool_header_rows = 2
        header_chrome = t.extend_renderer("storybook.chrome", function(_, block, render_ctx)
            assert(block.id and block.index ~= nil)
            if block.kind == "group" then assert(block.child_count == 2) end
            return l.row_prefix(t.get_renderer("tool_header_content")(block, render_ctx), {
                first = { { text = "界 ", hl = "SmeltAccent", selectable = false } },
                rest = { { text = "   ", selectable = false } },
            })
        end, { part = "tool_header" })
    "#,
    );
    ctx.assert_snapshot_named("custom_chrome");
    ctx.run_lua(
        r#"
        header_chrome:remove()
        local t = smelt.transcript
        default_header = t.get_renderer("tool_header")
        t.set_renderer(t.get_renderer("tool_header_content"), { part = "tool_header" })
    "#,
    );
    ctx.assert_snapshot_named("content_only");
    ctx.run_lua("smelt.transcript.set_renderer(default_header, { part = 'tool_header' })");
    ctx.assert_snapshot_named("restored_chrome");
});

app_story!(bash_background_hint_appears_after_ten_seconds, |ctx| {
    ctx.set_viewport(72, 10);
    ctx.tool_started("bash", &[("command", json!("cargo build --release"))]);

    ctx.advance_time(9000);
    ctx.assert_snapshot_named("nine_seconds");

    ctx.advance_time(1500);
    ctx.assert_snapshot_named("ten_seconds");

    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("ten_seconds_collapsed");
});

/// Claude Code's report of the dev server the Bash call `bash-1` runs.
fn dev_server(background: bool) -> EngineEvent {
    EngineEvent::NativeProcesses {
        processes: vec![protocol::NativeProcess {
            id: "claude-code:task".into(),
            command: "npm run dev".into(),
            started_at_ms: 0,
            background,
            agent_id: None,
            call_id: Some("bash-1".into()),
            output: String::new(),
        }],
    }
}

app_story!(native_call_moves_to_background_at_once, |ctx| {
    ctx.set_viewport(72, 10);
    // Claude Code's Bash shows as smelt's bash.
    let invocation = ctx.tool_started("bash", &[("command", json!("npm run dev"))]);
    let session_id = ctx.session_id();
    ctx.engine(EngineEvent::NativeSessionState {
        session_id,
        active: true,
        working: true,
    });
    ctx.engine(dev_server(false));
    ctx.advance_time(10500);
    ctx.assert_snapshot_named("waiting");

    ctx.press_key(KeyCode::Char('g'), KeyModifiers::CONTROL);
    ctx.assert_snapshot_named("moved");
    // The job's elapsed time keeps advancing after detaching.
    ctx.advance_time(2000);
    ctx.assert_snapshot_named("held");

    ctx.engine(EngineEvent::ActionFailed {
        action: protocol::ActionId::new(1),
        error: "busy".into(),
    });
    ctx.advance_time(100);
    ctx.assert_snapshot_named("refused");

    // Asked again, what Claude Code reports next shows the same: first the
    // shell in the background, then the call's result.
    ctx.press_key(KeyCode::Char('g'), KeyModifiers::CONTROL);
    ctx.engine(dev_server(true));
    ctx.assert_snapshot_named("reported");
    ctx.tool_finished(
        invocation,
        "bash-1",
        protocol::ToolOutcome::new(
            "Command running in background with ID: task".into(),
            false,
            Some(json!({"background": true, "background_id": "claude-code:task"})),
        ),
        Some(12_000),
    );
    ctx.assert_snapshot_named("confirmed");
});

app_story!(agent_background_hint_appears_after_ten_seconds, |ctx| {
    ctx.set_viewport(72, 10);
    ctx.tool_started(
        "claude-code/Agent",
        &[
            ("description", json!("Review diff")),
            ("prompt", json!("Review")),
        ],
    );
    ctx.advance_time(9000);
    ctx.assert_snapshot_named("nine_seconds");
    ctx.advance_time(1500);
    ctx.assert_snapshot_named("ten_seconds");
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
    ctx.finish_stream_reveal();
    ctx.assert_snapshot();
});

app_story!(streaming_edit_file_draft_diff, |ctx| {
    ctx.set_viewport(78, 14);
    let source = "pub fn live() -> i32 {\n    1\n}\n";
    let path = ctx.write_fixture("src/live.rs", source);
    ctx.cache_file_read(&path, source);
    ctx.tool_draft_delta(
        "edit_file",
        &format!(
            r#"{{"file_path":{path:?},"old_string":"pub fn live() -> i32 {{\n    1\n}}","new_string":"pub fn live() -> i32 {{\n    2"#
        ),
    );
    ctx.finish_stream_reveal();
    ctx.assert_snapshot();
});

app_story!(edit_file_tool_states, |ctx| {
    ctx.set_viewport(84, 30);
    let source = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let path = ctx.write_fixture("src/lib.rs", source);
    ctx.cache_file_read(&path, source);
    ctx.tool_draft(
        "edit_file",
        &format!(
            r#"{{"file_path":{path:?},"old_string":"fn add(a: i32, b: i32) -> i32 {{\n    a + b\n}}","new_string":"fn add(a: i64, b: i64) -> i64 {{\n    a.checked_add(b).expect(\"overflow\")\n}}"}}"#
        ),
    );
    ctx.tool_started(
        "edit_file",
        &[
            ("file_path", json!("src/pending.rs")),
            ("old_string", json!("old")),
            ("new_string", json!("new")),
        ],
    );
    ctx.tool_call_with_display_content(
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
        json!({ "path": "src/lib.rs" }),
        &[
            (
                "old_content",
                "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}",
            ),
            (
                "new_content",
                "fn add(a: i64, b: i64) -> i64 {\n    a.checked_add(b).expect(\"overflow\")\n}",
            ),
        ],
        Some(7),
    );
    ctx.tool_call_with_display_content(
        "edit_file",
        &[
            ("file_path", json!("runtime/lua/smelt/plugins/compact.lua")),
            ("old_string", json!("Use them as evidence for task priority and return instructions.")),
            ("new_string", json!("Use them as evidence for task priority and return/resume instructions.")),
        ],
        "edited runtime/lua/smelt/plugins/compact.lua",
        json!({ "path": "runtime/lua/smelt/plugins/compact.lua" }),
        &[
            (
                "old_content",
                "line 1\nUse them as evidence for task priority and return instructions.\nline 3\n",
            ),
            (
                "new_content",
                "line 1\nUse them as evidence for task priority and return/resume instructions.\nline 3\n",
            ),
        ],
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

app_story!(edit_file_success_uses_retained_content_diff, |ctx| {
    ctx.set_viewport(84, 14);
    ctx.tool_call_with_display_content(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("old")),
            ("new_string", json!("new")),
        ],
        "ok",
        json!({ "path": "src/lib.rs" }),
        &[("old_content", "old\n"), ("new_content", "new\n")],
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(edit_notebook_tool_states, |ctx| {
    ctx.set_viewport(84, 22);
    ctx.run_lua("require('smelt.plugins.notebook')");
    let notebook = r#"{"cells":[]}"#;
    let path = ctx.write_fixture("analysis.ipynb", notebook);
    ctx.cache_file_read(&path, notebook);
    ctx.tool_draft(
        "edit_notebook",
        &format!(
            r#"{{"notebook_path":{path:?},"cell_number":0,"new_source":"import pandas as pd\ndf = pd.read_csv(\"data.csv\")\n"}}"#
        ),
    );
    ctx.tool_call_with_display_content(
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
        }),
        &[
            (
                "old_source",
                "import pandas\ndf = pandas.read_csv('data.csv')\n",
            ),
            (
                "new_source",
                "import pandas as pd\ndf = pd.read_csv(\"data.csv\")\n",
            ),
        ],
        Some(6),
    );
    ctx.tool_call_with_display_content(
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
        }),
        &[("old_source", ""), ("new_source", "print(\"new cell\")\n")],
        Some(5),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(show_ansi_renders_colors_and_plain_text, |ctx| {
    ctx.set_viewport(60, 8);
    ctx.run_lua("require('smelt.plugins.show_ansi')");
    ctx.tool_call_with_display_content(
        "show_ansi",
        &[("path", json!("out.ansi"))],
        "shown to the user",
        json!({}),
        &[(
            "ansi",
            "\x1b[1mbold\x1b[0m \x1b[31mred\x1b[0m \x1b[38;5;208morange\x1b[0m \x1b[48;2;40;40;80m bg \x1b[0m\nplain text\n",
        )],
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(plot_stacked_area_reflows_with_the_width, |ctx| {
    ctx.set_viewport(64, 18);
    ctx.run_lua("require('smelt.plugins.plot')");
    ctx.tool_call(
        "plot",
        &[
            ("kind", json!("area")),
            ("title", json!("Monthly signups by plan")),
            (
                "x_labels",
                json!(["jan", "feb", "mar", "apr", "may", "jun"]),
            ),
            (
                "series",
                json!([
                    { "name": "free", "y": [120, 150, 180, 220, 260, 300] },
                    { "name": "pro", "y": [40, 60, 70, 90, 130, 160] },
                    { "name": "team", "y": [10, 15, 30, 35, 50, 80] },
                ]),
            ),
            ("height", json!(8)),
        ],
        "shown to the user",
        Some(1),
    );
    ctx.assert_snapshot_named("wide");
    ctx.set_viewport(40, 18);
    ctx.assert_snapshot_named("narrow");
});

app_story!(plot_heatmap_tiles_and_pixels, |ctx| {
    ctx.set_viewport(62, 36);
    ctx.run_lua("require('smelt.plugins.plot')");
    ctx.tool_call(
        "plot",
        &[
            ("kind", json!("heatmap")),
            ("labels", json!(["cpu", "mem", "io"])),
            ("x_labels", json!(["cpu", "mem", "io"])),
            (
                "matrix",
                json!([[1, 0.62, -0.2], [0.62, 1, "NA"], [-0.2, "NA", 1]]),
            ),
        ],
        "shown to the user",
        Some(1),
    );
    let hours: Vec<String> = (0..24).map(|h| format!("{h:02}")).collect();
    let activity: Vec<Vec<u32>> = (0..4)
        .map(|day| (0..24).map(|hour| (hour * (day + 1)) % 9).collect())
        .collect();
    ctx.tool_call(
        "plot",
        &[
            ("kind", json!("heatmap")),
            ("labels", json!(["Mon", "Tue", "Wed", "Thu"])),
            ("x_labels", json!(hours)),
            ("matrix", json!(activity)),
        ],
        "shown to the user",
        Some(1),
    );
    let weeks: Vec<String> = (1..=120).map(|w| format!("w{w}")).collect();
    let latency: Vec<Vec<i32>> = (0..5)
        .map(|host| (0..120).map(|t| (t * 7 + host * 13) % 40 - 10).collect())
        .collect();
    ctx.tool_call(
        "plot",
        &[
            ("kind", json!("heatmap")),
            ("labels", json!(["a", "b", "c", "d", "e"])),
            ("x_labels", json!(weeks)),
            ("matrix", json!(latency)),
        ],
        "shown to the user",
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(plot_percent_composition, |ctx| {
    ctx.set_viewport(54, 22);
    ctx.run_lua("require('smelt.plugins.plot')");
    for kind in ["area", "bar"] {
        ctx.tool_call(
            "plot",
            &[
                ("kind", json!(kind)),
                ("percent", json!(true)),
                ("height", json!(5)),
                ("labels", json!(["Jan", "Feb", "Mar"])),
                (
                    "series",
                    json!([{ "name": "code", "y": [1, 0, 9] }, { "name": "test", "y": [3, 0, 1] }]),
                ),
            ],
            "shown to the user",
            Some(1),
        );
    }
    ctx.assert_snapshot();
});

app_story!(plot_scatter_shows_unconnected_points, |ctx| {
    ctx.set_viewport(52, 19);
    ctx.run_lua("require('smelt.plugins.plot')");
    ctx.tool_call(
        "plot",
        &[
            ("kind", json!("scatter")),
            (
                "series",
                json!([
                    { "name": "control", "x": [0, 2, 9], "y": [2, 8, 4] },
                    { "name": "trial", "x": [1, 2, 7], "y": [6, 2, 8] },
                ]),
            ),
            ("height", json!(7)),
        ],
        "shown to the user",
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(plot_line_bar_and_histogram, |ctx| {
    ctx.set_viewport(56, 46);
    ctx.run_lua("require('smelt.plugins.plot')");
    ctx.tool_call(
        "plot",
        &[
            ("kind", json!("line")),
            (
                "series",
                json!([
                    { "name": "p50", "y": [12, 14, 13, 18, 22, 19, 25, 24] },
                    { "name": "p99", "y": [40, 42, 55, 48, 70, 66, 90, 84] },
                ]),
            ),
            ("y_label", json!("latency ms")),
            ("height", json!(6)),
        ],
        "shown to the user",
        Some(1),
    );
    ctx.tool_call(
        "plot",
        &[
            ("kind", json!("bar")),
            ("series", json!([])),
            ("labels", json!(["rust", "lua", "markdown", "toml"])),
            ("values", json!([182_340, 24_120, 9_870, 1_204])),
        ],
        "shown to the user",
        Some(1),
    );
    ctx.tool_call(
        "plot",
        &[
            ("kind", json!("histogram")),
            (
                "values",
                json!([1, 2, 2, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 6, 6, 7, 9]),
            ),
            ("bins", json!(8)),
            ("height", json!(5)),
        ],
        "shown to the user",
        Some(1),
    );
    ctx.assert_snapshot();
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
    ctx.tool_call_with_display_content(
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
        json!({
            "plan_path": "/tmp/smelt/sessions/sess/plans/20260101-000000-parser-refactor/plan.md"
        }),
        &[(
            "plan",
            "# Goal\nRefactor parser state.\n\n```rust\nfn parse(input: &str) -> Ast {\n    todo!()\n}\n```\n\n# Verification\nRun parser tests.",
        )],
        Some(9),
    );
    ctx.assert_snapshot_named("expanded");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(web_tool_states, |ctx| {
    ctx.set_viewport(76, 18);
    ctx.run_lua("smelt.settings.transcript.view.groups = { web = false }");
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

app_story!(tool_elapsed_timer_preserves_decimal_padding, |ctx| {
    ctx.set_viewport(72, 12);
    ctx.run_lua("smelt.settings.transcript.view.groups = { explore = false }");
    ctx.tool_started("read_file", &[("file_path", json!("timer.rs"))]);

    ctx.advance_time(1000);
    ctx.assert_snapshot_named("one_second");

    ctx.advance_time(200);
    ctx.assert_snapshot_named("fractional_second");

    ctx.advance_time(7800);
    ctx.assert_snapshot_named("nine_seconds");
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

app_story!(mcp_tool_args_wrap_in_header, |ctx| {
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
        "outline",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("kind", json!("function")),
            ("name_contains", json!("request")),
            ("max_depth", json!(2)),
        ],
        "2 symbols\n- fn request 70:1-92:2\n- fn request_symbol 94:1-126:2",
        json!({ "display_count": { "value": 2, "unit": "symbol" } }),
        Some(4),
    );
    ctx.tool_call_with_metadata(
        "language_server_status",
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
        "find_definition",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("line", json!(111)),
            ("column", json!(12)),
        ],
        "1 definition\n- crates/core/src/lsp.rs:71:12",
        json!({ "display_count": { "value": 1, "unit": "definition" } }),
        Some(9),
    );
    ctx.tool_call_with_metadata(
        "find_references",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("line", json!(111)),
            ("column", json!(12)),
            ("include_declaration", json!(false)),
        ],
        "1 reference\n- crates/core/src/lua/api/lsp.rs:44:23",
        json!({ "display_count": { "value": 1, "unit": "reference" } }),
        Some(27),
    );
    ctx.tool_call_with_metadata(
        "diagnostics",
        &[("file_path", json!("crates/core/src/lsp.rs"))],
        "1 diagnostic\n- warning crates/core/src/lsp.rs:211:9 - unused variable: settings",
        json!({ "display_count": { "value": 1, "unit": "diagnostic" } }),
        Some(6),
    );
    ctx.tool_call_with_metadata(
        "preview_rename",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("line", json!(111)),
            ("column", json!(12)),
            ("new_name", json!("manager")),
        ],
        "preview across 1 file\n- crates/core/src/lsp.rs (1 edit)",
        json!({}),
        Some(15),
    );
    ctx.tool_call_with_metadata(
        "rename_symbol",
        &[
            ("file_path", json!("crates/core/src/lsp.rs")),
            ("line", json!(111)),
            ("column", json!(12)),
            ("new_name", json!("manager")),
        ],
        "applied across 1 file\n- crates/core/src/lsp.rs (1 edit)",
        json!({}),
        Some(21),
    );
    ctx.assert_snapshot_named("defaults");
    ctx.run_lua("smelt.transcript.fold_all('close')");
    ctx.assert_snapshot_named("collapsed");
});

app_story!(background_calls_show_their_work_state, |ctx| {
    ctx.set_viewport(82, 24);
    ctx.tool_call_with_metadata(
        "bash",
        &[
            ("command", json!("cargo build")),
            ("background", json!(true)),
        ],
        "started background process proc_1, you'll be notified when it completes",
        json!({"background": true, "background_id": "proc_1"}),
        Some(20),
    );
    ctx.tool_call_with_metadata(
        "bash",
        &[
            ("command", json!("cargo test")),
            ("background", json!(true)),
        ],
        "started background process proc_2, you'll be notified when it completes",
        json!({"background": true, "background_id": "proc_2"}),
        Some(20),
    );
    ctx.tool_call_with_metadata(
        "claude-code/Agent",
        &[
            ("description", json!("Review diff")),
            ("prompt", json!("Review")),
            ("run_in_background", json!(true)),
        ],
        "Async agent launched successfully.",
        json!({"background": true, "background_id": "claude-code:a1"}),
        Some(30),
    );
    ctx.tool_call_with_metadata(
        "claude-code/Agent",
        &[
            ("description", json!("Fix bug")),
            ("prompt", json!("Fix")),
            ("run_in_background", json!(true)),
        ],
        "Async agent launched successfully.",
        json!({"background": true, "background_id": "claude-code:a2"}),
        Some(30),
    );
    ctx.tool_call_with_metadata(
        "claude-code/Monitor",
        &[
            ("command", json!("tail -f log")),
            ("description", json!("watch log")),
        ],
        "Monitor started (task m1). You will be notified on each event.",
        json!({"background": true, "background_id": "claude-code:m1"}),
        Some(30),
    );
    ctx.tool_started(
        "bash",
        &[
            ("command", json!("npm run dev")),
            ("background", json!(true)),
        ],
    );
    ctx.advance_time(4_200);
    ctx.push_background_process_completed("proc_2", Some(1), "1 test failed");
    ctx.advance_time(30_000);
    ctx.push_background_agent_ended(
        "claude-code:a1",
        "Review diff",
        protocol::AgentStatus::Completed,
    );
    ctx.advance_time(38_000);
    ctx.push_background_process_ended(
        "claude-code:m1",
        None,
        protocol::JobTermination::Stopped,
        "",
    );
    ctx.advance_time(61_000);
    ctx.assert_snapshot();
});

app_story!(native_task_tools, |ctx| {
    ctx.set_viewport(78, 28);
    ctx.tool_call(
        "claude-code/TodoWrite",
        &[(
            "todos",
            json!([
                {"content": "Read the parser", "status": "completed", "activeForm": "Reading the parser"},
                {"content": "Fix the off-by-one", "status": "in_progress", "activeForm": "Fixing the off-by-one"},
                {"content": "Add a regression test", "status": "pending", "activeForm": "Adding a regression test"},
            ]),
        )],
        "Todos have been modified successfully.",
        Some(4),
    );
    ctx.tool_call_with_metadata(
        "claude-code/TaskCreate",
        &[
            ("subject", json!("Profile the render loop")),
            (
                "description",
                json!("Measure **frame time** with the release-fast build."),
            ),
        ],
        "Task #3 created successfully: Profile the render loop",
        json!({"native": {"task": {"id": "3", "subject": "Profile the render loop"}}}),
        Some(3),
    );
    ctx.tool_call(
        "claude-code/TaskUpdate",
        &[("taskId", json!("3")), ("status", json!("in_progress"))],
        "Updated task #3 status",
        Some(2),
    );
    ctx.tool_call(
        "claude-code/TaskUpdate",
        &[
            ("taskId", json!("1")),
            ("status", json!("completed")),
            ("subject", json!("Read the parser")),
        ],
        "Updated task #1 subject, status",
        Some(2),
    );
    ctx.tool_call_error(
        "claude-code/TaskGet",
        &[("taskId", json!("9"))],
        "Task not found",
        Some(1),
    );
    ctx.tool_call_with_metadata(
        "claude-code/TaskList",
        &[],
        "#1 [completed] Read the parser\n#3 [in_progress] Profile the render loop\n#4 [pending] Ship it",
        json!({"native": {"tasks": [
            {"id": "1", "subject": "Read the parser", "status": "completed", "blocks": [], "blockedBy": []},
            {"id": "3", "subject": "Profile the render loop", "status": "in_progress", "owner": "perf", "blocks": ["4"], "blockedBy": []},
            {"id": "4", "subject": "Ship it", "status": "pending", "blocks": [], "blockedBy": ["3"]},
        ]}}),
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(native_schedule_tools, |ctx| {
    ctx.set_viewport(78, 24);
    ctx.tool_call(
        "claude-code/ScheduleWakeup",
        &[
            ("delaySeconds", json!(1200)),
            ("reason", json!("waiting for CI")),
            ("prompt", json!("Check whether CI passed and report.")),
        ],
        "Wakeup scheduled.",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/ScheduleWakeup",
        &[
            ("delaySeconds", json!(90)),
            ("reason", json!("idle tick")),
            ("prompt", json!("<<autonomous-loop-dynamic>>")),
        ],
        "Wakeup scheduled.",
        Some(1),
    );
    ctx.tool_call_with_metadata(
        "claude-code/CronCreate",
        &[
            ("cron", json!("7 9 * * 1-5")),
            ("prompt", json!("Summarize open PRs.")),
        ],
        "Scheduled recurring job c1a2. Auto-expires after 7 days.",
        json!({"native": {"id": "c1a2", "humanSchedule": "Weekdays at 9:07 AM", "recurring": true, "durable": false}}),
        Some(1),
    );
    // Claude Code repeats an expression it cannot phrase as its schedule.
    ctx.tool_call_with_metadata(
        "claude-code/CronCreate",
        &[
            ("cron", json!("30 14 25 11 *")),
            ("prompt", json!("Check the deploy.")),
            ("recurring", json!(false)),
        ],
        "Scheduled one-shot task c9f0.",
        json!({"native": {"id": "c9f0", "humanSchedule": "30 14 25 11 *", "recurring": false, "durable": false}}),
        Some(1),
    );
    ctx.tool_call(
        "claude-code/ScheduleWakeup",
        &[("stop", json!(true))],
        "Loop stopped.",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/CronDelete",
        &[("id", json!("c1a2"))],
        "Cancelled job c1a2",
        Some(1),
    );
    ctx.tool_call_with_metadata(
        "claude-code/CronList",
        &[],
        "c1a2  7 9 * * 1-5  Summarize open PRs.\nc9f0  30 14 25 11 *  once  Check the deploy.",
        json!({"native": {"jobs": [
            {"id": "c1a2", "cron": "7 9 * * 1-5", "humanSchedule": "Weekdays at 9:07 AM", "prompt": "Summarize open PRs.", "recurring": true, "durable": false},
            {"id": "c9f0", "cron": "30 14 25 11 *", "humanSchedule": "30 14 25 11 *", "prompt": "Check the deploy.\nThen report.", "durable": false},
        ]}}),
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(native_session_tools, |ctx| {
    ctx.set_viewport(78, 36);
    // A selection loads only the tools that exist.
    ctx.tool_call_with_metadata(
        "claude-code/ToolSearch",
        &[
            ("query", json!("select:CronCreate,CronDelete,TodoWrite")),
            ("max_results", json!(3)),
        ],
        "",
        json!({"native": {"matches": ["CronCreate", "CronDelete"], "query": "select:CronCreate,CronDelete,TodoWrite", "total_deferred_tools": 18}}),
        Some(1),
    );
    ctx.tool_call_with_metadata(
        "claude-code/ToolSearch",
        &[
            ("query", json!("notebook jupyter")),
            ("max_results", json!(5)),
        ],
        "",
        json!({"native": {"matches": ["NotebookEdit"], "query": "notebook jupyter", "total_deferred_tools": 18}}),
        Some(1),
    );
    ctx.tool_call(
        "claude-code/Skill",
        &[
            ("skill", json!("review-working")),
            ("args", json!("--strict")),
        ],
        "Launching skill: review-working",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/LSP",
        &[
            ("operation", json!("goToDefinition")),
            ("filePath", json!("src/parser.rs")),
            ("line", json!(42)),
            ("character", json!(9)),
        ],
        "Defined in src/lexer.rs:17:5",
        Some(2),
    );
    ctx.tool_call(
        "claude-code/LSP",
        &[
            ("operation", json!("workspaceSymbol")),
            ("filePath", json!("src/parser.rs")),
            ("line", json!(1)),
            ("character", json!(1)),
            ("query", json!("Token")),
        ],
        "Token (struct) src/lexer.rs:4",
        Some(2),
    );
    ctx.tool_call(
        "claude-code/TaskOutput",
        &[("task_id", json!("b7x2k9"))],
        "Compiling smelt v0.9.0\nFinished dev profile",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/TaskStop",
        &[("task_id", json!("b7x2k9"))],
        "Stopped b7x2k9",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/ReadMcpResourceTool",
        &[
            ("server", json!("docs")),
            ("uri", json!("docs://guide/setup")),
        ],
        "Install with cargo.",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/ListMcpResourcesTool",
        &[],
        "docs://guide/setup",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/EnterPlanMode",
        &[],
        "Entered plan mode.",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/PushNotification",
        &[("message", json!("build failed: 2 auth tests"))],
        "Notification sent.",
        Some(1),
    );
    ctx.assert_snapshot();
});

app_story!(native_project_tools, |ctx| {
    ctx.set_viewport(78, 36);
    ctx.tool_call(
        "claude-code/Workflow",
        &[(
            "script",
            json!("export const meta = { name: 'review-changes', description: 'Review each changed file' }"),
        )],
        "Workflow started.",
        Some(2),
    );
    ctx.tool_call(
        "claude-code/Workflow",
        &[("scriptPath", json!("/tmp/session/workflows/review.js"))],
        "Workflow started.",
        Some(2),
    );
    ctx.tool_call(
        "claude-code/Monitor",
        &[
            ("ws", json!({"url": "wss://ci.example.com/events"})),
            ("description", json!("CI events")),
            ("timeout_ms", json!(600000)),
        ],
        "Monitor started (task m2).",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/ExitWorktree",
        &[
            ("action", json!("remove")),
            ("discard_changes", json!(true)),
        ],
        "Removed the worktree.",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/SendToProject",
        &[
            ("project_name", json!("smelt-docs")),
            (
                "proposal",
                json!({"summary": "Document the new **native tool** renderers."}),
            ),
        ],
        "Sent.",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/PowerShell",
        &[(
            "command",
            json!("Get-ChildItem -Recurse src\nWrite-Host done"),
        )],
        "src/main.rs",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/ReportFindings",
        &[
            ("level", json!("high")),
            (
                "findings",
                json!([
                    {"file": "src/parser.rs", "line": 42, "summary": "Off-by-one at the end of input.", "short_summary": "off-by-one at end of input", "failure_scenario": "x"},
                    {"file": "src/lexer.rs", "summary": "Token spans skip the BOM.", "failure_scenario": "y"},
                ]),
            ),
        ],
        "Reported.",
        Some(1),
    );
    ctx.tool_call(
        "claude-code/SomeFutureTool",
        &[("target", json!("staging")), ("dry_run", json!(true))],
        "ok",
        Some(1),
    );
    ctx.assert_snapshot();
});
