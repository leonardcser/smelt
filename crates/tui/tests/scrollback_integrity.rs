//! Scrollback integrity tests (vt100 harness).
//!
//! Verifies that incrementally rendered output matches a fresh re-render
//! of the same block history. Dialog lifecycle tests live in dialog_lifecycle.rs.

mod harness;

use harness::TestHarness;
use tui::render::Block;

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
    h.push_and_render(Block::ToolCall {
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
        status: tui::render::ToolStatus::Ok,
        output: Some(tui::render::ToolOutput {
            content: "fn main() {}".into(),
            is_error: false,
            metadata: None,
        }),
        user_message: None,
        elapsed: Some(std::time::Duration::from_millis(150)),
    });
    h.push_and_render(Block::Text {
        content: "I read the file.".into(),
    });
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
