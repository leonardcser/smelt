//! Scrollback integrity tests (vt100 harness).
//!
//! Verifies that incrementally rendered output matches a fresh re-render
//! of the same block history. Dialog lifecycle tests live in dialog_lifecycle.rs.

mod harness;

use harness::TestHarness;
use std::collections::HashMap;
use tui::render::{
    Block, ConfirmDialog, ConfirmRequest, Dialog, HelpDialog, RewindDialog, ToolStatus,
};

#[test]
fn single_block() {
    let mut h = TestHarness::new(80, 24, "single_block");
    h.push_and_render(Block::User {
        text: "hello world".into(),
        image_labels: vec![],
    });
    h.assert_scrollback_integrity();
}

#[test]
fn two_blocks() {
    let mut h = TestHarness::new(80, 24, "two_blocks");
    h.push_and_render(Block::User {
        text: "What is 2+2?".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "The answer is 4.".into(),
    });
    h.assert_scrollback_integrity();
}

#[test]
fn incremental_rendering() {
    let mut h = TestHarness::new(80, 24, "incremental_rendering");
    for i in 0..4 {
        h.push_and_render(Block::User {
            text: format!("question {i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("answer {i}"),
        });
        h.assert_scrollback_integrity();
    }
}

#[test]
fn scrollback_overflow() {
    let mut h = TestHarness::new(80, 10, "scrollback_overflow");
    for i in 0..20 {
        let block = if i % 2 == 0 {
            Block::User {
                text: format!("question {i}"),
                image_labels: vec![],
            }
        } else {
            Block::Text {
                content: format!("answer {i}"),
            }
        };
        h.push_and_render(block);
    }
    h.assert_scrollback_integrity();
}

#[test]
fn multiline_text() {
    let mut h = TestHarness::new(80, 24, "multiline_text");
    h.push_and_render(Block::User {
        text: "Tell me a story".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "Once upon a time,\nthere was a programmer\nwho loved testing.".into(),
    });
    h.assert_scrollback_integrity();
}

#[test]
fn batch_commit() {
    let mut h = TestHarness::new(80, 24, "batch_commit");
    h.push(Block::User {
        text: "question".into(),
        image_labels: vec![],
    });
    h.push(Block::Thinking {
        content: "thinking...".into(),
    });
    h.push(Block::Text {
        content: "answer".into(),
    });
    h.render_pending();
    h.assert_scrollback_integrity();
}

#[test]
fn tool_call_block() {
    let mut h = TestHarness::new(80, 24, "tool_call_block");
    h.push_and_render(Block::User {
        text: "Read the file".into(),
        image_labels: vec![],
    });
    h.push_tool_call_and_render(
        Block::ToolCall {
            call_id: "call-1".into(),
            name: "read".into(),
            summary: "Reading file.rs".into(),
            args: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "path".into(),
                    serde_json::Value::String("/src/main.rs".into()),
                );
                m
            },
        },
        tui::render::ToolState {
            status: tui::render::ToolStatus::Ok,
            output: Some(Box::new(tui::render::ToolOutput {
                content: "fn main() {}".into(),
                is_error: false,
                metadata: None,
                render_cache: None,
            })),
            user_message: None,
            elapsed: Some(std::time::Duration::from_millis(150)),
        },
    );
    h.push_and_render(Block::Text {
        content: "I read the file.".into(),
    });
    h.assert_scrollback_integrity();
}

#[test]
fn tool_call_empty_result_has_no_extra_line() {
    let mut h = TestHarness::new(80, 24, "tool_call_empty_result_has_no_extra_line");
    h.push_and_render(Block::User {
        text: "Run it".into(),
        image_labels: vec![],
    });
    h.push_tool_call_and_render(
        Block::ToolCall {
            call_id: "call-2".into(),
            name: "message_agent".into(),
            summary: "cedar".into(),
            args: std::collections::HashMap::new(),
        },
        tui::render::ToolState {
            status: tui::render::ToolStatus::Ok,
            output: Some(Box::new(tui::render::ToolOutput {
                content: String::new(),
                is_error: false,
                metadata: None,
                render_cache: None,
            })),
            user_message: None,
            elapsed: Some(std::time::Duration::from_millis(150)),
        },
    );
    h.push_and_render(Block::Text {
        content: "Done.".into(),
    });

    let text = h.full_text();
    assert!(
        !text.contains("cedar\n\nDone."),
        "tool call with empty result added a blank line before following text:\n{text}"
    );
    h.assert_scrollback_integrity();
}

#[test]
fn narrow_terminal() {
    let mut h = TestHarness::new(40, 24, "narrow_terminal");
    h.push_and_render(Block::User {
        text: "This is a message that will wrap on a narrow terminal".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "And this is a response that is also quite long and should wrap around nicely."
            .into(),
    });
    h.assert_scrollback_integrity();
}

// ── Code block streaming ─────────────────────────────────────────────

/// Simulate the real app flow: stream deltas, then EngineEvent::Text
/// pushes the final full content. flush_streaming_text commits the
/// streamed blocks, then the full text block is pushed on top.
/// On redraw, only the final block exists — it must render the same.
#[test]
fn streamed_code_block() {
    let mut h = TestHarness::new(80, 24, "streamed_code_block");
    h.push_and_render(Block::User {
        text: "Show me the code".into(),
        image_labels: vec![],
    });

    let full = "Here's the code:\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```";
    h.stream_and_flush(full);
    h.assert_scrollback_integrity();
}

/// Code block where the closing fence has no trailing newline.
#[test]
fn streamed_code_block_no_trailing_newline() {
    let mut h = TestHarness::new(80, 24, "streamed_code_block_no_trailing_newline");
    h.push_and_render(Block::User {
        text: "Code please".into(),
        image_labels: vec![],
    });
    h.stream_and_flush("Here:\n```rust\nfn main() {}\n```");
    h.assert_scrollback_integrity();
}

/// Text after the code block.
#[test]
fn streamed_code_block_then_text() {
    let mut h = TestHarness::new(80, 24, "streamed_code_block_then_text");
    h.push_and_render(Block::User {
        text: "Show code".into(),
        image_labels: vec![],
    });
    h.stream_and_flush("Here's the code:\n```rust\nfn main() {}\n```\nThat's it.");
    h.assert_scrollback_integrity();
}

/// Realistic streaming: line by line with ticks between chunks.
#[test]
fn streamed_code_block_with_ticks() {
    let mut h = TestHarness::new(80, 24, "streamed_code_block_with_ticks");
    h.push_and_render(Block::User {
        text: "Show me the code".into(),
        image_labels: vec![],
    });
    h.stream_lines_with_ticks(
        "Here's the code:\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n",
    );
    h.assert_scrollback_integrity();
}

/// Closing fence arrives without trailing newline — flush must handle it.
#[test]
fn streamed_code_block_closing_fence_in_flush() {
    let mut h = TestHarness::new(80, 24, "streamed_code_block_closing_fence_in_flush");
    h.push_and_render(Block::User {
        text: "Code".into(),
        image_labels: vec![],
    });
    // No trailing \n after closing fence.
    h.stream_and_flush("Here:\n```rust\nfn main() {}\n```");

    let text = h.full_text();
    assert!(
        !text.contains("```"),
        "Raw backticks visible in output!\n\nCaptured:\n{text}"
    );
}

/// Compare streamed output (Text + CodeLine blocks with gaps) against
/// a single Block::Text with the full markdown (as stored on resume).
/// The gap before the code block should be the same in both cases.
#[test]
fn code_block_gap_streaming_vs_resume() {
    let content = "Here's the code:\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```";

    // Streamed: produces Text + CodeLine blocks with gap_between.
    let mut h_streamed = TestHarness::new(80, 24, "code_block_gap_streamed");
    h_streamed.push_and_render(Block::User {
        text: "Show me the code".into(),
        image_labels: vec![],
    });
    h_streamed.stream_and_flush(content);
    let streamed_text = h_streamed.full_text();

    // Resume: one Block::Text with full markdown content.
    let mut h_resume = TestHarness::new(80, 24, "code_block_gap_resume");
    h_resume.push_and_render(Block::User {
        text: "Show me the code".into(),
        image_labels: vec![],
    });
    h_resume.push_and_render(Block::Text {
        content: content.into(),
    });
    let resume_text = h_resume.full_text();

    if streamed_text != resume_text {
        let dump_dir = "target/test-frames/code_block_gap_streaming_vs_resume";
        let _ = std::fs::create_dir_all(dump_dir);
        let _ = std::fs::write(format!("{dump_dir}/streamed.txt"), &streamed_text);
        let _ = std::fs::write(format!("{dump_dir}/resume.txt"), &resume_text);

        use similar::TextDiff;
        let diff = TextDiff::from_lines(&streamed_text, &resume_text);
        let mut diff_str = String::new();
        diff_str.push_str("--- streamed\n+++ resume\n");
        for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
            diff_str.push_str(&format!("{hunk}"));
        }
        let _ = std::fs::write(format!("{dump_dir}/diff.txt"), &diff_str);

        panic!(
            "Code block renders differently between streaming and resume!\n\
             Saved to: {dump_dir}/\n\n{diff_str}"
        );
    }
}

/// Heading followed by paragraph — no extra blank line after the heading.
#[test]
fn paragraph_after_heading_no_gap() {
    let content = "## Quick Start\nRun `agent` from your project root.";

    let mut h_streamed = TestHarness::new(80, 24, "paragraph_after_heading_streamed");
    h_streamed.push_and_render(Block::User {
        text: "How do I start?".into(),
        image_labels: vec![],
    });
    h_streamed.stream_and_flush(content);
    let streamed_text = h_streamed.full_text();

    let mut h_resume = TestHarness::new(80, 24, "paragraph_after_heading_resume");
    h_resume.push_and_render(Block::User {
        text: "How do I start?".into(),
        image_labels: vec![],
    });
    h_resume.push_and_render(Block::Text {
        content: content.into(),
    });
    let resume_text = h_resume.full_text();

    let norm = |s: &str| -> String {
        s.lines()
            .map(|l| if l.trim().is_empty() { "" } else { l })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let streamed_text = norm(&streamed_text);
    let resume_text = norm(&resume_text);

    assert_eq!(
        streamed_text, resume_text,
        "Heading + paragraph renders differently between streaming and resume"
    );
}

/// Heading followed by code block — no gap between them.
#[test]
fn code_block_after_heading_no_gap() {
    let content = "## Quick Start\n```bash\nnpm install\nnpm run build\n```";

    let mut h_streamed = TestHarness::new(80, 24, "code_block_after_heading_streamed");
    h_streamed.push_and_render(Block::User {
        text: "How do I start?".into(),
        image_labels: vec![],
    });
    h_streamed.stream_and_flush(content);
    let streamed_text = h_streamed.full_text();

    let mut h_resume = TestHarness::new(80, 24, "code_block_after_heading_resume");
    h_resume.push_and_render(Block::User {
        text: "How do I start?".into(),
        image_labels: vec![],
    });
    h_resume.push_and_render(Block::Text {
        content: content.into(),
    });
    let resume_text = h_resume.full_text();

    // Normalize whitespace-only lines.
    let norm = |s: &str| -> String {
        s.lines()
            .map(|l| if l.trim().is_empty() { "" } else { l })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let streamed_text = norm(&streamed_text);
    let resume_text = norm(&resume_text);

    assert_eq!(
        streamed_text, resume_text,
        "Heading + code block renders differently between streaming and resume"
    );
}

/// Multiple code blocks in one message.
#[test]
fn streamed_multiple_code_blocks() {
    let mut h = TestHarness::new(80, 24, "streamed_multiple_code_blocks");
    h.push_and_render(Block::User {
        text: "Show two files".into(),
        image_labels: vec![],
    });
    h.stream_and_flush(
        "First file:\n```rust\nfn a() {}\n```\nSecond file:\n```rust\nfn b() {}\n```",
    );
    h.assert_scrollback_integrity();
}

/// Code block with blank line before fence (typical LLM output).
/// Must not produce a double gap.
#[test]
fn code_block_gap_with_existing_blank_line() {
    let content = "Here's the code:\n\n```rust\nfn main() {}\n```";

    let mut h_streamed = TestHarness::new(80, 24, "code_block_gap_existing_blank_streamed");
    h_streamed.push_and_render(Block::User {
        text: "Show code".into(),
        image_labels: vec![],
    });
    h_streamed.stream_and_flush(content);
    let streamed_text = h_streamed.full_text();

    let mut h_resume = TestHarness::new(80, 24, "code_block_gap_existing_blank_resume");
    h_resume.push_and_render(Block::User {
        text: "Show code".into(),
        image_labels: vec![],
    });
    h_resume.push_and_render(Block::Text {
        content: content.into(),
    });
    let resume_text = h_resume.full_text();

    // Normalize: blank lines may differ in whitespace (indent vs none)
    // but both are visually identical vertical gaps.
    let norm = |s: &str| -> String {
        s.lines()
            .map(|l| if l.trim().is_empty() { "" } else { l })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let streamed_text = norm(&streamed_text);
    let resume_text = norm(&resume_text);

    if streamed_text != resume_text {
        let dump_dir = "target/test-frames/code_block_gap_with_existing_blank_line";
        let _ = std::fs::create_dir_all(dump_dir);
        let _ = std::fs::write(format!("{dump_dir}/streamed.txt"), &streamed_text);
        let _ = std::fs::write(format!("{dump_dir}/resume.txt"), &resume_text);

        use similar::TextDiff;
        let diff = TextDiff::from_lines(&streamed_text, &resume_text);
        let mut diff_str = String::new();
        diff_str.push_str("--- streamed\n+++ resume\n");
        for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
            diff_str.push_str(&format!("{hunk}"));
        }
        panic!("Double gap detected!\nSaved to: {dump_dir}/\n\n{diff_str}");
    }
}

// ── Confirm dialog overlay tests ─────────────────────────────────────

/// When the terminal is nearly full and a confirm dialog opens, the active
/// tool overlay should NOT be shown if it would cause scroll. This test
/// verifies that we don't end up with duplicate tool calls (one from the
/// overlay that scrolled, one from the committed block).
#[test]
fn confirm_dialog_no_duplicate_tool_when_nearly_full() {
    // Use a small height to force the "doesn't fit" scenario
    let mut h = TestHarness::new(80, 12, "confirm_no_duplicate_nearly_full");

    // Fill up most of the terminal with conversation
    for i in 0..5 {
        h.push_and_render(Block::User {
            text: format!("Question {i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("Answer to question {i}"),
        });
    }

    // Draw prompt to establish anchor row
    h.draw_prompt();

    // Now run a confirm cycle. Overlay always shows streaming tool state and
    // tail-crops above the dialog; no more single-tool pre-check.
    let summary = "unique-tool-summary-12345";
    let output = "unique-tool-output-67890";
    h.confirm_cycle("c1", "bash", summary, output);

    // The tool summary must appear at least once (not lost).
    // On small terminals, dialog ScrollUp may push overlay content
    // into scrollback, causing a second copy — that is a known
    // limitation of the terminal-scroll approach.
    let text = h.full_text();
    assert!(
        text.contains(summary),
        "tool summary lost from output:\n{text}"
    );
}

/// Confirm dialog cycle on a medium terminal. The dialog opens with
/// the lock, tool is approved, completes, and commits. Verifies the
/// full scrollback after the cycle matches a fresh render.
#[test]
fn confirm_dialog_locked_height_scrollback_integrity() {
    let mut h = TestHarness::new(80, 24, "confirm_locked_height_integrity");
    h.push_and_render(Block::User {
        text: "run a tool".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "Sure, let me run that.".into(),
    });
    h.draw_prompt();

    h.confirm_cycle("c1", "bash", "echo hello", "hello");
    h.assert_scrollback_integrity();
}

/// Two consecutive confirm-dialog cycles with different tool summaries.
/// Each dialog may have a different height; the lock must be properly
/// reset between cycles so the second dialog reserves the right space.
#[test]
fn confirm_dialog_consecutive_cycles_integrity() {
    // Use a tall terminal so the confirm dialog doesn't need to scroll,
    // avoiding the known gap-suppression heuristic mismatch after scroll.
    let mut h = TestHarness::new(80, 40, "confirm_consecutive_cycles");
    h.push_and_render(Block::User {
        text: "run two tools".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.confirm_cycle("c1", "bash", "echo a", "a");
    h.confirm_cycle(
        "c2",
        "bash",
        "for i in 1 2 3; do\n  echo $i\ndone",
        "1\n2\n3",
    );

    h.assert_scrollback_integrity();
}

/// Confirm dialog on a nearly full terminal where the dialog + overlay
/// must scroll. After dismiss and commit, scrollback must be coherent.
#[test]
fn confirm_dialog_scroll_then_commit_integrity() {
    let mut h = TestHarness::new(80, 14, "confirm_scroll_then_commit");

    // Fill the viewport.
    for i in 0..6 {
        h.push_and_render(Block::User {
            text: format!("Q_{i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("A_{i}"),
        });
    }
    h.draw_prompt();

    h.confirm_cycle("c1", "bash", "echo hello", "hello");

    // The tool output "hello" should appear exactly once (the committed block).
    // "echo hello" (the summary) also contains "hello", so count 2 is correct.
    let full = h.full_text();
    assert_eq!(
        full.matches("echo hello").count(),
        1,
        "tool summary duplicated or lost:\n{full}"
    );
}

/// Real app flow: tool starts Pending → normal frame (may scroll) → dialog
/// opens immediately (no normal frame between) → user approves → tool runs.
/// The tool summary must appear exactly once — the ghost from the initial
/// scroll should not persist as a duplicate.
#[test]
fn dialog_overlay_replaced_by_live_tool() {
    let mut h = TestHarness::new(80, 14, "dialog_overlay_replaced");

    // Fill terminal so anchor is near the bottom.
    for i in 0..5 {
        h.push_and_render(Block::User {
            text: format!("Question {i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("Answer to question {i}"),
        });
    }
    h.draw_prompt();

    let summary = "unique-overlay-MARKER";

    // 1. Tool starts as Pending — normal frame renders overlay (may scroll).
    h.screen
        .start_tool("c1".into(), "bash".into(), summary.into(), HashMap::new());
    h.draw_prompt(); // Pending tool + prompt — this is the frame that scrolls

    // 2. Immediately: tool transitions to Confirm, dialog opens.
    //    (In the real app, no normal frame happens between these.)
    h.screen.set_active_status("c1", ToolStatus::Confirm);
    let req = ConfirmRequest {
        call_id: "c1".into(),
        tool_name: "bash".into(),
        desc: summary.into(),
        args: HashMap::new(),
        approval_patterns: vec![],
        outside_dir: None,
        summary: Some(summary.into()),
        request_id: 1,
    };
    let mut dialog = ConfirmDialog::new(&req, false);
    dialog.set_term_size(h.width, h.height);
    h.screen.render_pending_blocks();
    h.screen.erase_prompt();
    h.screen.set_dialog_open(true);
    let dialog_height = dialog.height();
    {
        let mut frame = tui::render::Frame::begin(h.screen.backend());
        let (_redirtied, placement) =
            h.screen
                .draw_frame(&mut frame, h.width as usize, None, Some(dialog_height));
        if let Some(p) = placement {
            dialog.draw(&mut frame, p.row, h.width, p.granted_rows);
        }
    }
    h.drain_sink();

    // 3. User approves — dialog closes, tool continues running.
    h.screen.clear_dialog_area();
    h.drain_sink();
    h.screen.set_active_status("c1", ToolStatus::Pending);
    h.draw_prompt(); // tool now live-updating

    // The summary should appear exactly once (the live overlay).
    let text = h.full_text();
    let count = text.matches(summary).count();
    if count != 1 {
        let dump_dir = "target/test-frames/dialog_overlay_replaced";
        let _ = std::fs::create_dir_all(dump_dir);
        let _ = std::fs::write(format!("{dump_dir}/output.txt"), &text);
        panic!(
            "Expected 1 occurrence of tool summary, found {count}.\n\
             Output saved to: {dump_dir}/output.txt\n\n{text}"
        );
    }
}

/// Opening and closing the rewind dialog (non-blocking) should not shift
/// the prompt down. The prompt must stay at the same position.
#[test]
fn rewind_dialog_does_not_shift_prompt() {
    let mut h = TestHarness::new(80, 24, "rewind_dialog_no_shift");

    // Build a small conversation.
    h.push_and_render(Block::User {
        text: "What is 2+2?".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "The answer is 4.".into(),
    });

    // Draw prompt to establish stable position.
    h.draw_prompt();
    h.draw_prompt(); // second draw to stabilize

    // Capture prompt position before dialog.
    let before = harness::extract_full_content(&mut h.parser);

    // Open the rewind dialog (non-blocking, like Esc-Esc or /rewind).
    let turns = vec![(0, "What is 2+2?".to_string())];
    let mut dialog = RewindDialog::new(turns, false);

    // Simulate the real app flow: erase_prompt → dialog draws → tick with dialog.
    h.screen.erase_prompt();
    draw_dialog_frame(&mut h, &mut dialog);

    // Dismiss the dialog.
    h.screen.clear_dialog_area();
    h.screen.set_dialog_open(false);
    h.drain_sink();

    // Redraw prompt after dismiss.
    h.draw_prompt();

    let after = harness::extract_full_content(&mut h.parser);

    if before != after {
        let dump_dir = "target/test-frames/rewind_dialog_no_shift";
        let _ = std::fs::create_dir_all(dump_dir);
        let _ = std::fs::write(format!("{dump_dir}/before.txt"), &before);
        let _ = std::fs::write(format!("{dump_dir}/after.txt"), &after);

        use similar::TextDiff;
        let diff = TextDiff::from_lines(&before, &after);
        let mut diff_str = String::new();
        diff_str.push_str("--- before dialog\n+++ after dialog\n");
        for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
            diff_str.push_str(&format!("{hunk}"));
        }
        panic!(
            "Prompt shifted after rewind dialog dismiss!\n\
             Saved to: {dump_dir}/\n\n{diff_str}"
        );
    }
}

// ── Resize integrity tests ──────────────────────────────────────────

fn big_table_markdown(rows: usize) -> String {
    let mut s = String::new();
    s.push_str("| ID | Name    | Age | City         | Country     |\n");
    s.push_str("|----|---------|-----|--------------|-------------|\n");
    let names = [
        ("Alice", 28, "New York", "USA"),
        ("Bob", 34, "London", "UK"),
        ("Charlie", 22, "Paris", "France"),
        ("Diana", 31, "Tokyo", "Japan"),
        ("Eve", 27, "Berlin", "Germany"),
        ("Frank", 45, "Sydney", "Australia"),
        ("Grace", 29, "Toronto", "Canada"),
        ("Henry", 38, "Rome", "Italy"),
        ("Ivy", 24, "Madrid", "Spain"),
        ("Jack", 33, "Moscow", "Russia"),
    ];
    for i in 0..rows {
        let n = &names[i % names.len()];
        s.push_str(&format!(
            "| {:<2} | {:<7} | {:<3} | {:<12} | {:<11} |\n",
            i + 1,
            n.0,
            n.1,
            n.2,
            n.3,
        ));
    }
    s
}

/// After a purge redraw, a later height-only shrink/expand cycle must not
/// duplicate prompt/status rows in scrollback. We approximate that by checking
/// the visible viewport still matches a fresh render after the resize cycle.
#[test]
fn purge_then_height_resize_keeps_single_status_bar() {
    let mut h = TestHarness::new(120, 30, "purge_then_height_resize_keeps_single_status_bar");

    h.push_and_render(Block::User {
        text: "reply with a table with 20 rows".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: big_table_markdown(20),
    });
    h.draw_prompt();

    h.purge_redraw();
    h.draw_prompt();

    h.resize_then_tick_prompt(120, 12);

    h.resize_then_tick_prompt(120, 40);
    h.assert_visible_matches_fresh_render();
}

/// Height-only resize must not duplicate prompt/status rows. We check that
/// the visible viewport still matches a fresh render after shrinking and
/// re-expanding.
#[test]
fn height_only_resize_keeps_single_status_bar() {
    let mut h = TestHarness::new(120, 40, "height_only_resize_keeps_single_status_bar");

    h.push_and_render(Block::User {
        text: "reply with a table with 20 rows".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: big_table_markdown(20),
    });
    h.draw_prompt();

    h.resize_then_tick_prompt(120, 12);

    h.resize_then_tick_prompt(120, 40);
    h.assert_visible_matches_fresh_render();
}

/// Streaming an overlay (markdown table) that outgrows the visible viewport
/// must push committed content into scrollback and then crop the overlay's
/// own head once the viewport is full. We verify by asking for the full
/// extracted text (viewport + scrollback) to contain the original user
/// message and an early committed row — those must only be accessible
/// through scrollback after the overlay has grown past the viewport.
#[test]
fn overlay_push_up_pushes_committed_into_scrollback() {
    let mut h = TestHarness::new(80, 14, "overlay_push_up_pushes_committed_into_scrollback");
    // Committed: user message + short reply.
    h.push_and_render(Block::User {
        text: "Make me a 20-row table".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "Sure, here it is:".into(),
    });
    h.draw_prompt();

    // Stream a 20-row markdown table one line at a time with a tick
    // after each line. The overlay grows past the viewport; committed
    // content should end up in real scrollback.
    let table = big_table_markdown(20);
    h.stream_lines_with_ticks(&table);

    // Final full text (visible + scrollback) must still contain every
    // data row and the user message — no content lost, just repositioned.
    h.assert_contains_all(&[
        "Make me a 20-row table",
        "Sure, here it is:",
        "Alice",
        "Jack",
    ]);
}

/// When committed content has already filled the viewport to the
/// prompt area, a subsequent streaming overlay must still be visible
/// during streaming — not only after it commits. Exercises the case
/// where `base_anchor >= viewport_bottom` and the overlay only has
/// room after a `ScrollUp`.
#[test]
fn streaming_overlay_visible_after_viewport_full() {
    let mut h = TestHarness::new(80, 14, "streaming_overlay_visible_after_viewport_full");
    // Fill committed history until the prompt sits at the bottom of
    // the viewport — committed content reaches the prompt reserve.
    h.push_and_render(Block::User {
        text: "make a table with 20 rows".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: big_table_markdown(20),
    });
    h.push_and_render(Block::User {
        text: "make it 30".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Stream a second, longer table. During streaming the overlay
    // must be visible — not blank.
    h.screen.append_streaming_text(
        "| ID | Name    | Age |\n|----|---------|-----|\n| 1  | Alice   | 28  |\n",
    );
    h.draw_prompt();
    h.drain_sink();

    // Mid-stream visible viewport must contain at least one table row.
    // Before the fix, nothing showed until commit.
    let visible = {
        let (_rows, cols) = h.parser.screen().size();
        let lines: Vec<String> = h.parser.screen().rows(0, cols).collect();
        lines.join("\n")
    };
    assert!(
        visible.contains("Alice"),
        "Mid-stream overlay is invisible — nothing shows until commit.\n\
         Visible viewport:\n{visible}"
    );
}

/// Growing the prompt line-by-line must NEVER push prompt content
/// into terminal scrollback. Each frame, the visible viewport should
/// contain exactly the prompt section — no stale snapshots above.
#[test]
fn typing_multiline_prompt_does_not_pollute_scrollback() {
    let mut h = TestHarness::new(73, 20, "typing_multiline_no_scrollback_pollution");
    h.screen.set_anchor_row(8); // prompt at bottom, parent shell above

    h.draw_prompt();

    for i in 1..=22 {
        let mut lines: Vec<String> = Vec::new();
        for j in 1..=i {
            lines.push(format!("line_{j:02}"));
        }
        let input = lines.join("\n");
        h.draw_prompt_with_input(&input);

        // Count prompt bars in full text — should be exactly 2 (live).
        let full = harness::extract_full_content(&mut h.parser);
        let bar_rows = full
            .lines()
            .filter(|l| l.chars().filter(|c| *c == '\u{2500}').count() > 10)
            .count();
        if bar_rows != 2 {
            let dump = "target/test-frames/typing_multiline_no_scrollback_pollution";
            let _ = std::fs::create_dir_all(dump);
            let _ = std::fs::write(format!("{dump}/full_at_{i}.txt"), &full);
            panic!(
                "After typing {i} lines: {bar_rows} bar rows found (expected 2).\n\
                 Full:\n{full}"
            );
        }
    }
}

/// Multi-line prompt that grows line-by-line (like real typing with
/// Enter between lines), eventually filling most of the terminal.
/// After sending the message, the committed Block::User should appear
/// exactly once — no stale prompt chrome or duplicated input lines.
///
/// This is the highest-fidelity reproduction of the bug: the prompt
/// section grows across many frames, each updating `prev_prompt_ui_rows`,
/// and the final send transitions from a large prompt to a committed
/// block + small empty prompt.
#[test]
fn multiline_prompt_fullscreen_no_duplicate_on_send() {
    let mut h = TestHarness::new(80, 15, "multiline_prompt_fullscreen_no_dup");
    h.screen.set_anchor_row(0);

    // Grow the prompt line-by-line, redrawing each time (like typing).
    let mut lines: Vec<String> = Vec::new();
    for i in 0..10 {
        lines.push(format!("line_{i:02}"));
        let input = lines.join("\n");
        h.draw_prompt_with_input(&input);
    }

    let big_input = lines.join("\n");

    // User presses Enter — commit as Block::User, then redraw with empty prompt.
    h.push(Block::User {
        text: big_input.clone(),
        image_labels: vec![],
    });
    h.draw_prompt();

    let visible = harness::visible_content(&h.parser);
    let full = h.full_text();
    let dump_dir = "target/test-frames/multiline_prompt_fullscreen_no_dup";
    let _ = std::fs::create_dir_all(dump_dir);
    let _ = std::fs::write(format!("{dump_dir}/visible.txt"), &visible);
    let _ = std::fs::write(format!("{dump_dir}/full.txt"), &full);

    // "line_00" should appear exactly once (as the committed block,
    // not duplicated from a stale prompt echo).
    let count = full.matches("line_00").count();
    assert_eq!(
        count, 1,
        "line_00 duplicated ({count}x) after sending multi-line prompt:\n{full}"
    );
    // Prompt bars should be exactly 2 (live top + bottom).
    let bar_rows = full
        .lines()
        .filter(|l| l.chars().filter(|c| *c == '\u{2500}').count() > 10)
        .count();
    assert_eq!(
        bar_rows, 2,
        "stale prompt bars leaked after sending multi-line input ({bar_rows} bars):\n{full}"
    );
}

/// Multi-line prompt at 3 lines — smaller than fullscreen but still
/// taller than the minimal 1-line prompt. Catches off-by-one in the
/// prompt reserve calculation.
#[test]
fn multiline_prompt_3_lines_no_duplicate() {
    let mut h = TestHarness::new(80, 15, "multiline_prompt_3_lines");
    h.screen.set_anchor_row(11);
    let input = "first line\nsecond line\nthird line";
    h.draw_prompt_with_input(input);

    h.push(Block::User {
        text: input.into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    let full = h.full_text();
    let count = full.matches("first line").count();
    assert_eq!(count, 1, "multi-line input duplicated after send:\n{full}");
    let bar_rows = full
        .lines()
        .filter(|l| l.chars().filter(|c| *c == '\u{2500}').count() > 10)
        .count();
    assert_eq!(bar_rows, 2, "stale bars leaked ({bar_rows}):\n{full}");
}

/// Prompt anchored near the bottom (real-world startup), then grown
/// line-by-line until it overflows the viewport (scrollable input).
/// After sending, the committed block should appear once, no stale
/// chrome. This is the exact scenario the user reported.
#[test]
fn multiline_prompt_at_bottom_overflow_no_duplicate() {
    let mut h = TestHarness::new(80, 15, "multiline_prompt_at_bottom_overflow");

    // Anchor at row 11 = parent shell output fills rows 0-10.
    h.screen.set_anchor_row(11);

    // Grow the prompt line-by-line. The prompt section
    // (2 bars + N input lines + status = N+3) eventually exceeds
    // the 4 available rows (15 - 11). The prompt section scrolls
    // the terminal to fit, pushing stale content into scrollback.
    let mut lines: Vec<String> = Vec::new();
    for i in 0..12 {
        lines.push(format!("typed_{i:02}"));
        let input = lines.join("\n");
        h.draw_prompt_with_input(&input);
    }

    let big_input = lines.join("\n");

    // User sends.
    h.push(Block::User {
        text: big_input.clone(),
        image_labels: vec![],
    });
    h.draw_prompt();

    let full = h.full_text();
    let dump_dir = "target/test-frames/multiline_prompt_at_bottom_overflow";
    let _ = std::fs::create_dir_all(dump_dir);
    let _ = std::fs::write(format!("{dump_dir}/full.txt"), &full);

    // Each unique line should appear exactly once.
    for i in 0..12 {
        let marker = format!("typed_{i:02}");
        let count = full.matches(&marker).count();
        assert_eq!(
            count, 1,
            "{marker} duplicated ({count}x) after sending overflowed prompt:\n{full}"
        );
    }
    let bar_rows = full
        .lines()
        .filter(|l| l.chars().filter(|c| *c == '\u{2500}').count() > 10)
        .count();
    assert_eq!(bar_rows, 2, "stale bars leaked ({bar_rows}):\n{full}");
}

/// Prompt at the bottom with a single-line message, then a streamed
/// assistant reply. Regression test for the stale-bar bug.
/// Exercises anchor_row tracking through the commit + stream + commit
/// transition.
#[test]
fn multiline_prompt_then_stream_reply_no_duplicate() {
    let mut h = TestHarness::new(80, 15, "multiline_prompt_stream_reply");
    h.screen.set_anchor_row(11);

    let input = "line_A\nline_B\nline_C\nline_D\nline_E";
    h.draw_prompt_with_input(input);

    // Send the multi-line message.
    h.push(Block::User {
        text: input.into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Stream a response.
    h.screen.append_streaming_text("Reply paragraph one.\n");
    h.draw_prompt();
    h.screen.flush_streaming_text();
    h.draw_prompt();

    let full = h.full_text();
    let count = full.matches("line_A").count();
    assert_eq!(count, 1, "user input duplicated after stream:\n{full}");
    let bar_rows = full
        .lines()
        .filter(|l| l.chars().filter(|c| *c == '\u{2500}').count() > 10)
        .count();
    assert_eq!(bar_rows, 2, "stale bars leaked ({bar_rows}):\n{full}");
}

/// Sending a user message when the prompt is anchored at the bottom
/// of the terminal (typical real-world startup where parent shell
/// content fills the rows above smelt) must NOT leave stale prompt
/// rows visible between the new committed blocks. Reproduces a
/// regression where, after the user message and assistant response
/// commit, a leftover top bar from the prompt section appeared
/// between them in the viewport.
#[test]
fn send_user_message_at_bottom_does_not_duplicate_prompt() {
    let mut h = TestHarness::new(
        76,
        15,
        "send_user_message_at_bottom_does_not_duplicate_prompt",
    );

    // Real smelt starts with the cursor near the bottom of the
    // terminal because the parent shell already filled the rows above
    // (e.g. `cargo run` output). Simulate that by clearing anchor_row
    // and setting the backend cursor row to a deep value before the
    // first draw.
    h.screen.set_anchor_row(11);
    h.draw_prompt();

    // User types "hi" and presses Enter — push the block to history
    // and tick a prompt frame (mirroring how the real event loop
    // commits a user message via tick_prompt → draw_frame).
    h.push(Block::User {
        text: "hi".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // The assistant streams its response (like the real engine).
    h.screen
        .append_streaming_text("Hi. What do you want to work on?\n");
    h.draw_prompt();
    h.screen.flush_streaming_text();
    h.draw_prompt();

    let visible = harness::visible_content(&h.parser);
    let full = h.full_text();
    let dump_dir = "target/test-frames/send_user_message_at_bottom_does_not_duplicate_prompt";
    let _ = std::fs::create_dir_all(dump_dir);
    let _ = std::fs::write(format!("{dump_dir}/visible.txt"), &visible);
    let _ = std::fs::write(format!("{dump_dir}/full.txt"), &full);

    // Between " hi" and " Hi. What..." there should be exactly ONE
    // blank line (the gap), not multiple. Multiple blanks indicate
    // stale rows from a prior frame's prompt position.
    let lines: Vec<&str> = visible.lines().collect();
    let hi_idx = lines.iter().position(|l| l.contains(" hi")).unwrap();
    let reply_idx = lines
        .iter()
        .position(|l| l.contains("Hi. What do you want"))
        .unwrap();
    let blanks_between = (hi_idx + 1..reply_idx)
        .filter(|i| lines[*i].trim().is_empty())
        .count();
    let nonblanks_between = (hi_idx + 1..reply_idx)
        .filter(|i| !lines[*i].trim().is_empty())
        .count();
    assert_eq!(
        blanks_between, 1,
        "expected 1 blank gap between user msg and reply, got {blanks_between}:\n{visible}"
    );
    assert_eq!(
        nonblanks_between, 0,
        "expected no stale rows between user msg and reply, got {nonblanks_between}:\n{visible}"
    );

    // Bar rows (long horizontal-line runs) should equal exactly 2 —
    // the live prompt's top bar and bottom bar. More than that means a
    // stale prompt bar leaked from a previous frame.
    let bar_rows = visible
        .lines()
        .filter(|l| l.chars().filter(|c| *c == '\u{2500}').count() > 10)
        .count();
    assert_eq!(
        bar_rows, 2,
        "expected 2 bar rows (live prompt top + bottom), got {bar_rows}.\n\
         Likely a stale prompt bar leaked between committed blocks.\n{visible}"
    );
}

/// Streaming an overlay tall enough to push committed history off the
/// screen, then ticking the prompt repeatedly. The status bar must
/// stay in the viewport — never end up in scrollback.
///
/// Regression: the overlay reservation (`prev_prompt_ui_rows.max(1)`)
/// excluded the 1-row gap between content and prompt. The overlay
/// painted into the gap row, then the prompt's newline for the gap
/// triggered a terminal scroll, leaking 1+ rows of overlay into
/// scrollback per frame. After enough frames, the prompt's own status
/// bar climbed to the top of the viewport and itself got pushed.
#[test]
fn long_streaming_session_no_prompt_in_scrollback() {
    let mut h = TestHarness::new(80, 16, "long_streaming_session_no_prompt_in_scrollback");

    h.push_and_render(Block::User {
        text: "stream a big tool".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Start a streaming bash tool whose output will fill the overlay
    // (more rows than the viewport leaves for it). Each appended line
    // is followed by a draw_prompt tick — mirroring the engine's
    // per-token frame rate.
    h.start_bash_tool("c1", "stream lots of lines");
    for i in 0..50 {
        h.screen
            .append_active_output("c1", &format!("STREAM_LINE_{i:03}"));
        h.draw_prompt();
    }

    // The visible viewport (what the user sees right now, no scrollback)
    // should contain at most ~20 rows of stream output (the active
    // tool's cap). The overlay should NOT have leaked older lines into
    // scrollback that show up in extracted full text.
    let visible_only = harness::visible_content(&h.parser);
    let dump_dir = "target/test-frames/long_streaming_session_no_prompt_in_scrollback";
    let _ = std::fs::create_dir_all(dump_dir);
    let _ = std::fs::write(format!("{dump_dir}/visible.txt"), &visible_only);

    // Count STREAM_LINE rows in scrollback (full minus visible).
    let full = h.full_text();
    let _ = std::fs::write(format!("{dump_dir}/full.txt"), &full);

    let stream_in_full = full.matches("STREAM_LINE_").count();
    let stream_in_visible = visible_only.matches("STREAM_LINE_").count();
    let stream_in_scrollback = stream_in_full - stream_in_visible;
    assert_eq!(
        stream_in_scrollback, 0,
        "streaming overlay leaked into scrollback: {stream_in_scrollback} stream rows in scrollback.\n\
         visible rows in viewport: {stream_in_visible}\n\
         total stream rows seen: {stream_in_full}\n\n\
         Visible viewport:\n{visible_only}\n\n\
         Full (viewport+scrollback):\n{full}"
    );

    // Status bar's "normal" mode label should appear exactly once.
    let normal_count = full.matches("normal").count();
    assert_eq!(
        normal_count, 1,
        "status bar leaked into scrollback ({normal_count} occurrences of 'normal'):\n{full}"
    );
}

/// Multi-block history where the LAST block alone is bigger than the
/// redraw budget. Earlier blocks should be excluded entirely; the last
/// block has its head cropped so its tail is visible.
#[test]
fn redraw_multi_block_oversized_last_crops_last() {
    let mut h = TestHarness::new(80, 14, "redraw_multi_block_oversized_last");

    // Two short blocks that together fit easily.
    h.push_and_render(Block::User {
        text: "OLD_BLOCK_AAA".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "OLD_BLOCK_BBB".into(),
    });
    // Then a 3000-line user message — past MAX_REDRAW_LINES.
    let huge: String = (0..3000)
        .map(|i| format!("BIG_{i:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    h.push_and_render(Block::User {
        text: huge,
        image_labels: vec![],
    });
    h.draw_prompt();
    h.drain_sink();

    h.purge_redraw();
    h.draw_prompt();

    let full = h.full_text();
    // Tail of the huge block must be visible.
    assert!(
        full.contains("BIG_2999"),
        "tail of huge block missing after redraw"
    );
    // The earlier blocks should NOT be in scrollback or viewport — they
    // were excluded by `redraw_start` to make room for the cropped huge.
    assert!(
        !full.contains("OLD_BLOCK_AAA"),
        "earlier block should be excluded after redraw with oversized tail"
    );
    assert!(
        !full.contains("OLD_BLOCK_BBB"),
        "earlier block should be excluded after redraw with oversized tail"
    );
    // Head of the huge block should be cropped.
    assert!(
        !full.contains("BIG_0000"),
        "head of huge block should be cropped after redraw"
    );
}

/// A single user message taller than the redraw budget must still be
/// visible after a `redraw()` (Ctrl+L / resize). Earlier behavior:
/// `redraw_start` excluded the oversized block entirely and the
/// viewport went blank. Expected behavior: tail-crop the block (head
/// rows dropped) so its tail still appears, tmux-style.
#[test]
fn redraw_with_oversized_block_crops_head_not_drops() {
    let mut h = TestHarness::new(80, 14, "redraw_with_oversized_block_crops_head");

    // 3000-line user message — comfortably past MAX_REDRAW_LINES (2000).
    let huge: String = (0..3000)
        .map(|i| format!("LINE_{i:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    h.push_and_render(Block::User {
        text: huge,
        image_labels: vec![],
    });
    h.draw_prompt();
    h.drain_sink();

    // Force a redraw (e.g. user hit Ctrl+L or resized).
    h.purge_redraw();
    h.draw_prompt();

    let full = h.full_text();
    // The TAIL of the message must be visible — these are the very
    // last data lines, well past the 2000-row budget.
    assert!(
        full.contains("LINE_2999"),
        "tail of oversized block missing after redraw:\n{}",
        // Don't dump 3000 lines on failure — show the last 30 lines.
        full.lines().rev().take(30).collect::<Vec<_>>().join("\n")
    );
    assert!(
        full.contains("LINE_2900"),
        "expected line LINE_2900 in tail after redraw"
    );
    // The very first lines should NOT be present — they were cropped.
    assert!(
        !full.contains("LINE_0000"),
        "head of oversized block should be cropped after redraw"
    );
}

/// Shrink + re-expand while a streaming overlay is active must not
/// corrupt the visible viewport.
#[test]
fn resize_during_active_overlay_visible_matches_fresh() {
    let mut h = TestHarness::new(80, 30, "resize_during_active_overlay_visible_matches_fresh");
    h.push_and_render(Block::User {
        text: "Make a table".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Start streaming a table that will outgrow the final (small) viewport.
    h.screen.append_streaming_text(
        "| ID | Name    | Age |\n|----|---------|-----|\n| 1  | Alice   | 28  |\n| 2  | Bob     | 34  |\n| 3  | Charlie | 22  |\n",
    );
    h.draw_prompt();
    h.drain_sink();

    // Resize smaller while the overlay is live.
    h.resize_then_tick_prompt(80, 15);
    // Resize back larger.
    h.resize_then_tick_prompt(80, 30);

    // Flush the streamed text (commits the table).
    h.screen.flush_streaming_text();
    h.screen.render_pending_blocks();
    h.drain_sink();

    // The visible viewport after the resize cycle should match a
    // fresh render at the current size.
    h.assert_visible_matches_fresh_render();
}

// ── Non-blocking dialog + streaming overlay tests ──────────────────

/// Draw a dialog-mode frame: draw_frame in dialog mode (prompt=None),
/// then draw the dialog at the computed dialog_row, sync anchors.
/// Mirrors the real `render_frame` path for active non-blocking dialogs.
fn draw_dialog_frame(h: &mut TestHarness, dialog: &mut dyn Dialog) {
    h.screen.set_dialog_open(true);
    h.screen.set_constrain_dialog(dialog.constrain_height());
    let dh = dialog.height();
    {
        let mut frame = tui::render::Frame::begin(h.screen.backend());
        let (redirtied, placement) =
            h.screen
                .draw_frame(&mut frame, h.width as usize, None, Some(dh));
        if redirtied {
            dialog.mark_dirty();
        }
        if let Some(p) = placement {
            dialog.draw(&mut frame, p.row, h.width, p.granted_rows);
        }
        h.screen.queue_status_line(&mut frame);
    }
    h.drain_sink();
}

/// Simulate a timer-tick frame: dialog is marked dirty externally (by
/// the animation timer) but draw_frame has nothing new to paint.
/// This matches the real app's timer arm where `d.mark_dirty()` is
/// called before `render_frame`.
fn tick_dialog_animation(h: &mut TestHarness, dialog: &mut dyn Dialog) {
    dialog.mark_dirty();
    draw_dialog_frame(h, dialog);
}

/// Close a non-blocking dialog and transition back to prompt mode.
fn close_dialog(h: &mut TestHarness, _dialog: &dyn Dialog) {
    h.screen.clear_dialog_area();
    h.screen.set_dialog_open(false);
    h.drain_sink();
}

/// Open a non-blocking help dialog while a tool is active, stream more
/// content over several frames, close the dialog, and verify that the
/// overlay is coherent (tool + streaming text visible, not overlapping
/// the dialog).
#[test]
fn nonblocking_dialog_with_growing_overlay() {
    // Use a tall terminal so the help dialog leaves room for the overlay.
    let mut h = TestHarness::new(80, 50, "nonblocking_dialog_growing_overlay");
    h.push_and_render(Block::User {
        text: "do something".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Start a tool (appears in ephemeral overlay).
    h.start_bash_tool("c1", "TOOL_ALPHA");
    h.draw_prompt();

    // Open non-blocking dialog while tool is active.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    // Stream assistant text over several dialog-mode frames.
    // On each frame the overlay grows; the dialog must stay below it.
    for i in 0..5 {
        h.screen
            .append_streaming_text(&format!("Streaming line {i}.\n"));
        h.screen.mark_dirty();
        draw_dialog_frame(&mut h, &mut dialog);
    }

    // The latest streaming line should be visible (not cropped or
    // hidden behind the dialog).
    let v = harness::visible_content(&h.parser);
    assert!(
        v.contains("Streaming line 4"),
        "latest streaming text hidden behind dialog:\n{v}"
    );

    // Close dialog, flush and commit everything.
    close_dialog(&mut h, &dialog);
    h.draw_prompt();

    // After dismiss, the tool overlay should be visible.
    let v = harness::visible_content(&h.parser);
    assert!(
        v.contains("TOOL_ALPHA"),
        "tool overlay missing after dialog close:\n{v}"
    );

    // Full text should contain all markers exactly once.
    h.screen.flush_streaming_text();
    h.screen.render_pending_blocks();
    h.drain_sink();
    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        Some(Box::new(tui::render::ToolOutput {
            content: "tool output".into(),
            is_error: false,
            metadata: None,
            render_cache: None,
        })),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.flush_blocks();
    h.drain_sink();
    let full = h.full_text();
    assert_eq!(
        full.matches("do something").count(),
        1,
        "user message duplicated or lost:\n{full}"
    );
}

/// Open a non-blocking dialog, start a NEW tool while the dialog is
/// open, close the dialog, and verify the tools are visible. Tests
/// that starting ephemeral content during dialog mode doesn't corrupt
/// offsets.
#[test]
fn nonblocking_dialog_tool_starts_while_open() {
    let mut h = TestHarness::new(80, 24, "nonblocking_dialog_tool_starts");
    h.push_and_render(Block::User {
        text: "do work".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Open non-blocking dialog with no ephemeral content yet.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    // Start a tool while the dialog is open.
    h.start_bash_tool("c1", "TOOL_BETA");
    h.screen.mark_dirty();
    draw_dialog_frame(&mut h, &mut dialog);

    // Start another tool.
    h.start_bash_tool("c2", "TOOL_GAMMA");
    h.screen.mark_dirty();
    draw_dialog_frame(&mut h, &mut dialog);

    // Close dialog.
    close_dialog(&mut h, &dialog);
    h.draw_prompt();

    // Both tools should be visible in the overlay.
    let v = harness::visible_content(&h.parser);
    assert!(
        v.contains("TOOL_BETA"),
        "tool BETA missing after dialog close:\n{v}"
    );
    assert!(
        v.contains("TOOL_GAMMA"),
        "tool GAMMA missing after dialog close:\n{v}"
    );
}

/// Full viewport with committed history, open non-blocking dialog,
/// stream content while dialog is open, close dialog, verify no
/// committed content is duplicated.
#[test]
fn nonblocking_dialog_viewport_overflow() {
    // Use a large terminal so the dialog fits alongside the content.
    let mut h = TestHarness::new(80, 40, "nonblocking_dialog_viewport_overflow");

    // A few committed blocks.
    for i in 0..3 {
        h.push_and_render(Block::User {
            text: format!("MSG_{i:02}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("REPLY_{i:02}"),
        });
    }
    h.draw_prompt();

    // Start a tool.
    h.start_bash_tool("c1", "OVERFLOW_TOOL");
    h.draw_prompt();

    // Open non-blocking dialog.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    // Stream text while dialog is open.
    for i in 0..5 {
        h.screen
            .append_streaming_text(&format!("overflow_line_{i:02}\n"));
        h.screen.mark_dirty();
        draw_dialog_frame(&mut h, &mut dialog);
    }

    // Close dialog. Let draw_prompt commit pending blocks.
    close_dialog(&mut h, &dialog);
    h.screen.flush_streaming_text();
    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        Some(Box::new(tui::render::ToolOutput {
            content: "done".into(),
            is_error: false,
            metadata: None,
            render_cache: None,
        })),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.commit_active_tools();
    h.draw_prompt();

    // All committed history must appear in full text.
    let full = h.full_text();
    for i in 0..3 {
        let marker = format!("MSG_{i:02}");
        assert!(full.contains(&marker), "{marker} lost from output:\n{full}");
    }
}

/// Open a non-blocking dialog, a tool completes and commits while the
/// dialog is open, close dialog, verify the tool result appears once.
#[test]
fn nonblocking_dialog_tool_completes_while_open() {
    let mut h = TestHarness::new(80, 24, "nonblocking_dialog_tool_completes");
    h.push_and_render(Block::User {
        text: "run tool".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Start tool.
    h.start_bash_tool("c1", "COMPLETE_ME");
    h.draw_prompt();

    // Open non-blocking dialog.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    // Tool completes while dialog is open.
    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        Some(Box::new(tui::render::ToolOutput {
            content: "tool result 42".into(),
            is_error: false,
            metadata: None,
            render_cache: None,
        })),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.flush_blocks();
    h.drain_sink();
    draw_dialog_frame(&mut h, &mut dialog);

    // Close dialog.
    close_dialog(&mut h, &dialog);

    // Tool result should appear exactly once in full text.
    let full = h.full_text();
    assert_eq!(
        full.matches("tool result 42").count(),
        1,
        "tool result duplicated or lost:\n{full}"
    );
}

/// Multiple dialog open/close cycles while the agent is working.
/// Content streamed during each dialog cycle must not be duplicated
/// or lost across viewport + scrollback.
#[test]
fn nonblocking_dialog_repeated_open_close() {
    let mut h = TestHarness::new(80, 20, "nonblocking_dialog_repeated_open_close");
    h.push_and_render(Block::User {
        text: "keep working".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.start_bash_tool("c1", "PERSISTENT_TOOL");

    for cycle in 0..3 {
        // Stream content.
        h.screen
            .append_streaming_text(&format!("cycle_{cycle}_text\n"));
        h.draw_prompt();

        // Open dialog.
        h.screen.erase_prompt();
        let mut dialog = HelpDialog::new(false);
        draw_dialog_frame(&mut h, &mut dialog);

        // Stream more while dialog is open.
        h.screen
            .append_streaming_text(&format!("during_dialog_{cycle}\n"));
        h.screen.mark_dirty();
        draw_dialog_frame(&mut h, &mut dialog);

        // Close dialog.
        close_dialog(&mut h, &dialog);
        h.draw_prompt();
    }

    // After the last cycle, the tool overlay should still be visible.
    let v = harness::visible_content(&h.parser);
    assert!(
        v.contains("PERSISTENT_TOOL"),
        "tool overlay lost after repeated dialog cycles:\n{v}"
    );

    // Flush and commit.
    h.screen.flush_streaming_text();
    h.screen.render_pending_blocks();
    h.drain_sink();
    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        Some(Box::new(tui::render::ToolOutput {
            content: "final output".into(),
            is_error: false,
            metadata: None,
            render_cache: None,
        })),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.flush_blocks();
    h.drain_sink();

    // All content should appear in full text (viewport + scrollback).
    let full = h.full_text();
    for cycle in 0..3 {
        assert!(
            full.contains(&format!("cycle_{cycle}_text")),
            "cycle_{cycle}_text missing:\n{full}"
        );
        assert!(
            full.contains(&format!("during_dialog_{cycle}")),
            "during_dialog_{cycle} missing:\n{full}"
        );
    }
}

/// Tool output must remain visible on EVERY frame while a non-blocking
/// dialog is open. The dialog animation timer causes redraws even when
/// the overlay hasn't changed — the tool output must survive those.
#[test]
fn nonblocking_dialog_tool_visible_every_frame() {
    let mut h = TestHarness::new(80, 30, "nonblocking_dialog_tool_visible_every_frame");
    h.push_and_render(Block::User {
        text: "run something".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Start a tool.
    h.start_bash_tool("c1", "VISIBLE_TOOL");
    h.draw_prompt();

    // Open non-blocking dialog.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    // Multiple timer-tick frames (dialog dirty, no content change).
    for i in 0..5 {
        tick_dialog_animation(&mut h, &mut dialog);
        let v = harness::visible_content(&h.parser);
        assert!(
            v.contains("VISIBLE_TOOL"),
            "tool disappeared on timer tick {i}:\n{v}"
        );
    }

    // New tool starts while dialog is open.
    h.start_bash_tool("c2", "SECOND_TOOL");
    h.screen.mark_dirty();
    draw_dialog_frame(&mut h, &mut dialog);
    let v = harness::visible_content(&h.parser);
    assert!(v.contains("SECOND_TOOL"), "second tool not visible:\n{v}");

    // More timer ticks — both tools must survive.
    for i in 0..3 {
        tick_dialog_animation(&mut h, &mut dialog);
        let v = harness::visible_content(&h.parser);
        assert!(
            v.contains("VISIBLE_TOOL") && v.contains("SECOND_TOOL"),
            "tools disappeared on tick {i} after second tool:\n{v}"
        );
    }
}

/// On a small terminal with committed history filling the viewport,
/// active tools must still be visible above the dialog — not fully
/// cropped by the dialog's height reservation.
#[test]
fn nonblocking_dialog_small_terminal_tool_visible() {
    let mut h = TestHarness::new(80, 18, "nonblocking_dialog_small_term_tool");

    // Fill some history.
    for i in 0..4 {
        h.push_and_render(Block::User {
            text: format!("Q{i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("A{i}"),
        });
    }
    h.draw_prompt();

    // Start a tool.
    h.start_bash_tool("c1", "SMALL_TERM_TOOL");
    h.draw_prompt();

    // Open dialog.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    let v = harness::visible_content(&h.parser);
    assert!(
        v.contains("SMALL_TERM_TOOL"),
        "tool not visible on small terminal with dialog:\n{v}"
    );

    // Timer ticks.
    for i in 0..3 {
        tick_dialog_animation(&mut h, &mut dialog);
        let v = harness::visible_content(&h.parser);
        assert!(
            v.contains("SMALL_TERM_TOOL"),
            "tool vanished on timer tick {i} (small terminal):\n{v}"
        );
    }
}

/// Streaming text updates while a non-blocking dialog is open must
/// not cause earlier tool output to vanish.
#[test]
fn nonblocking_dialog_streaming_preserves_tools() {
    let mut h = TestHarness::new(80, 30, "nonblocking_dialog_streaming_preserves_tools");
    h.push_and_render(Block::User {
        text: "work".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Start tool and stream text.
    h.start_bash_tool("c1", "TOOL_PERSISTENT");
    h.screen.append_streaming_text("Some streamed text.\n");
    h.draw_prompt();

    // Open dialog.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    // Stream more text while dialog is open.
    for i in 0..5 {
        h.screen.append_streaming_text(&format!("streaming_{i}\n"));
        h.screen.mark_dirty();
        draw_dialog_frame(&mut h, &mut dialog);

        let v = harness::visible_content(&h.parser);
        assert!(
            v.contains("TOOL_PERSISTENT"),
            "tool vanished after streaming update {i}:\n{v}"
        );
    }
}

// ── Dialog open → close → scrollback integrity ─────────────────────
//
// The user's exact flow: open dialog while agent is working, content
// streams, close dialog, re-render — the result must match a fresh
// render of the same block history.

/// Open a non-blocking dialog with a single active tool, draw a few
/// frames, close, and verify scrollback integrity matches fresh render.
#[test]
fn dialog_open_close_scrollback_integrity_simple() {
    let mut h = TestHarness::new(80, 24, "dialog_open_close_integrity_simple");
    h.push_and_render(Block::User {
        text: "hello".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "hi there".into(),
    });
    h.draw_prompt();

    // Start a tool overlay.
    h.start_bash_tool("c1", "TOOL_ONE");
    h.draw_prompt();

    // Open dialog, draw a few frames.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);
    draw_dialog_frame(&mut h, &mut dialog);

    // Close dialog.
    close_dialog(&mut h, &dialog);

    // Finish tool and commit.
    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        Some(Box::new(tui::render::ToolOutput {
            content: "output".into(),
            is_error: false,
            metadata: None,
            render_cache: None,
        })),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.commit_active_tools();

    // The critical assertion: after dialog close + prompt redraw,
    // scrollback must match a fresh render.
    h.assert_scrollback_integrity();
}

/// Same as above but with streaming text during the dialog.
#[test]
fn dialog_open_close_scrollback_integrity_with_streaming() {
    let mut h = TestHarness::new(80, 40, "dialog_open_close_integrity_streaming");
    h.push_and_render(Block::User {
        text: "run things".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.start_bash_tool("c1", "TOOL_STREAM");
    h.screen.append_streaming_text("Some text.\n");
    h.draw_prompt();

    // Open dialog, stream while open.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    for i in 0..3 {
        h.screen.append_streaming_text(&format!("line {i}\n"));
        h.screen.mark_dirty();
        draw_dialog_frame(&mut h, &mut dialog);
    }

    // Close dialog.
    close_dialog(&mut h, &dialog);

    // Flush and commit everything.
    h.screen.flush_streaming_text();
    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        Some(Box::new(tui::render::ToolOutput {
            content: "done".into(),
            is_error: false,
            metadata: None,
            render_cache: None,
        })),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.commit_active_tools();

    h.assert_scrollback_integrity();
}

/// Multiple blocks of history, dialog open/close, check integrity.
#[test]
fn dialog_open_close_scrollback_integrity_with_history() {
    let mut h = TestHarness::new(80, 40, "dialog_open_close_integrity_history");
    for i in 0..5 {
        h.push_and_render(Block::User {
            text: format!("Q{i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("A{i}"),
        });
    }
    h.draw_prompt();

    h.start_bash_tool("c1", "TOOL_HIST");
    h.draw_prompt();

    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);
    tick_dialog_animation(&mut h, &mut dialog);
    tick_dialog_animation(&mut h, &mut dialog);

    close_dialog(&mut h, &dialog);

    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        Some(Box::new(tui::render::ToolOutput {
            content: "result".into(),
            is_error: false,
            metadata: None,
            render_cache: None,
        })),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.commit_active_tools();

    h.assert_scrollback_integrity();
}

/// A large AgentMessage block arrives while the agents detail dialog is
/// open. After closing the dialog and redrawing, the scrollback must
/// match a fresh render — no huge gap of blank lines.
#[test]
fn agent_message_during_dialog_no_blank_gap() {
    let mut h = TestHarness::new(80, 30, "agent_message_during_dialog");
    h.push_and_render(Block::User {
        text: "do work".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "on it".into(),
    });
    h.draw_prompt();

    // Start a tool so there's ephemeral overlay.
    h.start_bash_tool("c1", "TOOL_X");
    h.draw_prompt();

    // Open non-blocking dialog.
    h.screen.erase_prompt();
    let mut dialog = HelpDialog::new(false);
    draw_dialog_frame(&mut h, &mut dialog);

    // A large agent message arrives while the dialog is open.
    let big_msg: String = (0..20)
        .map(|i| format!("  line {i} of the agent report"))
        .collect::<Vec<_>>()
        .join("\n");
    h.push(Block::AgentMessage {
        from_id: "sub-agent".into(),
        from_slug: "sub".into(),
        content: big_msg,
    });
    h.render_pending();
    draw_dialog_frame(&mut h, &mut dialog);

    // A few more timer ticks while the dialog is open.
    tick_dialog_animation(&mut h, &mut dialog);
    tick_dialog_animation(&mut h, &mut dialog);

    // Close dialog.
    close_dialog(&mut h, &dialog);

    // Finish tool, commit.
    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        Some(Box::new(tui::render::ToolOutput {
            content: "done".into(),
            is_error: false,
            metadata: None,
            render_cache: None,
        })),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.commit_active_tools();

    // After dialog close, scrollback must match a fresh render.
    h.assert_scrollback_integrity();

    // Also verify no huge blank gap (more than 2 consecutive blank
    // lines) in the full text.
    let full = h.full_text();
    let mut consecutive_blanks = 0u32;
    for line in full.lines() {
        if line.trim().is_empty() {
            consecutive_blanks += 1;
        } else {
            consecutive_blanks = 0;
        }
        assert!(
            consecutive_blanks <= 2,
            "found {consecutive_blanks} consecutive blank lines in scrollback:\n{full}"
        );
    }
}
