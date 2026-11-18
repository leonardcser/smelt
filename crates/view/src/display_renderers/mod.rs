//! Display renderers for compiled transcript blocks.

mod image_leaf;
mod layout_ir;
mod markdown;
mod temp_rows;

#[cfg(test)]
pub use layout_ir::measure_layout_ir_with_options;
pub use layout_ir::{
    measure_layout_ir_plan, refresh_layout_ir_content_measurements, render_layout_ir_into,
    render_layout_ir_into_with_history, render_layout_ir_range_into_measured, MeasuredLayout,
};
pub use markdown::render_markdown_inner;

#[cfg(test)]
mod tests {
    use crate::display_layout::{compile_block, render_block_into, RenderCtx};
    use smelt_buffer::buffer::{BufCreateOpts, BufId, Buffer};
    use smelt_buffer::cell_width;
    use smelt_buffer::theme::Theme;
    use smelt_transcript::content::builder::test_util::{read_buffer, TestLine};
    use smelt_transcript::content::{markdown_stream::thinking_summary, LayoutContext};
    use smelt_transcript::transcript_model::{
        gap_between, Block, ToolState, ToolStatus, ViewState,
    };
    use std::collections::HashMap;

    const W: usize = 80;
    const BLOCK_GUTTER_SPACE: &str = "  ";
    const CHROME_INNER_PAD: usize = 1;
    const THINKING_GUTTER: &str = "│ ";

    fn mk_collector_buf() -> (Buffer, Theme) {
        (
            Buffer::new(BufId(0), BufCreateOpts::default()),
            Theme::default(),
        )
    }

    fn render_block_test_into(
        buf: &mut Buffer,
        theme: &Theme,
        block: &Block,
        _state: Option<&ToolState>,
        _body: Option<()>,
        ctx: LayoutContext,
    ) -> u16 {
        let display = compile_block(block);
        render_block_into(
            buf,
            &display,
            RenderCtx {
                width: ctx.width,
                view_state: ctx.view_state,
                theme,
                history: None,
                inline_options: Default::default(),
            },
        )
        .line_count as u16
    }

    fn layout_block_test(
        block: &Block,
        state: Option<&ToolState>,
        ctx: &LayoutContext,
    ) -> Vec<TestLine> {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let rows = render_block_test_into(&mut buf, &theme, block, state, None, *ctx) as usize;
        read_buffer(&buf, &theme, rows)
    }

    fn text(s: &str) -> Block {
        Block::Text {
            content: s.to_string().into(),
        }
    }

    fn user(s: &str) -> Block {
        Block::User {
            text: s.to_string(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        }
    }

    fn thinking(s: &str) -> Block {
        Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            content: s.to_string().into(),
            kind: protocol::ReasoningKind::Raw,
        }
    }

    fn reasoning_summary(title: &str, content: &str) -> Block {
        Block::Thinking {
            title: Some(title.to_string()),
            summary_titles: vec![title.to_string()],
            content: content.to_string().into(),
            kind: protocol::ReasoningKind::Summary,
        }
    }

    fn empty_tool_call() -> Block {
        Block::ToolCall {
            call_id: String::new(),
            name: String::new(),
            summary: protocol::StyledLines::default(),
            args: HashMap::new().into(),
        }
    }

    fn pending_tool_state() -> ToolState {
        ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            called_at_ms: None,
            elapsed_active: true,
            output: None,
            user_message: None,
            preview_output: None,
        }
    }

    fn state_for(block: &Block) -> Option<ToolState> {
        matches!(block, Block::ToolCall { .. }).then(pending_tool_state)
    }

    fn block_rows(block: &Block) -> u16 {
        let (mut buf, theme) = mk_collector_buf();
        let st = state_for(block);
        render_block_test_into(
            &mut buf,
            &theme,
            block,
            st.as_ref(),
            None,
            LayoutContext::new(W as u16, ViewState::Expanded),
        )
    }

    fn tool_gap_for(blocks: &[Block]) -> u16 {
        blocks
            .last()
            .map(|b| gap_between(b, &empty_tool_call()))
            .unwrap_or(0)
    }

    fn render_all_at_once(blocks: &[Block]) -> (u16, u16, u16) {
        let (mut buf, theme) = mk_collector_buf();
        let mut total = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&blocks[i - 1], &blocks[i])
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block_test_into(
                    &mut buf,
                    &theme,
                    &blocks[i],
                    st.as_ref(),
                    None,
                    LayoutContext::new(W as u16, ViewState::Expanded),
                )
            };
            total += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (total, tg, total + tg)
    }

    #[test]
    fn empty_thinking_text_tool() {
        let blocks = vec![user("fix it"), thinking(""), text("Here's the fix.")];
        let a = render_all_at_once(&blocks);

        let thinking_rows = block_rows(&thinking(""));
        assert_eq!(thinking_rows, 0);

        let user_thinking_gap = gap_between(&user("fix it"), &thinking(""));
        let thinking_text_gap = gap_between(&thinking(""), &text("Here's the fix."));
        assert_eq!(user_thinking_gap, 1);
        assert_eq!(thinking_text_gap, 1);

        assert_eq!(a.1, 1, "tool gap after text = 1");
    }

    #[test]
    fn text_with_internal_blank_line() {
        // Text with internal blank line: "para1\n\npara2"
        let blocks = vec![user("hello"), text("para1\n\npara2")];
        let rows = block_rows(&text("para1\n\npara2"));
        assert_eq!(rows, 3, "3 rows: para1, blank, para2");

        let a = render_all_at_once(&blocks);
        assert_eq!(a.1, 1, "tool gap still 1");
    }

    #[test]
    fn empty_text_before_tool() {
        let blocks = vec![user("hello"), text("")];
        let rows = block_rows(&text(""));
        assert_eq!(rows, 0, "empty text renders 0 rows");

        let gap = gap_between(&text(""), &empty_tool_call());
        assert_eq!(gap, 1, "gap is still 1 for empty text block");

        let a = render_all_at_once(&blocks);

        let blocks_no_empty = vec![user("hello")];
        let c = render_all_at_once(&blocks_no_empty);
        let gap_user_tool = gap_between(&user("hello"), &empty_tool_call());
        assert_eq!(gap_user_tool, 1, "User→ActiveTool = 1");

        let user_text_gap = gap_between(&user("hello"), &text(""));
        assert_eq!(user_text_gap, 1, "User→Text = 1");

        let diff = a.2 as i32 - c.2 as i32;
        assert_eq!(diff, 1, "empty text block adds 1 extra gap row");
    }

    #[test]
    fn adjacent_thinking_blocks_only_gap_before_new_titles() {
        let gap = gap_between(&thinking("titled"), &thinking("untitled"));
        assert_eq!(gap, 0, "Thinking→Thinking gap without new title = 0");

        let titled_gap = gap_between(
            &thinking("untitled"),
            &reasoning_summary("New title", "body"),
        );
        assert_eq!(titled_gap, 1, "Thinking→Thinking gap before new title = 1");
    }

    #[test]
    fn empty_thinking_adds_extra_gap() {
        let with_empty_thinking = vec![user("hi"), thinking(""), text("response")];
        let without_thinking = vec![user("hi"), text("response")];

        let a = render_all_at_once(&with_empty_thinking);
        let b = render_all_at_once(&without_thinking);

        let diff = a.2 as i32 - b.2 as i32;
        assert_eq!(diff, 1, "empty thinking adds 1 extra gap row");
    }

    #[test]
    fn user_text_sanitizes_display_controls() {
        let rows = layout_block_test(
            &user("a\0\tb\nc\r"),
            None,
            &LayoutContext::new(40, ViewState::Expanded),
        );
        let text = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("a\u{FFFD}    b"));
        assert!(text.contains("c\u{FFFD}"));
        assert!(!text.contains('\0'));
        assert!(!text.contains('\r'));
    }

    #[test]
    fn exec_output_carriage_return_renders_latest_status_line() {
        let rows = layout_block_test(
            &Block::Exec {
                command: "git rebase main".into(),
                output: "Rebasing (1/1)\r\x1b[KSuccessfully rebased and updated refs/heads/topic."
                    .into(),
            },
            None,
            &LayoutContext::new(W as u16, ViewState::default()),
        );
        let text = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("Successfully rebased and updated refs/heads/topic."));
        assert!(!text.contains("Rebasing (1/1)"));
        assert!(!text.contains("[K"));
        assert!(!text.contains('\r'));
    }

    #[test]
    fn exec_command_controls_are_sanitized_before_wrapping() {
        let command =
            "\0\0\x00677S7\0\0\0\0\0\0*\x001k77 @crates/term/tests/storybook/snapshots/layout::vbox_mixed_length_fill_and_min.snap @FI";
        let (mut buf, theme) = mk_collector_buf();
        let block = Block::Exec {
            command: command.into(),
            output: String::new().into(),
        };
        let rows = render_block_test_into(
            &mut buf,
            &theme,
            &block,
            None,
            None,
            LayoutContext::new(107, ViewState::Expanded),
        ) as usize;

        assert!(rows > 3, "long command should wrap inside chrome");
        for row in 0..rows {
            let line = buf.get_line(row).unwrap_or("");
            assert!(
                !line.contains('\0'),
                "control byte leaked into row {row}: {line:?}"
            );
            assert!(
                cell_width::text_width(line) <= 107,
                "row {row} overflowed transcript width: {line:?}"
            );
        }
    }

    #[test]
    fn thinking_peek_renders_full_block() {
        let content = concat!(
            "first\n",
            "middle line that should remain visible in presentation-state peek\n",
            "tail words tail words tail words tail words tail words tail words tail words tail words tail words"
        );
        let rows = layout_block_test(
            &thinking(content),
            None,
            &LayoutContext::new(32, ViewState::Peek),
        );

        let expanded_rows = layout_block_test(
            &thinking(content),
            None,
            &LayoutContext::new(32, ViewState::Expanded),
        );

        assert_eq!(rows.len(), expanded_rows.len());
        assert_eq!(rows[0].text, "│ first");
        assert!(rows.iter().all(|row| row.text.starts_with("│ ")));
        assert!(rows.iter().any(|row| row.text.contains("middle line")));
    }

    #[test]
    fn thinking_peek_does_not_duplicate_short_blocks() {
        let rows = layout_block_test(
            &thinking("one\ntwo\nthree"),
            None,
            &LayoutContext::new(80, ViewState::Peek),
        );
        let text: Vec<_> = rows.into_iter().map(|row| row.text).collect();
        assert_eq!(text, vec!["│ one", "│ two", "│ three"]);
    }

    #[test]
    fn thinking_summary_extracts_bold_title() {
        let (label, lines) =
            thinking_summary("**Analyzing the bug**\nLet me check...\n\nMore notes");
        assert_eq!(label, "Analyzing the bug");
        assert_eq!(lines, 3);
    }

    #[test]
    fn thinking_summary_falls_back_to_default() {
        let (label, lines) = thinking_summary("Let me think about this.\nLine two.");
        assert_eq!(label, "thinking");
        assert_eq!(lines, 2);
    }

    #[test]
    fn thinking_summary_skips_blank_lines() {
        let (_, lines) = thinking_summary("\n\nfirst\n\nsecond\n\n");
        assert_eq!(lines, 2);
    }

    #[test]
    fn thinking_summary_empty() {
        let (label, lines) = thinking_summary("");
        assert_eq!(label, "thinking");
        assert_eq!(lines, 0);
    }

    #[test]
    fn thinking_summary_bold_must_have_content() {
        // "****" is 4 chars - the `len() > 4` check rejects empty bold
        let (label, _) = thinking_summary("****");
        assert_eq!(label, "thinking");
    }

    /// Snapshot the leftmost cells of every block variant against the shared
    /// spacing constants. This is the regression guard against block renderers
    /// drifting back to ad-hoc `"  "` / `"│ "` literals.
    #[test]
    fn leftmost_padding_matches_shared_constants() {
        let chrome_pad: String = " ".repeat(CHROME_INNER_PAD);
        let ctx = LayoutContext {
            width: W as u16,
            view_state: ViewState::Expanded,
        };
        let render = |b: &Block, st: Option<&ToolState>| layout_block_test(b, st, &ctx);

        // User: content rows in the chrome panel start with the chrome pad.
        // Blank padding rows carry row-fill metadata instead of fake text.
        let lines = render(&user("hello"), None);
        for line in lines.iter().filter(|line| !line.text.is_empty()) {
            assert!(
                line.text.starts_with(&chrome_pad),
                "user row missing chrome pad: {:?}",
                line.text
            );
        }

        // Exec command (no output): content rows start with chrome pad.
        let exec_no_output = Block::Exec {
            command: "ls".into(),
            output: String::new().into(),
        };
        let lines = render(&exec_no_output, None);
        for line in lines.iter().filter(|line| !line.text.is_empty()) {
            assert!(
                line.text.starts_with(&chrome_pad),
                "exec chrome row missing pad: {:?}",
                line.text
            );
        }

        // Exec output: bottom rows sit under the block gutter.
        let exec_with_output = Block::Exec {
            command: "echo hi".into(),
            output: "hello\nworld".into(),
        };
        let lines = render(&exec_with_output, None);
        let output_tail = &lines[lines.len() - 2..];
        for line in output_tail {
            assert!(
                line.text.starts_with(BLOCK_GUTTER_SPACE),
                "exec output row missing block gutter: {:?}",
                line.text
            );
        }

        // Thinking expanded: every row prefixed with the thinking gutter.
        let lines = layout_block_test(&thinking("**title**\nbody line"), None, &ctx);
        for line in &lines {
            assert!(
                line.text.starts_with(THINKING_GUTTER),
                "thinking expanded row missing gutter: {:?}",
                line.text
            );
        }

        // Text/markdown: renderer emits no left indent.
        let lines = render(&text("hello world"), None);
        for line in &lines {
            assert!(
                !line.text.starts_with(' '),
                "text row should be flush-left: {:?}",
                line.text
            );
        }

        // CodeLine: renderer emits no left indent (window-level line-number
        // gutter is applied at paint time, outside the block buffer).
        let code = Block::CodeLine {
            content: "fn main() {}".into(),
            lang: "rust".into(),
        };
        let lines = render(&code, None);
        for line in &lines {
            assert!(
                !line.text.starts_with(' '),
                "code line row should be flush-left: {:?}",
                line.text
            );
        }

        // Compacted: starts with a horizontal rule glyph (no indent).
        let lines = render(
            &Block::Compacted {
                summary: "ok".into(),
                kept: Vec::new(),
            },
            None,
        );
        assert!(
            lines[0].text.starts_with('\u{2500}'),
            "compacted row should start with ─: {:?}",
            lines[0].text
        );
    }
}
