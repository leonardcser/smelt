use super::*;

pub(super) fn handle_waiting_surround_motion(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    if let KeyCode::Char(c) = key.code {
        if c == 'i' || c == 'a' {
            ctx.vim_state.sub = SubState::WaitingSurroundTextObj(c == 'i');
            return Action::Consumed;
        }
        if c == 's' {
            let (start, end) = current_line_content_range(ctx.buf.as_str(), *ctx.cpos);
            ctx.vim_state.sub = SubState::WaitingSurroundChar(start, end);
            ctx.vim_state.reset_counts();
            return Action::Consumed;
        }
    }

    match resolve_motion_range(key, ctx, None) {
        MotionRange::Span(
            Span::Chars(start, end) | Span::Lines(start, end) | Span::AdjustedLines(start, end),
        ) => {
            ctx.vim_state.sub = SubState::WaitingSurroundChar(start, end);
            Action::Consumed
        }
        MotionRange::Pending(_) | MotionRange::None => {
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
    }
}

pub(super) fn handle_waiting_surround_textobj(
    key: KeyEvent,
    inner: bool,
    ctx: &mut VimContext<'_>,
) -> Action {
    if let KeyCode::Char(c) = key.code {
        if let Some((start, end)) = text_object(ctx.buf.as_str(), *ctx.cpos, inner, c) {
            ctx.vim_state.sub = SubState::WaitingSurroundChar(start, end);
            ctx.vim_state.reset_counts();
            return Action::Consumed;
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn handle_waiting_surround_char(
    key: KeyEvent,
    start: usize,
    end: usize,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(c) = key.code {
        if let Some((open, close)) = surround_pair(c) {
            add_surround(ctx, start, end, open, close);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn handle_waiting_delete_surround(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(c) = key.code {
        if delete_surround(ctx, c) {
            ctx.vim_state
                .record_change(RepeatCommand::DeleteSurround { target: c });
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn handle_waiting_change_surround_target(
    key: KeyEvent,
    ctx: &mut VimContext<'_>,
) -> Action {
    if let KeyCode::Char(c) = key.code {
        if surrounding_delimiters(ctx.buf.as_str(), *ctx.cpos, c).is_some() {
            ctx.vim_state.sub = SubState::WaitingChangeSurroundReplacement(c);
            return Action::Consumed;
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn handle_waiting_change_surround_replacement(
    key: KeyEvent,
    target: char,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(replacement) = key.code {
        if change_surround(ctx, target, replacement) {
            ctx.vim_state.record_change(RepeatCommand::ChangeSurround {
                target,
                replacement,
            });
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

pub(super) fn surround_pair(c: char) -> Option<(&'static str, &'static str)> {
    match c {
        '"' => Some(("\"", "\"")),
        '\'' => Some(("'", "'")),
        '`' => Some(("`", "`")),
        '(' => Some(("( ", " )")),
        ')' | 'b' => Some(("(", ")")),
        '[' => Some(("[ ", " ]")),
        ']' | 'r' => Some(("[", "]")),
        '{' => Some(("{ ", " }")),
        '}' | 'B' => Some(("{", "}")),
        '<' => Some(("< ", " >")),
        '>' | 'a' => Some(("<", ">")),
        'q' => Some(("\"", "\"")),
        _ => None,
    }
}

pub(super) fn add_surround(
    ctx: &mut VimContext<'_>,
    start: usize,
    end: usize,
    open: &str,
    close: &str,
) {
    let start = smelt_buffer::text::snap_grapheme(ctx.buf.as_str(), start.min(ctx.buf.len()));
    let end = smelt_buffer::text::snap_grapheme(ctx.buf.as_str(), end.min(ctx.buf.len()));
    if start > end {
        return;
    }
    ctx.save_undo();
    ctx.insert_str(end, close);
    ctx.insert_str(start, open);
    *ctx.cpos = start + open.len();
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

pub(super) fn delete_surround(ctx: &mut VimContext<'_>, kind: char) -> bool {
    let Some(delims) = surrounding_delimiters(ctx.buf.as_str(), *ctx.cpos, kind) else {
        return false;
    };
    ctx.save_undo();
    ctx.delete_range(delims.close_start, delims.close_end);
    ctx.delete_range(delims.open_start, delims.open_end);
    *ctx.cpos = delims.open_start.min(ctx.buf.len());
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
    true
}

pub(super) fn change_surround(ctx: &mut VimContext<'_>, target: char, replacement: char) -> bool {
    let Some((open, close)) = surround_pair(replacement) else {
        return false;
    };
    let Some(delims) = surrounding_delimiters(ctx.buf.as_str(), *ctx.cpos, target) else {
        return false;
    };
    ctx.save_undo();
    ctx.replace_range(delims.close_start, delims.close_end, close);
    ctx.replace_range(delims.open_start, delims.open_end, open);
    *ctx.cpos = delims.open_start + open.len();
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
    true
}
