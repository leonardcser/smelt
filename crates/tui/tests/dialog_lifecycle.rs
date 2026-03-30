//! Dialog lifecycle tests (vt100 harness).
//!
//! Verifies that content survives the confirm dialog open/dismiss cycle
//! and that no extra gaps are introduced.

mod harness;

use harness::TestHarness;
use tui::render::Block;

/// Check that no double blank lines (3+ consecutive empty lines) appear.
fn assert_no_double_gaps(text: &str, test_name: &str) {
    let lines: Vec<&str> = text.lines().collect();
    for (i, window) in lines.windows(3).enumerate() {
        if window.iter().all(|l| l.trim().is_empty()) {
            let start = i.saturating_sub(3);
            let end = (i + 6).min(lines.len());
            let context: String = lines[start..end]
                .iter()
                .enumerate()
                .map(|(j, l)| format!("{:3}│{l}", start + j + 1))
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "{test_name}: double blank line at line {}\n\n{context}\n",
                i + 1
            );
        }
    }
}

#[test]
fn confirm_simple() {
    let mut h = TestHarness::new(80, 24, "confirm_simple");
    h.push_and_render(Block::User {
        text: "Edit the file".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "I'll edit that for you.".into(),
    });
    h.confirm_cycle("c1", "write", "Writing main.rs", "fn main() {}");
    h.assert_contains_all(&["Edit the file", "I'll edit that for you", "Writing main.rs"]);
}

#[test]
fn confirm_with_scrollback() {
    let mut h = TestHarness::new(80, 24, "confirm_with_scrollback");
    for i in 0..8 {
        h.push_and_render(Block::User {
            text: format!("Msg {i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("Reply {i}"),
        });
    }
    h.confirm_cycle("c1", "bash", "cmd", "output");

    let mut expected: Vec<String> = Vec::new();
    for i in 0..8 {
        expected.push(format!("Msg {i}"));
        expected.push(format!("Reply {i}"));
    }
    let refs: Vec<&str> = expected.iter().map(|s| s.as_str()).collect();
    h.assert_contains_all(&refs);
}

#[test]
fn confirm_back_to_back() {
    let mut h = TestHarness::new(80, 24, "confirm_back_to_back");
    h.push_and_render(Block::User {
        text: "Write files".into(),
        image_labels: vec![],
    });
    for i in 0..3 {
        let id = format!("c{i}");
        h.confirm_cycle(&id, "write", &format!("file_{i}.rs"), &format!("// {i}"));
    }
    h.assert_contains_all(&["Write files", "// 0", "// 1", "// 2"]);
}

#[test]
fn no_double_gap_after_confirm() {
    let mut h = TestHarness::new(80, 24, "no_double_gap_after_confirm");
    h.push_and_render(Block::User {
        text: "Edit".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "Sure.".into(),
    });
    h.confirm_cycle("c1", "write", "main.rs", "fn main() {}");
    h.push_and_render(Block::Text {
        content: "Done.".into(),
    });

    let text = h.full_text();
    assert_no_double_gaps(&text, "no_double_gap_after_confirm");
}

#[test]
fn no_double_gap_nearly_full_terminal() {
    let mut h = TestHarness::new(80, 24, "no_double_gap_nearly_full_terminal");
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
    h.confirm_cycle("c1", "bash", "cmd", "output");

    let text = h.full_text();
    assert_no_double_gaps(&text, "no_double_gap_nearly_full_terminal");
}
