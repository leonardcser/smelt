//! Per-tool transcript rendering. Each story drives the real
//! `ToolStarted` → `ToolFinished` pipeline for a tool registered
//! under `runtime/lua/smelt/tools/`. The root transcript renderer
//! asks the bundled Lua defaults for the full tool block, including
//! any tool-adjacent structured body renderer (diff, file_view, "N
//! lines", etc.), so the snapshot reflects the production block
//! exactly as users see it. The `ctx.tool_call*` helpers hide the
//! args/call-id/outcome boilerplate so each story stays focused on
//! the inputs that drive the render.

use serde_json::json;

use crate::app_story;

// ── Generic tool call shape ───────────────────────────────────────

app_story!(tool_call_then_result_pair, |ctx| {
    ctx.set_viewport(60, 14);
    ctx.tool_call(
        "read_file",
        &[("path", json!("src/main.rs"))],
        "fn main() {}",
        Some(12),
    );
    ctx.assert_snapshot();
});

// ── edit_file ─────────────────────────────────────────────────────

app_story!(edit_file_tool_block_with_diff_gutter, |ctx| {
    // The dialog stories cover the gutterless preview body. In the
    // *transcript* the same `edit_file` tool renders with the
    // tool-block gutter chrome (2-cell indent, attached header).
    // Driving the real `ToolStarted` + `ToolFinished` events makes
    // the worker run the tool's `preview(args)` Lua callback, which
    // returns a `Diff` leaf - the snapshot then captures the buffer-
    // backed inline diff with line numbers, +/- markers, and
    // syntax-highlighted content all sharing one render pass.
    ctx.set_viewport(80, 16);
    ctx.tool_call(
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
        Some(7),
    );
    ctx.assert_snapshot();
});

app_story!(edit_file_tool_block_error, |ctx| {
    // `edit_file.render` falls back to `layout.text(content, is_error)`
    // when the result is an error - same fallback every tool with a
    // structured render uses. Pins the error chrome.
    ctx.set_viewport(70, 10);
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
    ctx.assert_snapshot();
});

// ── read_file ─────────────────────────────────────────────────────

app_story!(read_file_tool_block, |ctx| {
    ctx.set_viewport(60, 10);
    ctx.tool_call(
        "read_file",
        &[("file_path", json!("src/main.rs"))],
        "fn main() {\n    println!(\"hi\");\n}\n",
        Some(3),
    );
    ctx.assert_snapshot();
});

app_story!(read_file_tool_block_error, |ctx| {
    // `read_file.render` falls back to `layout.text(content, is_error)`
    // when the result carries `is_error`. Pins the "file not found"
    // chrome users actually see.
    ctx.set_viewport(60, 8);
    ctx.tool_call_error(
        "read_file",
        &[("file_path", json!("missing.rs"))],
        "could not read file: missing.rs",
        Some(2),
    );
    ctx.assert_snapshot();
});

// ── write_file ────────────────────────────────────────────────────

app_story!(write_file_tool_block, |ctx| {
    // `write_file.render` returns a `layout.file_view`, so the transcript
    // body shows the freshly-written contents with line numbers and
    // syntax highlighting (same chrome as the dialog preview).
    ctx.set_viewport(70, 14);
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
    ctx.assert_snapshot();
});

app_story!(write_file_tool_block_error, |ctx| {
    // Error path: `write_file.render` skips `file_view` and renders the
    // error message via `layout.text(content, {is_error=true})`.
    ctx.set_viewport(60, 8);
    ctx.tool_call_error(
        "write_file",
        &[
            ("file_path", json!("/root/forbidden.rs")),
            ("content", json!("ignored\n")),
        ],
        "permission denied: /root/forbidden.rs",
        Some(1),
    );
    ctx.assert_snapshot();
});

// ── bash ──────────────────────────────────────────────────────────

app_story!(bash_tool_block_with_output, |ctx| {
    // `bash.render` wraps the captured output in a vbox; success path
    // (no `is_error`) means no error chrome, just the plain text.
    ctx.set_viewport(60, 12);
    ctx.tool_call(
        "bash",
        &[
            ("command", json!("git status --short")),
            ("description", json!("Check working tree status")),
        ],
        " M src/lib.rs\n?? src/new.rs",
        Some(42),
    );
    ctx.assert_snapshot();
});

app_story!(bash_tool_block_error_output, |ctx| {
    // Error path: `is_error = true` flips `layout.text` into the error
    // style so the body renders in the error fg group.
    ctx.set_viewport(60, 10);
    ctx.tool_call_error(
        "bash",
        &[
            ("command", json!("cat missing.txt")),
            ("description", json!("Read a missing file")),
        ],
        "cat: missing.txt: No such file or directory",
        Some(7),
    );
    ctx.assert_snapshot();
});

// ── glob ──────────────────────────────────────────────────────────

app_story!(glob_tool_block, |ctx| {
    // `glob.render` returns "N files" - the header carries the pattern
    // via `summary(args)` (`pattern` + optional `path`).
    ctx.set_viewport(60, 10);
    ctx.tool_call(
        "glob",
        &[
            ("pattern", json!("**/*.rs")),
            ("path", json!("crates/term")),
        ],
        "crates/term/src/grid.rs\ncrates/term/src/lib.rs\ncrates/term/src/snapshot.rs",
        Some(4),
    );
    ctx.assert_snapshot();
});

app_story!(glob_tool_block_error, |ctx| {
    // Error path: skip the "N files" summary and render the error
    // message via the shared `is_error` guard.
    ctx.set_viewport(60, 8);
    ctx.tool_call_error(
        "glob",
        &[("pattern", json!("[invalid"))],
        "invalid glob pattern: missing ]",
        Some(1),
    );
    ctx.assert_snapshot();
});

// ── grep ──────────────────────────────────────────────────────────

app_story!(grep_tool_block, |ctx| {
    // `grep.render` returns "N matches" - the header carries the
    // pattern + path via `summary(args)`.
    ctx.set_viewport(70, 10);
    ctx.tool_call(
        "grep",
        &[
            ("pattern", json!("fn render")),
            ("path", json!("crates/tui/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/tui/src/render/markdown.rs\ncrates/tui/src/render/blocks/tool.rs",
        Some(11),
    );
    ctx.assert_snapshot();
});

app_story!(grep_tool_block_error, |ctx| {
    // Error path: ripgrep returns non-zero with a stderr message.
    ctx.set_viewport(70, 8);
    ctx.tool_call_error(
        "grep",
        &[
            ("pattern", json!("(?P<bad")),
            ("path", json!("crates/tui/src")),
        ],
        "regex parse error: unterminated group",
        Some(3),
    );
    ctx.assert_snapshot();
});

// ── Wrapping regressions ───────────────────────────────────────────

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
    ctx.tool_call(
        "glob",
        &[
            ("pattern", json!("crates/tui/tests/storybook/snapshots/app_tool::*.snap")),
            ("path", json!("/Users/leo/dev/rust/smelt/.worktrees/transcript-layout-rewrite")),
        ],
        "crates/tui/tests/storybook/snapshots/app_tool::grep_tool_block.snap\ncrates/tui/tests/storybook/snapshots/app_tool::glob_tool_block.snap",
        Some(34),
    );
    ctx.tool_call(
        "grep",
        &[
            ("pattern", json!("wrap_fragments_with_widths|continuation_indent|ToolSummaryResolver")),
            ("path", json!("crates/tui/src crates/buffer/src")),
            ("output_mode", json!("files_with_matches")),
        ],
        "crates/buffer/src/inline_line.rs\ncrates/tui/src/app/history.rs\ncrates/tui/src/content/display_renderers/layout_ir.rs",
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

// ── notebook_edit ─────────────────────────────────────────────────

app_story!(notebook_edit_tool_block, |ctx| {
    // `notebook_edit.render` looks at `output.metadata` to decide
    // between `layout.diff` (replace) and `layout.file_view` (insert).
    // This story drives the replace path so the transcript shows the
    // same buffer-backed diff chrome as `edit_file`, but rooted at a
    // cell rather than a file.
    ctx.set_viewport(80, 16);
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
    ctx.assert_snapshot();
});

app_story!(notebook_edit_tool_block_insert_cell, |ctx| {
    // Insert path: `edit_mode = "insert"` switches `render` to
    // `layout.file_view` so the snapshot pins the gutter chrome shared
    // with `write_file` (line numbers + syntax highlighting) keyed off
    // the synthesised `.py` path.
    ctx.set_viewport(70, 12);
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
    ctx.assert_snapshot();
});

// ── web_fetch / web_search ────────────────────────────────────────

app_story!(web_fetch_tool_block, |ctx| {
    // `web_fetch.render` prints the prompt above the result text. The
    // header summary is the URL + prompt via the tool's `summary` cb.
    ctx.set_viewport(70, 12);
    ctx.tool_call(
        "web_fetch",
        &[
            ("url", json!("https://example.com/release")),
            ("prompt", json!("Summarise the release notes")),
        ],
        "0.6 ships streaming markdown and removes the legacy renderer.",
        Some(820),
    );
    ctx.assert_snapshot();
});

app_story!(web_search_tool_block, |ctx| {
    // `web_search.render` is a single `layout.text`; the header shows
    // the query.
    ctx.set_viewport(70, 10);
    ctx.tool_call(
        "web_search",
        &[("query", json!("rust unicode width crate"))],
        "1. unicode-width - crates.io\n2. unicode-segmentation - crates.io",
        Some(330),
    );
    ctx.assert_snapshot();
});

// ── Tool block states beyond the happy path ───────────────────────

app_story!(tool_block_pending_no_result, |ctx| {
    // `ToolStarted` without a matching `ToolFinished` - the spinner /
    // running chrome users see while a tool is in flight.
    ctx.set_viewport(60, 8);
    ctx.tool_started("bash", &[("command", json!("sleep 5"))]);
    ctx.assert_snapshot();
});

app_story!(tool_block_parallel_pending, |ctx| {
    // Two `ToolStarted` events before any `ToolFinished` - the
    // claude-code shape where the agent fires multiple tools at once.
    // Pins the block ordering and the per-block pending chrome.
    ctx.set_viewport(60, 12);
    ctx.tool_started("read_file", &[("file_path", json!("a.rs"))]);
    ctx.tool_started("read_file", &[("file_path", json!("b.rs"))]);
    ctx.assert_snapshot();
});
