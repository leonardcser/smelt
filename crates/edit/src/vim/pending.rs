use super::*;

pub(super) fn handle_waiting_r(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    let replacement_char = match key.code {
        KeyCode::Char(c) => Some(c),
        KeyCode::Enter => Some('\n'),
        _ => None,
    };
    if let Some(c) = replacement_char.filter(|c| *c != ATTACHMENT_MARKER) {
        let n = ctx.vim_state.take_count();
        let start = *ctx.cpos;
        let eol = line_end(ctx.buf.as_str(), start);
        let end = advance_chars(ctx.buf.as_str(), start, n);
        // The count must fit on the line, or nothing is replaced.
        if ctx.buf.as_str()[start..eol].chars().count() >= n {
            ctx.vim_state.record_change(RepeatCommand::Replace {
                count: n,
                replacement: c,
            });
            ctx.save_undo();
            if c == '\n' {
                // `r<CR>` replaces all the characters with one line break. The
                // new line drops the blanks it starts with and keeps the indent
                // only when text follows.
                let sol = line_start(ctx.buf.as_str(), start);
                let rest = first_non_blank_at(ctx.buf.as_str(), end);
                let indent_end = if rest < eol {
                    first_non_blank_at(ctx.buf.as_str(), sol).min(start)
                } else {
                    sol
                };
                let indent = ctx.buf.as_str()[sol..indent_end].to_string();
                ctx.replace_range(start, rest, &format!("\n{indent}"));
                *ctx.cpos = start + 1 + indent.len();
            } else {
                let replacement = c.to_string().repeat(n);
                ctx.replace_range(start, end, &replacement);
                *ctx.cpos =
                    prev_grapheme_boundary(ctx.buf.as_str(), start + replacement.len()).max(start);
            }
            clamp_normal(ctx.buf.as_str(), ctx.cpos);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn handle_waiting_find(
    key: KeyEvent,
    kind: FindKind,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(ch) = key.code {
        ctx.vim_state.last_find = Some((kind, ch));
        move_cursor(
            ctx,
            Motion::Find {
                kind,
                ch,
                repeat: false,
            },
        );
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn handle_waiting_op_find(
    key: KeyEvent,
    op: Op,
    kind: FindKind,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(ch) = key.code {
        let count = ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
        ctx.vim_state.last_find = Some((kind, ch));
        let motion = Motion::Find {
            kind,
            ch,
            repeat: false,
        };
        if let Some(span) = motion_span(ctx, Some(op), motion) {
            if op != Op::Yank {
                ctx.vim_state.record_change(RepeatCommand::OpFind {
                    op,
                    kind,
                    target: ch,
                    count,
                });
            }
            return apply_span(op, ctx, span);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn handle_waiting_g(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let Some(motion) = display_rows(key, ctx) {
        return request_screen(ctx, None, motion);
    }
    if key.code == KeyCode::Char('v') {
        ctx.vim_state
            .reselect_visual(ctx.mode, ctx.cpos, ctx.buf.as_str());
        return Action::Consumed;
    }
    if matches!(key.code, KeyCode::Char('i' | 'I')) && *ctx.mode == VimMode::Normal {
        let count = ctx.vim_state.take_count();
        ctx.save_undo();
        *ctx.cpos = if key.code == KeyCode::Char('I') {
            line_start(ctx.buf.as_str(), *ctx.cpos)
        } else {
            ctx.vim_state
                .last_insert_end
                .map_or(*ctx.cpos, |end| end.min(ctx.buf.len()))
        };
        ctx.vim_state.insert_count = count;
        enter_insert_mode(ctx);
        return Action::Consumed;
    }
    if key.code == KeyCode::Char('J') && *ctx.mode != VimMode::Normal {
        return visual_command(VisualAction::Join { spaces: false }, false, ctx);
    }
    if key.code == KeyCode::Char('J') && *ctx.mode == VimMode::Normal {
        let count = ctx.vim_state.take_count();
        join_lines(ctx, count, false);
        ctx.vim_state
            .record_change(RepeatCommand::JoinWithoutSpaces { count });
        ctx.vim_state.reset_pending();
        return Action::Consumed;
    }
    if let KeyCode::Char(c) = key.code {
        if let Some(op) = Op::case_op(c) {
            if *ctx.mode == VimMode::Normal {
                ctx.vim_state.sub = SubState::WaitingOp(op);
                return Action::Consumed;
            }
            return visual_command(VisualAction::Operator(op), false, ctx);
        }
    }
    if let Some(motion) = motion_after_g(key) {
        move_cursor(ctx, motion);
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn handle_waiting_op_g(key: KeyEvent, op: Op, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if op.recase("").is_some() && key.code == KeyCode::Char(op.char()) {
        let count = ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
        let action = execute_linewise_op(op, ctx);
        ctx.vim_state
            .record_change(RepeatCommand::Linewise { op, count });
        return action;
    }
    if let Some(motion) = display_rows(key, ctx) {
        return request_screen(ctx, Some(op), motion);
    }
    if let Some(span) = motion_after_g(key).and_then(|motion| motion_span(ctx, Some(op), motion)) {
        return apply_span(op, ctx, span);
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

/// `gj` / `gk` from the key after `g`, consuming the typed count.
fn display_rows(key: KeyEvent, ctx: &mut VimContext<'_>) -> Option<ScreenMotion> {
    let KeyCode::Char(c @ ('j' | 'k')) = key.code else {
        return None;
    };
    let rows = ctx.vim_state.take_typed_count().unwrap_or(1) as isize;
    Some(ScreenMotion::Rows(if c == 'j' { rows } else { -rows }))
}

pub(super) fn motion_after_g(key: KeyEvent) -> Option<Motion> {
    match key.code {
        KeyCode::Char(c) => Motion::from_g(c),
        _ => None,
    }
}

/// The text object a key names, for `inner` (`i`) or around (`a`).
pub(super) fn text_object_spec(key: KeyEvent, inner: bool) -> Option<TextObjectSpec> {
    match key.code {
        KeyCode::Char(c) => TextObjectSpec::new(inner, c),
        _ => None,
    }
}

pub(super) fn handle_waiting_textobj(
    key: KeyEvent,
    op: Op,
    inner: bool,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    let count = ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
    let span = text_object_spec(key, inner).and_then(|spec| {
        let buf = ctx.buf.as_str();
        let sel = match select_object(buf, *ctx.cpos, None, spec, count) {
            Ok(sel) => sel,
            Err(stranded) => {
                *ctx.cpos = stranded.cursor;
                clamp_normal(buf, ctx.cpos);
                return None;
            }
        };
        if op == Op::Yank || op.recase("").is_some() {
            *ctx.cpos = sel.start;
            clamp_normal(buf, ctx.cpos);
        }
        // A tag block keeps a closing tag in column 0 out of the end adjustment.
        if spec.kind == TextObjectKind::Tag {
            let (start, end) = sel.range(buf);
            return Some(Span::Chars(start, end));
        }
        let reach = match (sel.linewise, sel.inclusive) {
            (true, _) => Reach::Linewise,
            (false, true) => Reach::Inclusive,
            (false, false) => Reach::Exclusive,
        };
        operator_span(buf, sel.start, sel.end, reach)
    });
    if let (Some(span), KeyCode::Char(object)) = (span, key.code) {
        if op != Op::Yank {
            ctx.vim_state.record_change(RepeatCommand::TextObject {
                op,
                inner,
                object,
                count,
            });
        }
        return apply_span(op, ctx, span);
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}
