//! Per-`Block`-variant renderers. Each variant lives in its own
//! sibling file (`text.rs`, `user.rs`, `thinking.rs`, `code_line.rs`,
//! `tool_call.rs`, `compacted.rs`, `exec.rs`) and exposes a
//! `pub(super) fn render(out, …) -> u16`. This module is the
//! dispatcher: [`layout_block_into`] (the entry point) builds a
//! [`LineBuilder`] over the per-block buffer, calls
//! [`render_block`] which does one match-arm per variant, and then
//! collapses the laid-out rows according to the block's
//! [`ViewState`].
//!
//! Pulls helpers shared with `tui::content::prompt_buf` and
//! `tui::app::transcript` (e.g. [`UserBlockGeometry`],
//! [`render_thinking_summary`]) out of the per-variant files for
//! direct re-export.

use smelt_core::buffer::Buffer;
use smelt_core::content::builder::{Outcome, LineBuilder};
use smelt_core::content::LayoutContext;
use smelt_core::theme::role_hl;
use smelt_core::theme::Theme;
use smelt_core::transcript_model::{Block, ToolState, ViewState};

pub mod markdown;
mod tools;

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
pub use tools::render_default_output;
pub use user::UserBlockGeometry;

pub use smelt_core::transcript_present::ToolBodyRenderer;

/// Cap on the number of rows a single tool block contributes to the
/// overlay or scrollback. Applied separately to:
///
/// - the bash command summary header (`print_tool_line`), which shows
///   the first N command lines + a "... M below" indicator,
/// - the tool output body (`render_wrapped_output`), which shows the
///   last N output lines + a "... M above" indicator.
///
/// 20 keeps a single tool block visually contained even on small
/// terminals; the overlay tail-crop handles the rare case where two
/// or more capped tools still don't fit.
const MAX_TOOL_BLOCK_ROWS: usize = 20;

/// Default number of lines shown in non-bash tool result previews
/// (`render_default_output`). Joined with " | " separators.
const DEFAULT_PREVIEW_LINES: usize = 3;

/// Layout entry point: render `block` directly into `buf` at the
/// given width. Drives the per-variant renderers against a fresh
/// `LineBuilder` and applies the block's view state on the resulting
/// buffer slice.
///
/// `state` must be `Some(_)` for `Block::ToolCall` and is unused for
/// every other variant. Returns rendering metadata (`line_count`,
/// `was_wrapped`, `max_line_width`) so callers can reason about width
/// pinning the same way the old `DisplayBlock` shape allowed.
pub fn layout_block_into(
    buf: &mut Buffer,
    theme: &Theme,
    block: &Block,
    state: Option<&ToolState>,
    ctx: &LayoutContext,
    renderer: Option<&dyn ToolBodyRenderer>,
) -> Outcome {
    let width = ctx.width as usize;
    let show_thinking = ctx.show_thinking;
    let outcome = {
        let mut col = LineBuilder::new(buf, theme, ctx.width);
        render_block(&mut col, block, state, width, show_thinking, renderer);
        col.finish()
    };
    apply_view_state(buf, theme, ctx.width, ctx.view_state, outcome)
}

/// Truncate / collapse the laid-out block according to its view state.
/// Runs post-layout so every block variant gets the same treatment.
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
                // Drop the leading lines we don't keep.
                buf.set_lines(start, start + (total - keep), vec![]);
                // Now insert an ellipsis line before the kept tail.
                // Easiest: render the ellipsis at `start`, then move the
                // kept tail by one. We do that by collecting the kept
                // lines, clearing the slice, rendering the ellipsis
                // first, then re-inserting via set_lines.
                let mut kept_lines: Vec<String> = (0..keep)
                    .map(|i| buf.get_line(start + i).unwrap_or("").to_string())
                    .collect();
                // Snapshot per-line decorations + highlights for the
                // kept tail before we overwrite.
                let kept_decorations: Vec<_> = (0..keep)
                    .map(|i| buf.decoration_at(start + i).clone())
                    .collect();
                let kept_highlights: Vec<_> =
                    (0..keep).map(|i| buf.highlights_at(start + i)).collect();
                // Wipe the slice.
                buf.set_lines(start, start + keep, vec![]);
                // Render ellipsis at `start`.
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
                // Re-append the kept content rows.
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
        col.push_hl(role_hl("Muted"));
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
    renderer: Option<&dyn ToolBodyRenderer>,
) -> u16 {
    let _perf = smelt_core::perf::begin(match block {
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
                renderer,
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
    use smelt_core::transcript_model::{ToolOutput, ToolStatus};
    use smelt_core::transcript_present::{gap_between, Element};
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
        renderer: Option<&dyn ToolBodyRenderer>,
    ) -> Vec<TestLine> {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        let outcome = layout_block_into(&mut buf, &theme, block, state, ctx, renderer);
        read_buffer(&buf, &theme, outcome.line_count)
    }

    /// Test renderer that mirrors the production tool registry well enough
    /// to drive bash-flavored layout assertions.
    struct TestToolRenderer;

    impl ToolBodyRenderer for TestToolRenderer {
        fn render(
            &self,
            _: &str,
            _: &HashMap<String, serde_json::Value>,
            _: Option<&ToolOutput>,
            _: usize,
            _: &mut LineBuilder,
        ) -> u16 {
            0
        }
        fn elapsed_visible(&self, name: &str) -> bool {
            name == "bash"
        }
        fn render_summary_line(
            &self,
            name: &str,
            line: &str,
            _args: &HashMap<String, serde_json::Value>,
            out: &mut LineBuilder,
        ) -> bool {
            if name == "bash" {
                // Stand-in for the bash highlighter — preserve the shape
                // (renderer says "yes I rendered it") without pulling
                // syntect into core tests.
                out.print(line);
                true
            } else {
                false
            }
        }
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
            summary: String::new(),
            args: HashMap::new(),
        }
    }

    fn tool_call() -> Block {
        let mut args = HashMap::new();
        args.insert("command".into(), serde_json::Value::String("ls".into()));
        Block::ToolCall {
            call_id: "call-1".into(),
            name: "bash".into(),
            summary: "ls".into(),
            args,
        }
    }

    fn pending_tool_state() -> ToolState {
        ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
        }
    }

    fn state_for(block: &Block) -> Option<ToolState> {
        matches!(block, Block::ToolCall { .. }).then(pending_tool_state)
    }

    fn block_rows(block: &Block) -> u16 {
        let (mut buf, theme) = mk_collector_buf();
        let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
        let st = state_for(block);
        render_block(&mut out, block, st.as_ref(), W, true, None)
    }

    /// Compute total gap rows between the last history block and an active tool.
    fn tool_gap_for(blocks: &[Block]) -> u16 {
        blocks
            .last()
            .map(|b| gap_between(&Element::Block(b), &Element::Block(&empty_tool_call())))
            .unwrap_or(0)
    }

    /// Simulate the "all-at-once" render path: all blocks are unflushed,
    /// rendered in one pass, then active tool is appended.
    /// Returns (block_rows, tool_gap, total_before_tool).
    fn render_all_at_once(blocks: &[Block]) -> (u16, u16, u16) {
        let (mut buf, theme) = mk_collector_buf();
        let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
        let mut total = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true, None)
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
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true, None)
            };
            block_rows_total += gap + rows;
        }
        // anchor_row = start_row + block_rows_total

        // Phase 2: draw_frame(None) — dialog mode
        // block_rows = 0 (all flushed)
        let tg = tool_gap_for(blocks);
        // Active tool rendered at anchor_row + tg
        // Total rows from start to tool = block_rows_total + tg
        (block_rows_total, tg, block_rows_total + tg)
    }

    /// Simulate a third path: blocks flushed across multiple draw_frame calls
    /// (each event gets its own tick), then dialog frame renders tool.
    /// Key difference: anchor_row is set by the LAST draw_frame(prompt) call,
    /// which uses anchor_row = top_row + block_rows. When blocks were flushed
    /// in a previous frame, block_rows = 0, so anchor_row = top_row.
    fn render_incremental(blocks: &[Block]) -> (u16, u16, u16) {
        // Each block arrives in a separate frame.
        // Frame N renders block N, prompt after it.
        // anchor_row = top_row + block_rows_in_this_frame.
        // For the LAST frame (that rendered the last block), anchor_row =
        // draw_start + (gap + rows of that block).
        // But draw_start for that frame = anchor from previous frame.
        //
        // Net effect: final anchor = sum of all block rows + gaps.
        // This is the same as render_split.
        let (mut buf, theme) = mk_collector_buf();
        let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
        let mut cumulative = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true, None)
            };
            cumulative += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (cumulative, tg, cumulative + tg)
    }

    // ── The actual tests ────────────────────────────────────────────────

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
        // Empty thinking block renders 0 rows but still exists in history.
        let blocks = vec![user("fix it"), thinking(""), text("Here's the fix.")];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);

        // The empty thinking block renders 0 rows.
        let thinking_rows = block_rows(&thinking(""));
        assert_eq!(thinking_rows, 0);

        // But gap_between still counts gaps around it:
        // User→Thinking = 1, Thinking→Text = 1
        // So there are 2 blank lines between User and Text.
        let user_thinking_gap = gap_between(
            &Element::Block(&user("fix it")),
            &Element::Block(&thinking("")),
        );
        let thinking_text_gap = gap_between(
            &Element::Block(&thinking("")),
            &Element::Block(&text("Here's the fix.")),
        );
        assert_eq!(user_thinking_gap, 1);
        assert_eq!(thinking_text_gap, 1);

        // But the gap from Text→ActiveTool should still be 1.
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
        // What if the LLM sends empty text content?
        let blocks = vec![user("hello"), text("")];
        let rows = block_rows(&text(""));
        assert_eq!(rows, 0, "empty text renders 0 rows");

        let gap = gap_between(
            &Element::Block(&text("")),
            &Element::Block(&empty_tool_call()),
        );
        assert_eq!(gap, 1, "gap is still 1 for empty text block");

        // This means: User(1 row) + gap(1) + Text(0 rows) + gap(1) = tool at offset 3
        // But visually the empty text is invisible, so it looks like 2 blank lines.
        // This could be the bug source!
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        assert_eq!(a.2, b.2, "both paths match (even if wrong)");

        // Compare with blocks that DON'T have the empty text:
        let blocks_no_empty = vec![user("hello")];
        let c = render_all_at_once(&blocks_no_empty);
        // User→ActiveTool gap:
        let gap_user_tool = gap_between(
            &Element::Block(&user("hello")),
            &Element::Block(&empty_tool_call()),
        );
        assert_eq!(gap_user_tool, 1, "User→ActiveTool = 1");

        // With empty text:  total = user_rows + 1(User→Text gap=0, Text→Text=0? no, User→Text)
        // Let me compute manually:
        let user_text_gap =
            gap_between(&Element::Block(&user("hello")), &Element::Block(&text("")));
        // User→anything = 1
        assert_eq!(user_text_gap, 1, "User→Text = 1");
        // text("")→ActiveTool = 1
        // So total: user_rows + 1(gap) + 0(empty text) + 1(gap) = user_rows + 2
        // vs without empty text: user_rows + 1(gap)
        // That's ONE EXTRA blank line when there's an empty text block!

        let diff = a.2 as i32 - c.2 as i32;
        // diff should be 1 if there's an extra gap from the empty text
        assert_eq!(diff, 1, "empty text block adds 1 extra gap row (the bug!)");
    }

    #[test]
    fn adjacent_text_blocks_gap() {
        // Two consecutive text blocks — gap should be 1 (paragraph spacing).
        let gap = gap_between(&Element::Block(&text("a")), &Element::Block(&text("b")));
        assert_eq!(gap, 1, "Text→Text gap = 1");
    }

    /// Simulate draw_frame anchor tracking across multiple frames.
    /// Returns the row offset where the active tool starts, relative to
    /// where the first block started rendering.
    ///
    /// `flushed_at` is the set of frame boundaries: blocks[0..flushed_at[0]]
    /// are rendered in frame 0, blocks[flushed_at[0]..flushed_at[1]] in
    /// frame 1, etc. The active tool renders in the final frame.
    fn tool_start_row(blocks: &[Block], flushed_at: &[usize]) -> u16 {
        let mut anchor: u16 = 0; // start of rendering
        let mut flushed: usize = 0;

        for &end in flushed_at {
            // This frame renders blocks[flushed..end]
            let mut frame_block_rows = 0u16;
            let (mut buf, theme) = mk_collector_buf();
            let mut out = LineBuilder::new(&mut buf, &theme, W as u16);
            for i in flushed..end {
                let gap = if i > 0 {
                    gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
                } else {
                    0
                };
                let rows = {
                    let st = state_for(&blocks[i]);
                    render_block(&mut out, &blocks[i], st.as_ref(), W, true, None)
                };
                frame_block_rows += gap + rows;
            }
            // In non-dialog draw_frame: anchor_row = top_row + block_rows
            // where top_row = draw_start_row = previous anchor
            // So new anchor = anchor + frame_block_rows
            anchor += frame_block_rows;
            flushed = end;
        }

        // Final frame: dialog mode. All blocks flushed.
        // draw_start_row = anchor (from last frame)
        // block_rows = 0 (all flushed)
        // tool_gap = gap_between(last block, ActiveTool)
        let tg = tool_gap_for(blocks);
        // Tool renders at anchor + tg
        anchor + tg
    }

    #[test]
    fn anchor_tracking_single_frame() {
        // All blocks arrive together, single frame before dialog.
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[2]);

        let user_rows = block_rows(&user("hello"));
        let text_rows = block_rows(&text("response"));
        let expected = user_rows + 1 /* User→Text */ + text_rows + 1 /* Text→Tool */;
        assert_eq!(row, expected);
    }

    #[test]
    fn anchor_tracking_split_frames() {
        // User flushed in frame 0, Text in frame 1, then dialog.
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[1, 2]);

        let user_rows = block_rows(&user("hello"));
        let text_rows = block_rows(&text("response"));
        let expected = user_rows + 1 /* User→Text */ + text_rows + 1 /* Text→Tool */;
        assert_eq!(row, expected);
    }

    #[test]
    fn anchor_tracking_each_block_separate() {
        // Each block flushed in its own frame.
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[1, 2]);

        // Same as single frame — the math should be identical.
        let single = tool_start_row(&blocks, &[2]);
        assert_eq!(row, single, "split and single-frame anchors must match");
    }

    #[test]
    fn anchor_tracking_with_empty_thinking() {
        let blocks = vec![user("hi"), thinking(""), text("fix")];

        let single = tool_start_row(&blocks, &[3]);
        let split = tool_start_row(&blocks, &[1, 2, 3]);
        assert_eq!(single, split, "empty thinking: single vs split must match");

        // Without the empty thinking:
        let blocks_no_thinking = vec![user("hi"), text("fix")];
        let no_thinking = tool_start_row(&blocks_no_thinking, &[2]);

        // The empty thinking adds 1 extra row (its gap before text).
        assert_eq!(
            single - no_thinking,
            1,
            "empty thinking adds exactly 1 extra row"
        );
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
        // Empty thinking between user and text adds 2 gaps for 0 visible rows.
        let with_empty_thinking = vec![user("hi"), thinking(""), text("response")];
        let without_thinking = vec![user("hi"), text("response")];

        let a = render_all_at_once(&with_empty_thinking);
        let b = render_all_at_once(&without_thinking);

        // Gap accounting:
        // With: User(N) + 1(User→Thinking) + 0(empty) + 1(Thinking→Text) + M(Text) = N+M+2
        // Without: User(N) + 1(User→Text) + M(Text) = N+M+1
        let diff = a.2 as i32 - b.2 as i32;
        assert_eq!(
            diff, 1,
            "empty thinking adds 1 extra gap row before text content"
        );
    }

    #[test]
    fn horizontal_rule_detection() {
        // Valid horizontal rules
        assert!(is_horizontal_rule("---"), "basic dashes");
        assert!(is_horizontal_rule("___"), "basic underscores");
        assert!(is_horizontal_rule("***"), "basic asterisks");
        assert!(is_horizontal_rule("------"), "longer dashes");
        assert!(is_horizontal_rule("-----"), "odd length");
        assert!(is_horizontal_rule(" - - - "), "spaced dashes");
        assert!(is_horizontal_rule(" * * * "), "spaced asterisks");
        assert!(is_horizontal_rule(" _ _ _ "), "spaced underscores");
        assert!(is_horizontal_rule("  ---  "), "padded dashes");

        // Invalid horizontal rules
        assert!(!is_horizontal_rule("--"), "too short");
        assert!(!is_horizontal_rule("-"), "single char");
        assert!(!is_horizontal_rule(""), "empty");
        assert!(!is_horizontal_rule("text"), "regular text");
        assert!(!is_horizontal_rule("- -"), "too short with spaces");
        assert!(!is_horizontal_rule("-*-*-*"), "mixed characters");
        assert!(!is_horizontal_rule("---a"), "contains other chars");
        assert!(!is_horizontal_rule("123"), "numbers");
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
        // A bash command that wraps at width 30 should produce:
        // - Row 0: source_text = full command, soft_wrapped = false
        // - Row 1+: source_text = None, soft_wrapped = true
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
        };
        let ctx = LayoutContext {
            width: 30,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx, Some(&TestToolRenderer));

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
            assert!(
                line.source_text.is_none(),
                "continuation rows should not have source_text"
            );
            assert!(
                line.soft_wrapped,
                "continuation rows should be soft_wrapped"
            );
        }
    }

    #[test]
    fn bash_tool_multiline_command_only_wraps_mark_soft() {
        // A multi-line command (real newlines) should NOT mark line 2
        // as soft-wrapped — only segments within a wrapped line should.
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
        };
        let ctx = LayoutContext {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx, Some(&TestToolRenderer));

        assert!(display.len() >= 2);
        assert!(!display[0].soft_wrapped);
        assert!(
            !display[1].soft_wrapped,
            "second real line should NOT be soft-wrapped"
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
        };
        let ctx = LayoutContext {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block_test(&block, Some(&state), &ctx, Some(&TestToolRenderer));
        let first_line = &display[0];

        // The time suffix "  3s" should be in a non-selectable span
        let has_non_selectable_time = first_line
            .spans
            .iter()
            .any(|span| !span.meta.selectable && span.text.contains("3s"));
        assert!(
            has_non_selectable_time,
            "elapsed time should be in a non-selectable span"
        );
    }
}
