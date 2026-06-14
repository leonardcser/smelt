//! Per-`Block`-variant transcript renderers.

pub mod markdown;
mod tools;

mod chrome;
pub(in crate::content) mod compacted;
pub(in crate::content) mod exec;
mod metrics;
pub(in crate::content) mod mode;
pub(in crate::content) mod process_status;
pub(in crate::content) mod text;
pub(in crate::content) mod thinking;
pub(in crate::content) mod tool_call;
pub(in crate::content) mod user;

#[cfg(test)]
use markdown::is_horizontal_rule;
pub use markdown::render_markdown_inner;
pub(in crate::content) use tools::measure_tool_height;
pub use tools::render_tool_body_into;

/// Per-tool row cap (applied to command header and output body separately).
const MAX_TOOL_BLOCK_ROWS: usize = 20;

#[cfg(test)]
mod tests {
    use super::thinking::thinking_summary;
    use super::*;
    use crate::content::display_block::{compile_block, render_block_into, RenderCtx};
    use smelt_core::buffer::{BufCreateOpts, BufId, Buffer};
    use smelt_core::content::builder::test_util::{read_buffer, TestLine};
    use smelt_core::content::LayoutContext;
    use smelt_core::theme::Theme;
    use smelt_core::transcript_model::{gap_between, Block, ToolState, ToolStatus, ViewState};
    use std::collections::HashMap;

    const W: usize = 80;

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
        state: Option<&ToolState>,
        ctx: LayoutContext,
    ) -> u16 {
        let display = compile_block(block, state);
        render_block_into(
            buf,
            &display,
            RenderCtx {
                width: ctx.width,
                show_thinking: ctx.show_thinking,
                view_state: ctx.view_state,
                theme,
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
        let rows = render_block_test_into(&mut buf, &theme, block, state, *ctx) as usize;
        read_buffer(&buf, &theme, rows)
    }

    fn text(s: &str) -> Block {
        Block::Text {
            content: s.to_string(),
        }
    }

    fn user(s: &str) -> Block {
        Block::User {
            text: s.to_string(),
            image_labels: vec![],
        }
    }

    fn thinking(s: &str) -> Block {
        Block::Thinking {
            content: s.to_string(),
        }
    }

    fn empty_tool_call() -> Block {
        Block::ToolCall {
            call_id: String::new(),
            name: String::new(),
            summary: protocol::StyledLines::default(),
            args: HashMap::new(),
        }
    }

    fn tool_call() -> Block {
        let mut args = HashMap::new();
        args.insert("command".into(), serde_json::Value::String("ls".into()));
        Block::ToolCall {
            call_id: "call-1".into(),
            name: "bash".into(),
            summary: protocol::StyledLines::from_plain("ls"),
            args,
        }
    }

    fn pending_tool_state() -> ToolState {
        ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
            body: None,
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
            LayoutContext::new(W as u16, true, ViewState::Expanded),
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
                    LayoutContext::new(W as u16, true, ViewState::Expanded),
                )
            };
            total += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (total, tg, total + tg)
    }

    fn render_split(blocks: &[Block]) -> (u16, u16, u16) {
        let (mut buf, theme) = mk_collector_buf();
        let mut block_rows_total = 0u16;
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
                    LayoutContext::new(W as u16, true, ViewState::Expanded),
                )
            };
            block_rows_total += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (block_rows_total, tg, block_rows_total + tg)
    }

    fn render_incremental(blocks: &[Block]) -> (u16, u16, u16) {
        let (mut buf, theme) = mk_collector_buf();
        let mut cumulative = 0u16;
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
                    LayoutContext::new(W as u16, true, ViewState::Expanded),
                )
            };
            cumulative += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (cumulative, tg, cumulative + tg)
    }

    #[test]
    fn text_then_tool_all_at_once() {
        let blocks = vec![user("hello"), text("I'll check that.")];
        let (_, tg, _) = render_all_at_once(&blocks);
        assert_eq!(tg, 1, "exactly 1 gap row between Text and ActiveTool");
    }

    #[test]
    fn text_then_tool_split() {
        let blocks = vec![user("hello"), text("I'll check that.")];
        let (_, tg, _) = render_split(&blocks);
        assert_eq!(
            tg, 1,
            "exactly 1 gap row between Text and ActiveTool (split)"
        );
    }

    #[test]
    fn all_paths_produce_same_total() {
        let blocks = vec![user("hello"), text("I'll check that.")];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        let c = render_incremental(&blocks);
        assert_eq!(a.2, b.2, "all-at-once vs split total must match");
        assert_eq!(b.2, c.2, "split vs incremental total must match");
    }

    #[test]
    fn thinking_text_tool_all_paths_match() {
        let blocks = vec![
            user("fix the bug"),
            thinking("Let me analyze..."),
            text("I'll fix it now."),
        ];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        let c = render_incremental(&blocks);
        assert_eq!(a.2, b.2, "all-at-once vs split");
        assert_eq!(b.2, c.2, "split vs incremental");
        assert_eq!(a.1, 1, "tool gap = 1");
    }

    #[test]
    fn empty_thinking_text_tool() {
        let blocks = vec![user("fix it"), thinking(""), text("Here's the fix.")];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);

        let thinking_rows = block_rows(&thinking(""));
        assert_eq!(thinking_rows, 0);

        let user_thinking_gap = gap_between(&user("fix it"), &thinking(""));
        let thinking_text_gap = gap_between(&thinking(""), &text("Here's the fix."));
        assert_eq!(user_thinking_gap, 1);
        assert_eq!(thinking_text_gap, 1);

        assert_eq!(a.1, 1, "tool gap after text = 1");
        assert_eq!(a.2, b.2, "paths match with empty thinking");
    }

    #[test]
    fn text_with_internal_blank_line() {
        // Text with internal blank line: "para1\n\npara2"
        let blocks = vec![user("hello"), text("para1\n\npara2")];
        let rows = block_rows(&text("para1\n\npara2"));
        assert_eq!(rows, 3, "3 rows: para1, blank, para2");

        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        assert_eq!(a.1, 1, "tool gap still 1");
        assert_eq!(a.2, b.2);
    }

    #[test]
    fn tool_call_then_text_then_tool() {
        // Multi-tool turn: first tool finished, then new text + new tool.
        let blocks = vec![
            user("do two things"),
            text("First task:"),
            tool_call(),
            text("Second task:"),
        ];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        assert_eq!(a.1, 1);
        assert_eq!(a.2, b.2);
    }

    #[test]
    fn empty_text_before_tool() {
        let blocks = vec![user("hello"), text("")];
        let rows = block_rows(&text(""));
        assert_eq!(rows, 0, "empty text renders 0 rows");

        let gap = gap_between(&text(""), &empty_tool_call());
        assert_eq!(gap, 1, "gap is still 1 for empty text block");

        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        assert_eq!(a.2, b.2);

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
    fn adjacent_text_blocks_gap() {
        // Two consecutive text blocks - gap should be 1 (paragraph spacing).
        let gap = gap_between(&text("a"), &text("b"));
        assert_eq!(gap, 1, "Text→Text gap = 1");
    }

    fn tool_start_row(blocks: &[Block], flushed_at: &[usize]) -> u16 {
        let mut anchor: u16 = 0;
        let mut flushed: usize = 0;

        for &end in flushed_at {
            // This frame renders blocks[flushed..end]
            let mut frame_block_rows = 0u16;
            let (mut buf, theme) = mk_collector_buf();
            for i in flushed..end {
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
                        LayoutContext::new(W as u16, true, ViewState::Expanded),
                    )
                };
                frame_block_rows += gap + rows;
            }
            anchor += frame_block_rows;
            flushed = end;
        }

        let tg = tool_gap_for(blocks);
        anchor + tg
    }

    #[test]
    fn anchor_tracking_single_frame() {
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[2]);

        let user_rows = block_rows(&user("hello"));
        let text_rows = block_rows(&text("response"));
        let expected = user_rows + 1 + text_rows + 1;
        assert_eq!(row, expected);
    }

    #[test]
    fn anchor_tracking_split_frames() {
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[1, 2]);

        let user_rows = block_rows(&user("hello"));
        let text_rows = block_rows(&text("response"));
        let expected = user_rows + 1 + text_rows + 1;
        assert_eq!(row, expected);
    }

    #[test]
    fn anchor_tracking_each_block_separate() {
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[1, 2]);
        let single = tool_start_row(&blocks, &[2]);
        assert_eq!(row, single);
    }

    #[test]
    fn anchor_tracking_with_empty_thinking() {
        let blocks = vec![user("hi"), thinking(""), text("fix")];

        let single = tool_start_row(&blocks, &[3]);
        let split = tool_start_row(&blocks, &[1, 2, 3]);
        assert_eq!(single, split);

        let blocks_no_thinking = vec![user("hi"), text("fix")];
        let no_thinking = tool_start_row(&blocks_no_thinking, &[2]);
        assert_eq!(single - no_thinking, 1, "empty thinking adds 1 extra row");
    }

    #[test]
    fn anchor_tracking_with_thinking() {
        let blocks = vec![user("hi"), thinking("let me think"), text("fix")];

        let single = tool_start_row(&blocks, &[3]);
        let split_2 = tool_start_row(&blocks, &[1, 3]);
        let split_3 = tool_start_row(&blocks, &[1, 2, 3]);
        assert_eq!(single, split_2, "single vs 2-split");
        assert_eq!(single, split_3, "single vs 3-split");
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
    fn exec_output_carriage_return_renders_latest_status_line() {
        let rows = layout_block_test(
            &Block::Exec {
                command: "git rebase main".into(),
                output: "Rebasing (1/1)\r\x1b[KSuccessfully rebased and updated refs/heads/topic."
                    .into(),
            },
            None,
            &LayoutContext::new(W as u16, true, ViewState::default()),
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
    fn horizontal_rule_detection() {
        assert!(is_horizontal_rule("---"));
        assert!(is_horizontal_rule("___"));
        assert!(is_horizontal_rule("***"));
        assert!(is_horizontal_rule("------"));
        assert!(is_horizontal_rule("-----"));
        assert!(is_horizontal_rule(" - - - "));
        assert!(is_horizontal_rule(" * * * "));
        assert!(is_horizontal_rule(" _ _ _ "));
        assert!(is_horizontal_rule("  ---  "));

        assert!(!is_horizontal_rule("--"));
        assert!(!is_horizontal_rule("-"));
        assert!(!is_horizontal_rule(""));
        assert!(!is_horizontal_rule("text"));
        assert!(!is_horizontal_rule("- -"));
        assert!(!is_horizontal_rule("-*-*-*"));
        assert!(!is_horizontal_rule("---a"));
        assert!(!is_horizontal_rule("123"));
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

    #[test]
    fn bash_tool_layout_sets_source_text_and_soft_wrap() {
        let mut args = HashMap::new();
        args.insert(
            "command".into(),
            serde_json::Value::String("echo hello && echo world && echo done".into()),
        );
        let block = Block::ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            summary: "echo hello && echo world && echo done".into(),
            args,
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: Some(std::time::Duration::from_secs(1)),
            output: None,
            user_message: None,
            body: None,
        };
        let ctx = LayoutContext {
            width: 30,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx);

        assert!(
            display.len() >= 2,
            "command should wrap at width 30, got {} lines",
            display.len()
        );
        assert_eq!(
            display[0].source_text.as_deref(),
            Some("* bash echo hello && echo world && echo done"),
        );
        assert!(!display[0].soft_wrapped);
        for line in &display[1..] {
            assert!(line.source_text.is_none());
            assert!(line.soft_wrapped);
        }
    }

    #[test]
    fn bash_tool_multiline_command_only_wraps_mark_soft() {
        let mut args = HashMap::new();
        args.insert(
            "command".into(),
            serde_json::Value::String("echo one\necho two".into()),
        );
        let block = Block::ToolCall {
            call_id: "c2".into(),
            name: "bash".into(),
            summary: "echo one\necho two".into(),
            args,
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: None,
            user_message: None,
            body: None,
        };
        let ctx = LayoutContext {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx);

        assert!(display.len() >= 2);
        assert!(!display[0].soft_wrapped);
        assert!(!display[1].soft_wrapped);
    }

    #[test]
    fn tool_title_chrome_renders_elapsed_after_completion() {
        let summary = protocol::StyledLines(vec![vec![
            protocol::StyledSpan {
                text: "echo hi".into(),
                syntax: Some("bash".into()),
                ..Default::default()
            },
            protocol::StyledSpan {
                text: "(timeout: 2m)".into(),
                selectable: false,
                title_suffix: true,
                ..Default::default()
            },
        ]]);
        let block = Block::ToolCall {
            call_id: "c-title-chrome".into(),
            name: "bash".into(),
            summary,
            args: HashMap::new(),
        };
        let ctx = LayoutContext {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };

        let pending = ToolState {
            status: ToolStatus::Pending,
            elapsed: Some(std::time::Duration::from_secs(2)),
            output: None,
            user_message: None,
            body: None,
        };
        let pending_display = layout_block_test(&block, Some(&pending), &ctx);
        assert!(pending_display[0]
            .text
            .contains("echo hi  2s (timeout: 2m)"));

        let done = ToolState {
            status: ToolStatus::Ok,
            elapsed: Some(std::time::Duration::from_secs(2)),
            output: None,
            user_message: None,
            body: None,
        };
        let done_display = layout_block_test(&block, Some(&done), &ctx);
        assert!(done_display[0].text.contains("echo hi  2s"));
        assert!(!done_display[0].text.contains("timeout"));

        let failed = ToolState {
            status: ToolStatus::Err,
            elapsed: Some(std::time::Duration::from_secs(65)),
            output: None,
            user_message: None,
            body: None,
        };
        let failed_display = layout_block_test(&block, Some(&failed), &ctx);
        assert!(failed_display[0].text.contains("echo hi  1m5s"));
        assert!(!failed_display[0].text.contains("timeout"));
    }

    #[test]
    fn tool_timer_waits_until_one_second_and_formats_coarsely() {
        let block = Block::ToolCall {
            call_id: "c-short-timer".into(),
            name: "bash".into(),
            summary: protocol::StyledLines::from_plain("echo hi"),
            args: HashMap::new(),
        };
        let ctx = LayoutContext {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let render_elapsed = |elapsed| {
            let state = ToolState {
                status: ToolStatus::Ok,
                elapsed: Some(elapsed),
                output: None,
                user_message: None,
                body: None,
            };
            layout_block_test(&block, Some(&state), &ctx)[0]
                .text
                .clone()
        };

        let under_one = render_elapsed(std::time::Duration::from_millis(999));
        assert!(!under_one.contains("0."));
        assert!(!under_one.contains("  0s"));
        assert!(render_elapsed(std::time::Duration::from_secs(59)).contains("  59s"));
        assert!(render_elapsed(std::time::Duration::from_secs(60)).contains("  1m0s"));
        assert!(render_elapsed(std::time::Duration::from_secs(65)).contains("  1m5s"));
        assert!(render_elapsed(std::time::Duration::from_secs(3660)).contains("  1h1m"));
    }

    /// Width-independent tool bodies are precomputed on the main thread and
    /// stashed on `ToolState.body`; this test asserts that when a body is present,
    /// its content reaches the transcript and `output.content` does not.
    #[test]
    fn tool_body_replaces_raw_output() {
        use smelt_core::content::block_layout::{BlockLayout, IrLeaf, TextSpec, ToolBody};
        use smelt_core::transcript_model::ToolOutput;

        let body = ToolBody::Layout(BlockLayout::Leaf(IrLeaf::Text(TextSpec {
            content: "fn main() {\n    println!(\"hi\");".into(),
            hl_group: None,
        })));

        let mut args = HashMap::new();
        args.insert(
            "file_path".into(),
            serde_json::Value::String("hello.rs".into()),
        );
        let block = Block::ToolCall {
            call_id: "c-render-cache".into(),
            name: "write_file".into(),
            summary: protocol::StyledLines::from_plain("hello.rs"),
            args,
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: "RAW_FALLBACK_TEXT".into(),
                is_error: false,
                metadata: None,
            })),
            user_message: None,
            body: Some(body),
        };
        let ctx = LayoutContext {
            width: W as u16,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx);
        let joined: String = display
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            joined.contains("fn main() {"),
            "cached layout content should reach the transcript, got: {joined:?}"
        );
        assert!(
            !joined.contains("RAW_FALLBACK_TEXT"),
            "raw output.content must not leak when body is present, got: {joined:?}"
        );
    }

    #[test]
    fn edit_file_diff_body_column_stays_stable_across_completion() {
        use smelt_core::content::block_layout::ToolBody;
        use smelt_core::transcript_model::ToolOutput;

        fn render_insert_column(body: ToolBody, status: ToolStatus) -> usize {
            let block = Block::ToolCall {
                call_id: "c-edit-shift".into(),
                name: "edit_file".into(),
                summary: protocol::StyledLines::from_plain("a.txt"),
                args: HashMap::new(),
            };
            let state = ToolState {
                status,
                elapsed: None,
                output: None,
                user_message: None,
                body: Some(body),
            };
            let ctx = LayoutContext {
                width: W as u16,
                show_thinking: true,
                view_state: ViewState::Expanded,
            };
            layout_block_test(&block, Some(&state), &ctx)
                .into_iter()
                .find_map(|line| line.text.find("inserted line"))
                .expect("diff should render the inserted line")
        }

        fn edit_file_layout(
            app: &mut crate::app::test_harness::TestApp,
            args: &HashMap<String, serde_json::Value>,
            output: Option<&ToolOutput>,
            status: &'static str,
        ) -> ToolBody {
            let ctx = smelt_core::lua::runtime::ToolRenderCtx {
                summary: "",
                status,
                elapsed_secs: None,
                call_id: Some("c-edit-shift"),
            };
            let layout = app
                .app
                .lua
                .render_tool_layout("edit_file", args, output, ctx)
                .expect("edit_file should render a layout");
            crate::app::transcript::compile_tool_body(&layout)
                .expect("edit_file body should compile")
        }

        fn layout_text(body: ToolBody) -> String {
            let block = Block::ToolCall {
                call_id: "c-edit-shift".into(),
                name: "edit_file".into(),
                summary: protocol::StyledLines::from_plain("a.txt"),
                args: HashMap::new(),
            };
            let state = ToolState {
                status: ToolStatus::Pending,
                elapsed: None,
                output: None,
                user_message: None,
                body: Some(body),
            };
            let ctx = LayoutContext {
                width: W as u16,
                show_thinking: true,
                view_state: ViewState::Expanded,
            };
            layout_block_test(&block, Some(&state), &ctx)
                .into_iter()
                .map(|line| line.text)
                .collect::<Vec<_>>()
                .join("\n")
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let path_str = path.to_str().unwrap().to_string();
        let before: String = (1..=9).map(|i| format!("line {i}\n")).collect();
        let after = before.replacen("line 1\n", "line 1\ninserted line\n", 1);
        std::fs::write(&path, &before).unwrap();

        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut args = HashMap::new();
        args.insert(
            "file_path".into(),
            serde_json::Value::String(path_str.clone()),
        );
        args.insert(
            "old_string".into(),
            serde_json::Value::String("line 1\n".into()),
        );
        args.insert(
            "new_string".into(),
            serde_json::Value::String("line 1\ninserted line\n".into()),
        );

        let pending_layout = edit_file_layout(&mut app, &args, None, "pending");
        assert!(
            layout_text(pending_layout.clone()).contains("inserted line"),
            "pending edit_file render should show the planned replacement without blocking on a disk read"
        );

        std::fs::write(&path, &after).unwrap();
        let output = ToolOutput {
            content: "edited a.txt".into(),
            is_error: false,
            metadata: Some(serde_json::json!({
                "old_content": before,
                "new_content": after,
                "path": path_str,
            })),
        };
        let finished_layout = edit_file_layout(&mut app, &args, Some(&output), "ok");

        let pending_col = render_insert_column(pending_layout, ToolStatus::Pending);
        let finished_col = render_insert_column(finished_layout, ToolStatus::Ok);
        assert_eq!(pending_col, finished_col);
    }

    #[test]
    fn tool_body_compile_rejects_buffer_leaf_layouts() {
        use smelt_core::buffer::BufId;
        use smelt_core::content::block_layout::{BlockLayout, LuaLeaf, TextSpec};

        let layout = BlockLayout::Vbox(vec![
            BlockLayout::Leaf(LuaLeaf::Text(TextSpec {
                content: "kept".into(),
                hl_group: None,
            })),
            BlockLayout::Leaf(LuaLeaf::Buf(BufId(99))),
        ]);

        let err = crate::app::transcript::compile_tool_body(&layout)
            .expect_err("buffer leaves should reject the whole body");
        assert!(err.contains("layout.leaf"), "unexpected error: {err}");
    }

    #[test]
    fn tool_body_diff_ir_respects_tool_row_cap() {
        use smelt_core::content::block_layout::{BlockLayout, IrLeaf, ToolBody};
        use smelt_core::transcript_model::ToolOutput;

        let content = (0..80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let ir = smelt_core::content::highlight::build_file_view_ir(&content, Some("txt"));
        let block = Block::ToolCall {
            call_id: "c-cap".into(),
            name: "write_file".into(),
            summary: protocol::StyledLines::from_plain("x.txt"),
            args: HashMap::new(),
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: "FALLBACK".into(),
                is_error: false,
                metadata: None,
            })),
            user_message: None,
            body: Some(ToolBody::Layout(BlockLayout::Leaf(IrLeaf::DiffIr(ir)))),
        };
        let ctx = LayoutContext {
            width: W as u16,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx);
        assert_eq!(display.len(), 1 + MAX_TOOL_BLOCK_ROWS);
    }

    #[test]
    fn tool_body_ir_replaces_output_fallback() {
        use smelt_core::content::block_layout::{BlockLayout, IrLeaf, ToolBody};
        use smelt_core::transcript_model::ToolOutput;

        let ir = smelt_core::content::highlight::build_file_view_ir("IR_LAYOUT\n", Some("txt"));
        let body = ToolBody::Layout(BlockLayout::Leaf(IrLeaf::DiffIr(ir)));

        let block = Block::ToolCall {
            call_id: "c-ir".into(),
            name: "write_file".into(),
            summary: protocol::StyledLines::from_plain("x.rs"),
            args: HashMap::new(),
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: "FALLBACK".into(),
                is_error: false,
                metadata: None,
            })),
            user_message: None,
            body: Some(body),
        };
        let ctx = LayoutContext {
            width: W as u16,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx);
        let joined: String = display
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            joined.contains("IR_LAYOUT"),
            "tool body should render, got: {joined:?}"
        );
        assert!(
            !joined.contains("FALLBACK"),
            "tool body should replace output fallback, got: {joined:?}"
        );
    }

    #[test]
    fn tool_without_body_falls_back_to_output() {
        use smelt_core::transcript_model::ToolOutput;

        let block = Block::ToolCall {
            call_id: "c-fallback".into(),
            name: "write_file".into(),
            summary: protocol::StyledLines::from_plain("x.rs"),
            args: HashMap::new(),
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: Some(Box::new(ToolOutput {
                content: "FALLBACK".into(),
                is_error: false,
                metadata: None,
            })),
            user_message: None,
            body: None,
        };
        let ctx = LayoutContext {
            width: W as u16,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx);
        let joined: String = display
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.text.as_str()))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            joined.contains("FALLBACK"),
            "expected fallback to output.content without body, got: {joined:?}"
        );
    }

    #[test]
    fn bash_tool_time_suffix_is_non_selectable() {
        let mut args = HashMap::new();
        args.insert("command".into(), serde_json::Value::String("ls".into()));
        let block = Block::ToolCall {
            call_id: "c3".into(),
            name: "bash".into(),
            summary: "ls".into(),
            args,
        };
        let state = ToolState {
            status: ToolStatus::Pending,
            elapsed: Some(std::time::Duration::from_secs(3)),
            output: None,
            user_message: None,
            body: None,
        };
        let ctx = LayoutContext {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx);
        let first_line = &display[0];
        let has_non_selectable_time = first_line
            .spans
            .iter()
            .any(|span| !span.meta.selectable && span.text.contains("3s"));
        assert!(has_non_selectable_time);
    }

    /// Snapshot the leftmost cells of every block variant against the values
    /// declared in `metrics.rs`. This is the regression guard against block
    /// renderers drifting back to ad-hoc `"  "` / `"│ "` literals.
    #[test]
    fn leftmost_padding_matches_metrics() {
        use super::metrics::{BLOCK_GUTTER_SPACE, CHROME_INNER_PAD, THINKING_GUTTER};
        use smelt_core::transcript_model::ToolState;

        let chrome_pad: String = " ".repeat(CHROME_INNER_PAD);
        let ctx = LayoutContext {
            width: W as u16,
            show_thinking: false,
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
            output: String::new(),
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
        let expanded_ctx = LayoutContext {
            show_thinking: true,
            ..ctx
        };
        let lines = layout_block_test(&thinking("**title**\nbody line"), None, &expanded_ctx);
        for line in &lines {
            assert!(
                line.text.starts_with(THINKING_GUTTER),
                "thinking expanded row missing gutter: {:?}",
                line.text
            );
        }

        // Thinking collapsed (one summary row).
        let lines = render(&thinking("**title**\nbody"), None);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.starts_with(THINKING_GUTTER));

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

        // ToolCall header begins with the `*` pill glyph (no indent).
        let tc = tool_call();
        let state = pending_tool_state();
        let lines = render(&tc, Some(&state));
        assert!(
            lines[0].text.starts_with('*'),
            "tool header should start with pill '*': {:?}",
            lines[0].text
        );

        // ToolCall with a user_message: that row sits under the block gutter.
        let denied = ToolState {
            status: ToolStatus::Denied,
            user_message: Some("nope".into()),
            ..pending_tool_state()
        };
        let lines = render(&tc, Some(&denied));
        assert!(
            lines[1].text.starts_with(BLOCK_GUTTER_SPACE),
            "tool user_message row missing block gutter: {:?}",
            lines[1].text
        );

        // Compacted: starts with a horizontal rule glyph (no indent).
        let lines = render(
            &Block::Compacted {
                summary: "ok".into(),
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
