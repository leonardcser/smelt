//! Dispatcher for per-`Block`-variant renderers. `layout_block_into` is the entry point;
//! `render_block` fans out to per-variant files and `apply_view_state` collapses the result.

use smelt_core::buffer::Buffer;
use smelt_core::content::builder::{LineBuilder, Outcome};
use smelt_core::content::LayoutContext;
use smelt_core::theme::intern;
use smelt_core::theme::Theme;
use smelt_core::transcript_model::{Block, ToolState, ViewState};

pub mod markdown;
mod tools;

mod chrome;
mod code_line;
mod compacted;
mod exec;
mod text;
mod thinking;
mod tool_call;
mod user;

#[cfg(test)]
use markdown::is_horizontal_rule;
pub use markdown::render_markdown_inner;
pub use thinking::{render_thinking_summary, thinking_summary};
pub use tools::render_layout_into;
pub use user::UserBlockGeometry;

/// Per-tool row cap (applied to command header and output body separately).
const MAX_TOOL_BLOCK_ROWS: usize = 20;

/// Render `block` into `buf`, then apply its view state. `state` is required for `ToolCall`.
pub fn layout_block_into(
    buf: &mut Buffer,
    theme: &Theme,
    block: &Block,
    state: Option<&ToolState>,
    ctx: &LayoutContext,
) -> Outcome {
    let width = ctx.width as usize;
    let show_thinking = ctx.show_thinking;
    let outcome = {
        let mut col = LineBuilder::new(buf, theme, ctx.width);
        render_block(&mut col, block, state, width, show_thinking);
        col.finish()
    };
    apply_view_state(buf, theme, ctx.width, ctx.view_state, outcome)
}

fn apply_view_state(
    buf: &mut Buffer,
    theme: &Theme,
    width: u16,
    state: ViewState,
    outcome: Outcome,
) -> Outcome {
    let total = outcome.line_count;
    let start = buf.line_count().saturating_sub(total);
    match state {
        ViewState::Expanded => outcome,
        ViewState::Collapsed => {
            if total > 1 {
                let hidden = total - 1;
                // Keep first line, drop the rest.
                buf.set_lines(start + 1, start + total, vec![]);
                let after_truncate_outcome = Outcome {
                    line_count: 1,
                    ..outcome
                };
                append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines"),
                    after_truncate_outcome,
                )
            } else {
                outcome
            }
        }
        ViewState::TrimmedHead { keep } => {
            let keep = keep as usize;
            if total > keep {
                let hidden = total - keep;
                buf.set_lines(start + keep, start + total, vec![]);
                let after_truncate_outcome = Outcome {
                    line_count: keep,
                    ..outcome
                };
                append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines"),
                    after_truncate_outcome,
                )
            } else {
                outcome
            }
        }
        ViewState::TrimmedTail { keep } => {
            let keep = keep as usize;
            if total > keep {
                let hidden = total - keep;
                buf.set_lines(start, start + (total - keep), vec![]);
                let mut kept_lines: Vec<String> = (0..keep)
                    .map(|i| buf.get_line(start + i).unwrap_or("").to_string())
                    .collect();
                let kept_decorations: Vec<_> = (0..keep)
                    .map(|i| buf.decoration_at(start + i).clone())
                    .collect();
                let kept_highlights: Vec<_> =
                    (0..keep).map(|i| buf.highlights_at(start + i)).collect();
                buf.set_lines(start, start + keep, vec![]);
                let after_ellipsis_outcome = append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines above"),
                    Outcome {
                        line_count: 0,
                        ..outcome
                    },
                );
                let cur_len = buf.line_count();
                buf.set_lines(cur_len, cur_len, std::mem::take(&mut kept_lines));
                for (i, hl_list) in kept_highlights.into_iter().enumerate() {
                    let row = cur_len + i;
                    for span in hl_list {
                        buf.add_highlight_group_with_meta(
                            row,
                            span.col_start,
                            span.col_end,
                            span.hl,
                            span.meta,
                        );
                    }
                }
                for (i, dec) in kept_decorations.into_iter().enumerate() {
                    if dec != smelt_core::buffer::LineDecoration::default() {
                        buf.set_decoration(cur_len + i, dec);
                    }
                }
                Outcome {
                    line_count: after_ellipsis_outcome.line_count + keep,
                    ..outcome
                }
            } else {
                outcome
            }
        }
    }
}

fn append_ellipsis(
    buf: &mut Buffer,
    theme: &Theme,
    width: u16,
    text: &str,
    outcome: Outcome,
) -> Outcome {
    let added = {
        let mut col = LineBuilder::new(buf, theme, width);
        col.push_dim();
        col.push_hl(intern("Comment"));
        col.print(text);
        col.pop_style();
        col.pop_style();
        col.newline();
        col.finish()
    };
    Outcome {
        line_count: outcome.line_count + added.line_count,
        was_wrapped: outcome.was_wrapped || added.was_wrapped,
        max_line_width: outcome.max_line_width.max(added.max_line_width),
        layout_width: outcome.layout_width,
    }
}

pub(super) fn render_block(
    out: &mut LineBuilder,
    block: &Block,
    state: Option<&ToolState>,
    width: usize,
    show_thinking: bool,
) -> u16 {
    let _perf = smelt_perf::perf::begin(match block {
        Block::User { .. } => "render:user",
        Block::Thinking { .. } => "render:thinking",
        Block::Text { .. } => "render:text",
        Block::CodeLine { .. } => "render:code_line",
        Block::ToolCall { .. } => "render:tool_call",
        Block::Compacted { .. } => "render:compacted",
        Block::Exec { .. } => "render:exec",
    });
    match block {
        Block::User { text, image_labels } => user::render(out, text, image_labels, width),
        Block::Thinking { content } => thinking::render(out, content, width, show_thinking),
        Block::Text { content } => text::render(out, content, width),
        Block::CodeLine { content, lang } => code_line::render(out, content, lang, width),
        Block::ToolCall {
            call_id,
            name,
            summary,
            args,
        } => {
            let state = state.expect("ToolCall layout requires ToolState");
            tool_call::render(
                out,
                call_id,
                name,
                summary,
                args,
                state.status,
                state.elapsed,
                state,
                width,
            )
        }
        Block::Compacted { summary } => compacted::render(out, summary, width),
        Block::Exec { command, output } => exec::render(out, command, output, width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::buffer::{BufCreateOpts, BufId, Buffer};
    use smelt_core::content::builder::test_util::{read_buffer, TestLine};
    use smelt_core::content::builder::LineBuilder;
    use smelt_core::theme::Theme;
    use smelt_core::transcript_model::{gap_between, ToolStatus};
    use std::collections::HashMap;

    const W: usize = 80;

    fn mk_collector_buf() -> (Buffer, Theme) {
        (
            Buffer::new(BufId(0), BufCreateOpts::default()),
            Theme::default(),
        )
    }

    fn layout_block_test(
        block: &Block,
        state: Option<&ToolState>,
        ctx: &LayoutContext,
    ) -> Vec<TestLine> {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let outcome = layout_block_into(&mut buf, &theme, block, state, ctx);
        read_buffer(&buf, &theme, outcome.line_count)
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
            render_cache: None,
        }
    }

    fn state_for(block: &Block) -> Option<ToolState> {
        matches!(block, Block::ToolCall { .. }).then(pending_tool_state)
    }

    fn block_rows(block: &Block) -> u16 {
        let (mut buf, theme) = mk_collector_buf();
        let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
        let st = state_for(block);
        render_block(&mut out, block, st.as_ref(), W, true)
    }

    fn tool_gap_for(blocks: &[Block]) -> u16 {
        blocks
            .last()
            .map(|b| gap_between(b, &empty_tool_call()))
            .unwrap_or(0)
    }

    fn render_all_at_once(blocks: &[Block]) -> (u16, u16, u16) {
        let (mut buf, theme) = mk_collector_buf();
        let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
        let mut total = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&blocks[i - 1], &blocks[i])
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true)
            };
            total += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (total, tg, total + tg)
    }

    fn render_split(blocks: &[Block]) -> (u16, u16, u16) {
        let (mut buf, theme) = mk_collector_buf();
        let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
        let mut block_rows_total = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&blocks[i - 1], &blocks[i])
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true)
            };
            block_rows_total += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (block_rows_total, tg, block_rows_total + tg)
    }

    fn render_incremental(blocks: &[Block]) -> (u16, u16, u16) {
        let (mut buf, theme) = mk_collector_buf();
        let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
        let mut cumulative = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&blocks[i - 1], &blocks[i])
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true)
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
        // Two consecutive text blocks — gap should be 1 (paragraph spacing).
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
            let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
            for i in flushed..end {
                let gap = if i > 0 {
                    gap_between(&blocks[i - 1], &blocks[i])
                } else {
                    0
                };
                let rows = {
                    let st = state_for(&blocks[i]);
                    render_block(&mut out, &blocks[i], st.as_ref(), W, true)
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
        // "****" is 4 chars — the `len() > 4` check rejects empty bold
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
            render_cache: None,
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
            Some("echo hello && echo world && echo done"),
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
            render_cache: None,
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

    /// Regression guard for the silent fall-through where parallel layout workers
    /// couldn't reach Lua, so `render_tool` dropped to `render_wrapped_output` and showed
    /// the raw `output.content` (e.g. "wrote 562 bytes to poem.txt") instead of the
    /// plugin-rendered body. The fix pre-bakes the render on the main thread and stashes
    /// it on `ToolState.render_cache`; this test asserts that when a cache is present,
    /// its content reaches the transcript and `output.content` does not.
    #[test]
    fn tool_render_cache_replaces_raw_output() {
        use smelt_core::content::block_layout::BlockLayout;
        use smelt_core::transcript_model::ToolOutput;

        let mut payload = Buffer::new(BufId(99), BufCreateOpts::default());
        payload.set_all_lines(vec!["fn main() {".into(), "    println!(\"hi\");".into()]);
        let cache = (
            W as u16,
            BlockLayout::Leaf(smelt_core::content::block_layout::RenderedLeaf::Buf(
                Box::new(payload),
            )),
        );

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
            render_cache: Some(cache),
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
            "raw output.content must not leak when render_cache is present, got: {joined:?}"
        );
    }

    /// When the cached width doesn't match the layout width, the cache is ignored and
    /// the rendered body falls back to `output.content`. This guards the resize path.
    #[test]
    fn tool_render_cache_ignored_on_width_mismatch() {
        use smelt_core::content::block_layout::BlockLayout;
        use smelt_core::transcript_model::ToolOutput;

        let mut payload = Buffer::new(BufId(99), BufCreateOpts::default());
        payload.set_all_lines(vec!["STALE_LAYOUT".into()]);
        let stale_cache = (
            (W as u16) + 10, // cached width != ctx.width
            BlockLayout::Leaf(smelt_core::content::block_layout::RenderedLeaf::Buf(
                Box::new(payload),
            )),
        );

        let block = Block::ToolCall {
            call_id: "c-stale".into(),
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
            render_cache: Some(stale_cache),
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
            !joined.contains("STALE_LAYOUT"),
            "stale-width cache must not be used, got: {joined:?}"
        );
        assert!(
            joined.contains("FALLBACK"),
            "expected fallback to output.content on width mismatch, got: {joined:?}"
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
            status: ToolStatus::Ok,
            elapsed: Some(std::time::Duration::from_secs(3)),
            output: None,
            user_message: None,
            render_cache: None,
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
}
