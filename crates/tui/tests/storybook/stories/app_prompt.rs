//! Prompt-input and shell-escape surfaces. The prompt is the chrome
//! users actually type into; `Block::Exec` is the resulting transcript
//! block when `!cmd` runs. Both share rendering primitives, so they
//! live together.

use crate::app_story;

// ── Prompt input variants ─────────────────────────────────────────

app_story!(prompt_typing_renders_into_input, |ctx| {
    ctx.set_viewport(40, 8);
    ctx.type_prompt("hello prompt");
    ctx.assert_snapshot();
});

app_story!(prompt_multiline_input, |ctx| {
    // Multi-line prompt: pin the chrome that auto-grows when the
    // buffer wraps onto extra rows (different from the single-line
    // chrome captured above).
    ctx.set_viewport(50, 10);
    ctx.type_prompt("first line\nsecond line\nthird line");
    ctx.assert_snapshot();
});

app_story!(prompt_slash_command_completion, |ctx| {
    // Typing `/` triggers the slash-command completion popup over
    // the prompt. Pins the popup layout (registered commands in
    // descending recency, dim descriptions, accent on `/`).
    ctx.set_viewport(50, 14);
    ctx.type_prompt("/");
    ctx.assert_snapshot();
});

app_story!(prompt_shell_escape_prefix, |ctx| {
    // `!cmd` switches the prompt into shell-escape rendering: accent
    // `!` prefix, panel chrome shared with the exec block.
    ctx.set_viewport(50, 8);
    ctx.type_prompt("!ls -la");
    ctx.assert_snapshot();
});

// ── Exec block (`!cmd` shell escape) ──────────────────────────────

app_story!(exec_command_block_with_output, |ctx| {
    // `!ls` drives the real `start_exec` → `append_exec_output` →
    // `finish_exec` → `finalize_exec` lifecycle. The snapshot
    // captures the `!` accent prefix, panel chrome shared with user
    // blocks, and the wrapped output region.
    ctx.set_viewport(50, 12);
    ctx.exec_with_output("ls -1 src", "lib.rs\nmain.rs\nparser.rs", Some(0));
    ctx.assert_snapshot();
});
