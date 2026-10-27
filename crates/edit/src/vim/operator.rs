use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum MotionRange {
    Span(Span),
    Pending(SubState),
    Screen(ScreenMotion),
    None,
}

pub(super) fn resolve_motion_range(
    key: KeyEvent,
    ctx: &mut VimContext<'_>,
    op: Option<Op>,
) -> MotionRange {
    if let (KeyCode::Char(c), Some(op)) = (key.code, op) {
        let find = match c {
            'f' => Some(FindKind::Forward),
            'F' => Some(FindKind::Backward),
            't' => Some(FindKind::ForwardTill),
            'T' => Some(FindKind::BackwardTill),
            _ => None,
        };
        if let Some(kind) = find {
            return MotionRange::Pending(SubState::WaitingOpFind(op, kind));
        }
        if c == 'g' {
            return MotionRange::Pending(SubState::WaitingOpG(op));
        }
    }
    let motion = match key.code {
        KeyCode::Char(c @ (';' | ',')) => Motion::repeat_find(ctx.vim_state.last_find, c == ','),
        code => Motion::from_key(code),
    };
    if let Some(screen) = motion.and_then(|motion| ScreenMotion::of(motion, 1)) {
        let count = ctx.vim_state.take_typed_count().unwrap_or(1);
        return MotionRange::Screen(screen.with_count(count));
    }
    match motion.and_then(|motion| motion_span(ctx, op, motion)) {
        Some(span) => MotionRange::Span(span),
        None => MotionRange::None,
    }
}

/// The span `motion` covers from the cursor, consuming the typed count.
pub(super) fn motion_span(
    ctx: &mut VimContext<'_>,
    op: Option<Op>,
    motion: Motion,
) -> Option<Span> {
    let usage = if op == Some(Op::Change) {
        Use::Change
    } else {
        Use::Operator
    };
    let count = ctx.vim_state.take_typed_count();
    let from = *ctx.cpos;
    let (pos, reach) = resolve(ctx.buf.as_str(), from, motion, count, usage)?;
    let span = operator_span(ctx.buf.as_str(), from, pos, reach)?;
    if op.is_some_and(|op| yank_ends_above(ctx, op, from, pos, span)) {
        if motion.keeps_column(count) {
            let cell = cursor_cell(ctx);
            place_on_line(ctx, pos, cell);
        } else {
            *ctx.cpos = pos;
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
        }
    }
    Some(span)
}

/// A yank over lines leaves the cursor where the motion ended when that is on
/// a line above.
pub(super) fn yank_ends_above(
    ctx: &VimContext<'_>,
    op: Op,
    from: usize,
    pos: usize,
    span: Span,
) -> bool {
    let buf = ctx.buf.as_str();
    op == Op::Yank
        && matches!(span, Span::Lines(..) | Span::AdjustedLines(..))
        && line_start(buf, pos) < line_start(buf, from)
}

/// Vim deletes whole lines when a multi-line charwise `d` starts in the indent
/// and ends where only blanks remain on its line.
pub(super) fn whole_lines(buf: &str, span: Span) -> Span {
    let Span::Chars(start, end) = span else {
        return span;
    };
    let blank = |text: &str| text.trim_matches([' ', '\t']).is_empty();
    let covers_lines = buf[start..end].contains('\n')
        && blank(&buf[line_start(buf, start)..start])
        && blank(&buf[end..line_end(buf, end)]);
    if covers_lines {
        Span::Lines(line_start(buf, start), line_end(buf, end))
    } else {
        span
    }
}

pub(super) fn apply_span(op: Op, ctx: &mut VimContext<'_>, span: Span) -> Action {
    let span = if op == Op::Delete {
        whole_lines(ctx.buf.as_str(), span)
    } else {
        span
    };
    match span {
        Span::Chars(start, end) => apply_charwise_op(op, ctx, start, end),
        Span::Lines(start, end) => apply_linewise_op(op, ctx, start, end, true),
        Span::AdjustedLines(start, end) => apply_linewise_op(op, ctx, start, end, false),
    }
}

/// Operator-pending motion dispatch.
pub(super) fn execute_op_motion(key: KeyEvent, op: Op, ctx: &mut VimContext<'_>) -> Action {
    match resolve_motion_range(key, ctx, Some(op)) {
        MotionRange::Span(span) => apply_span(op, ctx, span),
        MotionRange::Pending(sub) => {
            ctx.vim_state.sub = sub;
            Action::Consumed
        }
        MotionRange::Screen(motion) => request_screen(ctx, Some(op), motion),
        MotionRange::None => Action::Consumed,
    }
}

pub(super) fn execute_linewise_op(op: Op, ctx: &mut VimContext<'_>) -> Action {
    let n = ctx.vim_state.effective_count();
    ctx.vim_state.reset_counts();
    ctx.vim_state.sub = SubState::Ready;
    // A count reaches down from the cursor's line, so it cannot start on the last one.
    if n > 1 && line_end(ctx.buf.as_str(), *ctx.cpos) >= ctx.buf.len() {
        return Action::Consumed;
    }
    // Deleting from an empty buffer leaves the register alone.
    if op == Op::Delete && ctx.buf.is_empty() {
        return Action::Consumed;
    }

    let start = line_start(ctx.buf.as_str(), *ctx.cpos);
    let mut end_pos = *ctx.cpos;
    for _ in 1..n {
        let next = line_end(ctx.buf.as_str(), end_pos);
        if next < ctx.buf.len() {
            end_pos = next + 1;
        }
    }
    let end = line_end(ctx.buf.as_str(), end_pos);
    if op.recase("").is_some() && n == 1 {
        *ctx.cpos = start;
    }
    apply_linewise_op(op, ctx, start, end, true)
}

/// Apply a charwise operator over the byte range [start..end).
pub(super) fn apply_charwise_op(
    op: Op,
    ctx: &mut VimContext<'_>,
    start: usize,
    end: usize,
) -> Action {
    // An empty span deletes and yanks nothing, but `c` still starts inserting.
    if start == end && matches!(op, Op::Delete | Op::Yank) {
        *ctx.cpos = start;
        clamp_normal(ctx.buf.as_str(), ctx.cpos);
        return Action::Consumed;
    }
    match op {
        Op::Delete => {
            ctx.save_undo();
            ctx.yank_range(op, start, end, false);
            ctx.delete_range(start, end);
            *ctx.cpos = start;
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
        }
        Op::Change => {
            ctx.save_undo();
            ctx.yank_range(op, start, end, false);
            ctx.delete_range(start, end);
            *ctx.cpos = start;
            enter_insert_mode(ctx);
            ctx.vim_state.reset_counts();
            return Action::Consumed;
        }
        Op::Yank => {
            ctx.yank_range(op, start, end, false);
            ctx.clipboard.kill_ring.mark_yanked(ctx.now);
            *ctx.cpos = start;
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
        }
        Op::Indent | Op::Dedent => {
            shift_lines(op, ctx, start, last_char(ctx.buf.as_str(), start, end), 1);
        }
        Op::Upper | Op::Lower | Op::Toggle => {
            // Vim treats an empty exclusive span as the char under it.
            let end = if start == end && !ctx.buf.as_str()[start..].starts_with('\n') {
                next_grapheme_boundary(ctx.buf.as_str(), start)
            } else {
                end
            };
            recase_range(op, ctx, start, end, start);
        }
    }
    Action::Consumed
}

/// Columns `>` and `<` shift a line by.
pub(super) const SHIFT_WIDTH: usize = 2;

/// The start of the last character of the charwise span `start..end`, which
/// names the last line it touches.
pub(super) fn last_char(buf: &str, start: usize, end: usize) -> usize {
    if end > start {
        prev_grapheme_boundary(buf, end).max(start)
    } else {
        start
    }
}

/// `>` and `<`: shift the lines from `first`'s to `last`'s right or left by
/// `times` shift widths as one undo step, and put the cursor on the first
/// line's first non-blank. `>` leaves empty lines alone; `<` takes a tab as
/// one shift width.
pub(super) fn shift_lines(
    op: Op,
    ctx: &mut VimContext<'_>,
    first: usize,
    last: usize,
    times: usize,
) {
    let buf = ctx.buf.as_str();
    let start = line_start(buf, first);
    let end = line_end(buf, last.max(first));
    let width = SHIFT_WIDTH.saturating_mul(times);
    let shifted = buf[start..end]
        .split('\n')
        .map(|line| {
            if op == Op::Indent {
                return if line.is_empty() {
                    String::new()
                } else {
                    format!("{}{line}", " ".repeat(width))
                };
            }
            let mut columns = 0;
            let cut = line
                .find(|c| {
                    let step = match c {
                        ' ' => 1,
                        '\t' => SHIFT_WIDTH,
                        _ => return true,
                    };
                    if columns + step > width {
                        return true;
                    }
                    columns += step;
                    false
                })
                .unwrap_or(line.len());
            line[cut..].to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    if shifted != buf[start..end] {
        *ctx.cpos = start;
        ctx.save_undo();
        ctx.replace_range(start, end, &shifted);
    }
    *ctx.cpos = first_non_blank_at(ctx.buf.as_str(), start);
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

/// Rewrite the case of `start..end` with a case operator as one undo step and
/// leave the cursor at `cursor`, which is also where undo puts it back.
pub(super) fn recase_range(
    op: Op,
    ctx: &mut VimContext<'_>,
    start: usize,
    end: usize,
    cursor: usize,
) {
    let Some(text) = op.recase(smelt_buffer::text::slice(ctx.buf.as_str(), start..end)) else {
        return;
    };
    *ctx.cpos = cursor;
    ctx.save_undo();
    ctx.replace_range(start, end, &text);
    *ctx.cpos = smelt_buffer::text::snap_grapheme(ctx.buf.as_str(), cursor);
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

/// Apply linewise operator over [start..end] (line boundaries). A delete
/// leaves the cursor in its column, or on the first non-blank when `keep_column`
/// is false.
pub(super) fn apply_linewise_op(
    op: Op,
    ctx: &mut VimContext<'_>,
    start: usize,
    end: usize,
    keep_column: bool,
) -> Action {
    match op {
        Op::Delete => {
            ctx.save_undo();
            ctx.yank_range(op, start, end, true);
            let column = cursor_cell(ctx);
            let removed = linewise_delete_range(ctx.buf.as_str(), start..end);
            ctx.delete_range(removed.start, removed.end);
            if keep_column {
                place_on_line(ctx, removed.start, column);
            } else {
                let line = line_start(ctx.buf.as_str(), removed.start.min(ctx.buf.len()));
                *ctx.cpos = first_non_blank_at(ctx.buf.as_str(), line);
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
        }
        Op::Change => {
            ctx.save_undo();
            ctx.yank_range(op, start, end, true);
            // The first line keeps its indent.
            let content_start = first_non_blank_at(ctx.buf.as_str(), start);
            ctx.delete_range(content_start, end);
            *ctx.cpos = content_start;
            ctx.vim_state.auto_indent = content_start - start;
            enter_insert_mode(ctx);
            return Action::Consumed;
        }
        Op::Yank => {
            // Linewise yank does not reposition the cursor (vim default).
            // The register holds the line contents without a trailing newline
            // so a linewise paste can prepend or append its own.
            ctx.yank_range(op, start, end, true);
            ctx.clipboard.kill_ring.mark_yanked(ctx.now);
        }
        Op::Indent | Op::Dedent => shift_lines(op, ctx, start, end, 1),
        Op::Upper | Op::Lower | Op::Toggle => {
            // The cursor keeps its column and moves to the first line.
            let column = *ctx.cpos - line_start(ctx.buf.as_str(), *ctx.cpos);
            let cursor = (start + column).min(line_end(ctx.buf.as_str(), start));
            recase_range(op, ctx, start, end, cursor);
        }
    }
    Action::Consumed
}
