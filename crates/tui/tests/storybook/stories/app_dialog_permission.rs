//! Permission-confirm dialogs. `AppStoryCtx::request_permission`
//! calls the tool's real `summary(args)` Lua callback to populate the
//! `RequestPermission` event, then routes the event through
//! `dispatch_engine_event`. The app registers the confirm, fires
//! `smelt.confirm.open`, and renders the tool's `preview(args)`
//! layout (diff, file_view, notebook preview, …) through the live
//! `extract_rendered_layout` + `render_layout_into` pipeline - the
//! same one production transcript blocks use.

use serde_json::json;

use crate::app_story;
use crate::storybook::args;

app_story!(bash_permission_dialog, |ctx| {
    ctx.set_viewport(80, 22);
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
        args([("url", json!("https://example.com/page"))]),
        vec!["https://example.com/*".into()],
    );
    ctx.assert_snapshot();
});
