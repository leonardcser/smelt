//! Permission-confirm dialogs. `AppStoryCtx::request_permission`
//! calls the tool's real `summary(args)` Lua callback to populate the
//! `RequestPermission` event, then routes the event through
//! `dispatch_engine_event`. The app registers the confirm, fires
//! `smelt.confirm.open`, and renders the tool's `preview(args)`
//! layout (diff, file_view, notebook preview, …) through the live
//! tool-body IR preview pipeline.

use std::collections::HashMap;

use serde_json::json;

use crate::app_story;
use crate::storybook::app_ctx::AppStoryCtx;
use crate::storybook::args;

fn open_bash_permission_dialog(ctx: &mut AppStoryCtx) {
    ctx.request_permission(
        "bash",
        args([
            ("command", json!("ls -la /tmp/foo")),
            ("description", json!("List files in /tmp/foo")),
        ]),
        vec!["ls *".into()],
    );
}

app_story!(bash_permission_dialog, |ctx| {
    ctx.set_viewport(80, 22);
    open_bash_permission_dialog(ctx);
    ctx.assert_snapshot();
});

app_story!(bash_permission_dialog_expanded_max_height, |ctx| {
    ctx.set_viewport(80, 22);
    open_bash_permission_dialog(ctx);
    ctx.expand_active_dialog_to_max_height();
    ctx.assert_snapshot();
});

app_story!(bash_permission_reason_input_wraps, |ctx| {
    ctx.set_viewport(58, 22);
    open_bash_permission_dialog(ctx);
    ctx.press_tab();
    ctx.type_prompt(
        "Needed to inspect the temporary directory before deciding whether the generated fixtures can be reused.",
    );
    ctx.assert_snapshot();
});

app_story!(bash_permission_dialog_tall_terminal, |ctx| {
    ctx.set_viewport(80, 36);
    ctx.request_permission(
        "bash",
        args([
            ("command", json!("ls -la /tmp/foo")),
            ("description", json!("List files in /tmp/foo")),
        ]),
        vec!["ls *".into()],
    );
    ctx.assert_snapshot();
});

app_story!(bash_multiline_command_summary, |ctx| {
    ctx.set_viewport(80, 22);
    ctx.request_permission(
        "bash",
        args([(
            "command",
            json!("for f in *.rs; do\n  rustfmt \"$f\"\ndone"),
        )]),
        vec!["for *".into()],
    );
    ctx.assert_snapshot();
});

app_story!(bash_long_command_summary_wraps, |ctx| {
    ctx.set_viewport(70, 22);
    ctx.request_permission(
        "bash",
        args([(
            "command",
            json!("ls -ld /Users/leo/.dotfiles; realpath /Users/leo/.dotfiles 2>/dev/null || true; cd /Users/leo/dev/rust/smelt/.worktrees/fix-path-parsing && cargo test -p smelt-core permissions::tests"),
        )]),
        vec!["ls *".into()],
    );
    ctx.assert_snapshot();
});

app_story!(bash_outside_workspace_extra_options, |ctx| {
    ctx.set_viewport(80, 22);
    ctx.request_permission(
        "bash",
        args([("command", json!("cat /etc/hosts"))]),
        vec!["cat *".into(), "cat /etc/*".into()],
    );
    ctx.assert_snapshot();
});

app_story!(bash_permission_dialog_long_extra_options_wrap, |ctx| {
    ctx.set_viewport(70, 22);
    ctx.request_permission(
        "bash",
        args([(
            "command",
            json!(
                "cat /var/log/smelt/projects/alpha/beta/gamma/delta/epsilon/very-long-permission-target.log"
            ),
        )]),
        vec![
            "cat /var/log/smelt/projects/alpha/beta/gamma/delta/epsilon/very-long-permission-target.log"
                .into(),
            "cat /var/log/smelt/projects/alpha/beta/gamma/delta/epsilon/*".into(),
        ],
    );
    ctx.press_char('j');
    ctx.press_char('j');
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
        vec![],
    );
    ctx.assert_snapshot();
});

app_story!(edit_file_permission_dialog_with_diff, |ctx| {
    ctx.set_viewport(80, 24);
    ctx.request_permission(
        "edit_file",
        args([
            ("file_path", json!("src/main.rs")),
            ("old_string", json!("println!(\"hello, world\");")),
            ("new_string", json!("println!(\"hello, smelt\");")),
        ]),
        vec![],
    );
    ctx.assert_snapshot();
});

app_story!(edit_file_long_path_permission_dialog_wraps, |ctx| {
    ctx.set_viewport(70, 24);
    ctx.request_permission(
        "edit_file",
        args([
            (
                "file_path",
                json!("/tmp/smelt-storybook-nonexistent-session-93e4220e873c55398e6e22ff065d617e27efc1a9b326032e323d00a522764901/plans/20260618-083946-virtual-transcript-performance/plan.md"),
            ),
            ("old_string", json!("# Plan\n")),
            ("new_string", json!("# Updated plan\n")),
        ]),
        vec![],
    );
    ctx.assert_snapshot();
});

app_story!(edit_file_permission_dialog_multiline_diff, |ctx| {
    ctx.set_viewport(80, 28);
    ctx.request_permission(
        "edit_file",
        args([
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
        ]),
        vec![],
    );
    ctx.assert_snapshot();
});

app_story!(notebook_edit_permission_dialog_with_diff, |ctx| {
    ctx.set_viewport(80, 28);
    // notebook_edit's preview reads the source notebook from disk
    // via `smelt.notebook.preview_data`. Seed a 2-cell fixture so
    // the preview pipeline produces a real cell-level diff.
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
        vec![],
    );
    ctx.assert_snapshot();
});

app_story!(notebook_edit_permission_dialog_insert_cell, |ctx| {
    ctx.set_viewport(80, 22);
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
        vec![],
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
        vec!["https://example.com/*".into()],
    );
    ctx.assert_snapshot();
});

app_story!(web_search_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission(
        "web_search",
        args([("query", json!("rust terminal tui snapshots"))]),
        vec!["rust terminal *".into()],
    );
    ctx.assert_snapshot();
});

app_story!(grep_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission(
        "grep",
        args([
            ("pattern", json!("confirm_dialog")),
            ("path", json!("crates/tui/src")),
            ("output_mode", json!("content")),
        ]),
        vec!["confirm_*".into()],
    );
    ctx.assert_snapshot();
});

app_story!(glob_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission(
        "glob",
        args([
            ("pattern", json!("**/*.rs")),
            ("path", json!("crates/tui/tests")),
        ]),
        vec!["**/*.rs".into()],
    );
    ctx.assert_snapshot();
});

app_story!(read_file_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission(
        "read_file",
        args([
            ("file_path", json!("crates/tui/src/app.rs")),
            ("offset", json!(120)),
            ("limit", json!(40)),
        ]),
        vec![],
    );
    ctx.assert_snapshot();
});

app_story!(read_file_long_path_permission_dialog_wraps, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission(
        "read_file",
        args([
            (
                "file_path",
                json!("/tmp/smelt-storybook-nonexistent-session-93e4220e873c55398e6e22ff065d617e27efc1a9b326032e323d00a522764901/plans/20260618-083946-virtual-transcript-performance/plan.md"),
            ),
            ("offset", json!(1)),
            ("limit", json!(80)),
        ]),
        vec![],
    );
    ctx.assert_snapshot();
});

app_story!(read_process_output_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission("read_process_output", args([("id", json!("4242"))]), vec![]);
    ctx.assert_snapshot();
});

app_story!(stop_process_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission("stop_process", args([("id", json!("4242"))]), vec![]);
    ctx.assert_snapshot();
});

app_story!(load_skill_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission("load_skill", args([("name", json!("customize"))]), vec![]);
    ctx.assert_snapshot();
});

app_story!(smelt_reload_permission_dialog, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.request_permission("smelt_reload", HashMap::new(), vec![]);
    ctx.assert_snapshot();
});

app_story!(ask_user_question_permission_dialog, |ctx| {
    ctx.set_viewport(70, 24);
    ctx.request_permission(
        "ask_user_question",
        args([(
            "questions",
            json!([
                {
                    "header": "Approach",
                    "question": "Which implementation should I use?",
                    "multiSelect": false,
                    "options": [
                        { "label": "Minimal", "description": "Small targeted change." },
                        { "label": "Rewrite", "description": "Replace the component." }
                    ]
                }
            ]),
        )]),
        vec![],
    );
    ctx.assert_snapshot();
});
