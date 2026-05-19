//! Conversation-flow message blocks: user, assistant text/thinking,
//! streaming deltas, turn errors, compacted summaries, plus the
//! full-turn composite. Every story drives the production event path
//! (`push_user_message` or an `EngineEvent`).

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;

// ── Assistant output: Text / Thinking / streaming ─────────────────

app_story!(text_block_plain, |ctx| {
    ctx.set_viewport(40, 12);
    ctx.engine(EngineEvent::Text {
        content: "hello, world.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(thinking_block_then_answer, |ctx| {
    ctx.set_viewport(40, 12);
    ctx.engine(EngineEvent::Thinking {
        content: "let me think about this carefully.".into(),
    });
    ctx.engine(EngineEvent::Text {
        content: "the answer is 42.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_streams_via_deltas, |ctx| {
    ctx.set_viewport(40, 12);
    for chunk in ["stream ", "of ", "tokens."] {
        ctx.engine(EngineEvent::TextDelta {
            delta: chunk.into(),
        });
    }
    ctx.assert_snapshot();
});

app_story!(text_block_code_fence_no_language, |ctx| {
    // Fenced block without a language tag — the renderer must still
    // emit the code chrome (gutter, monospace body) but skip the
    // syntax highlighting pipeline. Pins the fallback path.
    ctx.set_viewport(60, 12);
    ctx.engine(EngineEvent::Text {
        content: "Raw output:\n\n```\nhello\nworld\n```\n\nDone.".into(),
    });
    ctx.assert_snapshot();
});

// ── User messages ─────────────────────────────────────────────────

app_story!(user_message_block_single_line, |ctx| {
    ctx.set_viewport(50, 8);
    ctx.push_user_turn("refactor the parser into its own module");
    ctx.assert_snapshot();
});

app_story!(user_message_block_multiline_chrome, |ctx| {
    // Multi-line user messages get the panel chrome (rounded box,
    // `block_w` width). Exercises `UserBlockGeometry::new` + the
    // shared `chrome::render` path.
    ctx.set_viewport(50, 10);
    ctx.push_user_turn("let's plan this out.\nstep 1: parse\nstep 2: render\nstep 3: ship");
    ctx.assert_snapshot();
});

app_story!(user_message_block_slash_command_accent, |ctx| {
    // `/permissions` is a built-in registered slash command, so
    // `user::render` paints the whole line with `SmeltAccent` fg
    // instead of the default body color. The styles snapshot
    // captures the accent color span — drift in the
    // command-resolver wiring will surface as a styles diff.
    ctx.set_viewport(40, 8);
    ctx.push_user_turn("/permissions");
    ctx.assert_snapshot();
});

// ── Error / compaction ────────────────────────────────────────────

app_story!(turn_error_block, |ctx| {
    // `TurnError` renders as the assistant's error block — its own
    // chrome and color group, distinct from a regular text block.
    ctx.set_viewport(60, 10);
    ctx.engine(EngineEvent::Text {
        content: "Working on it…".into(),
    });
    ctx.engine(EngineEvent::TurnError {
        message: "provider returned 503: service unavailable".into(),
    });
    ctx.assert_snapshot();
});

app_story!(compacted_block_summary, |ctx| {
    ctx.set_viewport(60, 8);
    ctx.push_compacted(
        "Compacted 8 earlier turns: parser refactor, renderer wiring, three bug fixes.",
    );
    ctx.assert_snapshot();
});

// ── Full agent turn composite ─────────────────────────────────────

app_story!(full_agent_turn_composite, |ctx| {
    // End-to-end smoke for the transcript: every block kind the user
    // sees in a single turn lined up in production order. If any
    // block's chrome / wrap / spacing regresses, the diff lands here
    // before any per-block story catches it.
    ctx.set_viewport(70, 22);
    ctx.push_user_turn("rename `add` to `checked_add`");
    ctx.engine(EngineEvent::Thinking {
        content: "scan the file, then issue an edit_file with the renamed signature.".into(),
    });
    ctx.engine(EngineEvent::Text {
        content: "I'll rename the function and harden it against overflow.".into(),
    });
    ctx.tool_call(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("fn add(a: i32, b: i32)")),
            ("new_string", json!("fn checked_add(a: i64, b: i64)")),
        ],
        "ok",
        Some(9),
    );
    ctx.engine(EngineEvent::Text {
        content: "Done. Want me to update the call sites too?".into(),
    });
    ctx.assert_snapshot();
});
