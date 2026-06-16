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

app_story!(prompt_slash_completion_shrinks_for_short_terminal, |ctx| {
    // When the terminal is very short the prompt-docked picker must
    // shrink so it never overlaps the prompt input.
    ctx.set_viewport(50, 8);
    ctx.type_prompt("/");
    ctx.assert_snapshot();
});

app_story!(prompt_slash_completion_two_rows_when_cramped, |ctx| {
    // Extreme height: the picker collapses to just a couple of rows
    // rather than painting over the prompt chrome.
    ctx.set_viewport(50, 6);
    ctx.type_prompt("/");
    ctx.assert_snapshot();
});

app_story!(prompt_modal_picker_hides_tip, |ctx| {
    // Secondary prompt-docked pickers (for slash-command follow-ups like
    // /model, file finders, and reverse history search) own the prompt via
    // smelt.prompt.acquire(). The discovery tip must stay hidden while that
    // modal picker is open even though the prompt text is empty.
    ctx.set_viewport(50, 10);
    ctx.run_lua(
        r#"
      smelt.cmd.picker("tipless", {
        items = {
          { label = "alpha", description = "first" },
          { label = "beta", description = "second" },
        },
      })
    "#,
    );
    ctx.run_command("/tipless");
    ctx.assert_snapshot();
});

app_story!(prompt_shell_escape_prefix, |ctx| {
    // `!cmd` switches the prompt into shell-escape rendering: accent
    // `!` prefix, panel chrome shared with the exec block.
    ctx.set_viewport(50, 8);
    ctx.type_prompt("!ls -la");
    ctx.assert_snapshot();
});

// ── Prompt top-bar chrome (queued + stash) ────────────────────────

app_story!(prompt_stash_row, |ctx| {
    // Ctrl+S stashes the current buffer. The top bar grows by one row
    // with the `◌ Stashed (ctrl+s to unstash)` cue.
    ctx.set_viewport(50, 8);
    ctx.type_prompt("draft note");
    ctx.stash_prompt();
    ctx.assert_snapshot();
});

app_story!(prompt_queued_messages, |ctx| {
    // While a turn is active, Enter-on-prompt queues the message for the
    // next turn. Empty Enter promotes the oldest item into the next-request
    // queue, giving request and turn rows different markers.
    ctx.set_viewport(50, 10);
    ctx.push_queued_message("first follow-up");
    ctx.push_queued_message("second follow-up");
    ctx.promote_next_queued_message();
    ctx.assert_snapshot();
});

app_story!(
    prompt_queued_messages_collapse_when_tall_queue_short_terminal,
    |ctx| {
        // A large queue on a short terminal must collapse oldest messages
        // into a "+N more" row so the transcript keeps at least two rows.
        ctx.set_viewport(50, 10);
        for i in 1..=12 {
            ctx.push_queued_message(&format!("follow-up {}", i));
        }
        ctx.assert_snapshot();
    }
);

app_story!(
    prompt_queued_messages_with_stash_collapse_on_short_terminal,
    |ctx| {
        // Stash plus a tall queue on a short terminal: the stash row must
        // stay visible and the queue still collapses to "+N more".
        ctx.set_viewport(50, 10);
        ctx.type_prompt("draft note");
        ctx.stash_prompt();
        for i in 1..=10 {
            ctx.push_queued_message(&format!("follow-up {}", i));
        }
        ctx.assert_snapshot();
    }
);

app_story!(
    prompt_slash_completion_with_stash_shrinks_for_short_terminal,
    |ctx| {
        // A prompt-docked picker must still avoid overlapping the prompt
        // when the top bar has grown by an extra stash row.
        ctx.set_viewport(50, 8);
        ctx.type_prompt("draft note");
        ctx.stash_prompt();
        ctx.type_prompt("/");
        ctx.assert_snapshot();
    }
);

app_story!(
    prompt_queued_messages_with_notification_collapse_on_short_terminal,
    |ctx| {
        // A notification hides the tip and reserves its own row above the
        // prompt; the queue still collapses and the stash/indicator rows
        // stay visible on a short terminal.
        ctx.set_viewport(50, 10);
        ctx.notify("this is a notification", None);
        for i in 1..=10 {
            ctx.push_queued_message(&format!("follow-up {}", i));
        }
        ctx.assert_snapshot();
    }
);

app_story!(
    prompt_slash_completion_with_notification_shrinks_for_short_terminal,
    |ctx| {
        // A prompt-docked picker must still avoid overlapping the prompt
        // when a notification is taking a row above the top bar.
        ctx.set_viewport(50, 8);
        ctx.notify("this is a notification", None);
        ctx.type_prompt("/");
        ctx.assert_snapshot();
    }
);

app_story!(prompt_turn_queue_row_truncates, |ctx| {
    // Long next-turn queue entries stay on one prompt-above row, truncate with
    // a Unicode ellipsis, and leave right padding so they do not hit the edge.
    ctx.set_viewport(32, 8);
    ctx.push_queued_message("turn queue message that should be clipped at the right edge");
    ctx.assert_snapshot();
});

app_story!(prompt_request_queue_row_truncates, |ctx| {
    // Promoted next-request entries use the `»` marker and share the same
    // truncation/padding behavior as next-turn entries.
    ctx.set_viewport(32, 8);
    ctx.push_queued_message("request queue message that should be clipped at the right edge");
    ctx.promote_next_queued_message();
    ctx.assert_snapshot();
});

app_story!(prompt_stash_row_truncates, |ctx| {
    // The stash reminder is fixed text, but on narrow terminals it still needs
    // ellipsis truncation and right padding like the queue rows.
    ctx.set_viewport(24, 8);
    ctx.type_prompt("draft note");
    ctx.stash_prompt();
    ctx.assert_snapshot();
});

app_story!(prompt_compacting_keeps_token_counter_visible, |ctx| {
    // A narrow prompt bar during compaction should keep the token counter
    // visible even when secondary chrome needs to drop.
    ctx.set_viewport(28, 8);
    ctx.set_context_window(Some(25_000));
    ctx.set_context_tokens(19_500);
    ctx.run_lua("_G._busy_handle = smelt.work.busy('compacting')");
    ctx.assert_snapshot();
});

app_story!(prompt_working_bar_width_ladder, |ctx| {
    // The top prompt bar degrades in stages as the viewport shrinks: full
    // label, compact spinner, then the token strip without orphan spacing.
    ctx.set_context_window(Some(25_000));
    ctx.set_context_tokens(19_500);
    ctx.run_lua("_G._busy_handle = smelt.work.busy('compacting')");

    for width in [48, 36, 28, 20, 16] {
        ctx.set_viewport(width, 8);
        ctx.assert_snapshot();
    }
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
