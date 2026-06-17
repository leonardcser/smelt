//! Toast (`smelt.notify`) stories. Pin two invariants:
//!   1. A multi-line body collapses to its first line in the toast row
//!      (the full body still lives in `smelt.messages`).
//!   2. The toast row never exceeds terminal width; long summaries get
//!      ellipsis-clipped instead of wrapping onto the prompt-above row.

use crate::app_story;

app_story!(notify_error_clips_long_summary, |ctx| {
    // Narrow viewport so even a single-line body has to be clipped.
    // The original bug: long bodies + multi-line tracebacks wrapped into
    // the prompt-above row above the toast.
    ctx.set_viewport(40, 6);
    ctx.run_lua(
        r#"
        smelt.notify.error(
          "/upgrade: cargo install exited 101\nerror: multiple packages with binaries\nfollow-up traceback line",
          "upgrade"
        )
        "#,
    );
    ctx.assert_snapshot();
});

app_story!(notify_info_short_body_renders_verbatim, |ctx| {
    // Baseline: a short single-line info toast renders the body as-is
    // (no ellipsis, no clipping). Catches accidental over-clipping if
    // someone tightens the width budget.
    ctx.set_viewport(40, 6);
    ctx.run_lua(r#"smelt.notify("checking for upgrades…")"#);
    ctx.assert_snapshot();
});

app_story!(notify_refits_after_resize, |ctx| {
    ctx.set_viewport(32, 6);
    ctx.run_lua(
        r#"smelt.notify("update available: v0.6.0 with detailed release notes", "upgrade")"#,
    );
    ctx.assert_snapshot();

    ctx.set_viewport(72, 6);
    ctx.assert_snapshot();
});

app_story!(notify_logs_to_messages_with_source, |ctx| {
    // The toast's full body lands in `smelt.messages` tagged with the
    // caller-provided source. Pins the contract that `/messages` is the
    // audit trail for every toast, including the parts that got clipped
    // out of the toast row.
    ctx.set_viewport(70, 14);
    ctx.run_lua(
        r#"
        smelt.notify.error(
          "/upgrade: cargo install exited 101\nerror: multiple packages with binaries",
          "upgrade"
        )
        smelt.notify("update available: v0.6.0", "upgrade")
        "#,
    );
    ctx.run_command("messages");
    ctx.assert_snapshot();
});
