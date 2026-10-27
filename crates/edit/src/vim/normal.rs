use super::*;

pub(super) fn handle_normal(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('r') => {
                ctx.redo();
                return Action::Consumed;
            }
            KeyCode::Char(c @ ('a' | 'x')) => {
                let count = ctx.vim_state.take_count();
                add_to_number(ctx, add_amount(c, count));
                ctx.vim_state.record_change(RepeatCommand::Add {
                    subtract: c == 'x',
                    count,
                });
                return Action::Consumed;
            }
            // Pass through keys that the main handler needs.
            KeyCode::Char(
                'c' | 'd' | 'u' | 't' | 'k' | 'l' | 'f' | 'b' | 'j' | 'n' | 'p' | 's' | 'y' | 'e',
            ) => return Action::Passthrough,
            _ => return Action::Consumed,
        }
    }

    if key.code == KeyCode::BackTab {
        return Action::Passthrough;
    }

    // Shift+arrows pass through for shared shift-selection actions.
    if key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
        )
    {
        return Action::Passthrough;
    }

    match ctx.vim_state.sub {
        SubState::WaitingR => return handle_waiting_r(key, ctx),
        SubState::WaitingZ => {
            ctx.vim_state.sub = SubState::Ready;
            return match key.code {
                KeyCode::Char('z') => Action::CenterScroll,
                KeyCode::Char('h') => Action::PanColumns(-1),
                KeyCode::Char('l') => Action::PanColumns(1),
                _ => Action::Consumed,
            };
        }
        SubState::WaitingFind(kind) => return handle_waiting_find(key, kind, ctx),
        SubState::WaitingOpFind(op, kind) => return handle_waiting_op_find(key, op, kind, ctx),
        SubState::WaitingG => return handle_waiting_g(key, ctx),
        SubState::WaitingOpG(op) => return handle_waiting_op_g(key, op, ctx),
        SubState::WaitingTextObj(op, inner) => return handle_waiting_textobj(key, op, inner, ctx),
        SubState::WaitingSurroundMotion => return handle_waiting_surround_motion(key, ctx),
        SubState::WaitingSurroundTextObj(inner) => {
            return handle_waiting_surround_textobj(key, inner, ctx)
        }
        SubState::WaitingSurroundChar(start, end) => {
            return handle_waiting_surround_char(key, start, end, ctx)
        }
        SubState::WaitingDeleteSurround => return handle_waiting_delete_surround(key, ctx),
        SubState::WaitingChangeSurroundTarget => {
            return handle_waiting_change_surround_target(key, ctx)
        }
        SubState::WaitingChangeSurroundReplacement(target) => {
            return handle_waiting_change_surround_replacement(key, target, ctx)
        }
        SubState::WaitingOp(op) => {
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count2.is_some()) {
                    ctx.vim_state.count2 = Some(
                        ctx.vim_state.count2.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize,
                    );
                    return Action::Consumed;
                }
                if c == op.char() {
                    let count =
                        ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
                    let action = execute_linewise_op(op, ctx);
                    if op != Op::Yank {
                        ctx.vim_state
                            .record_change(RepeatCommand::Linewise { op, count });
                    }
                    return action;
                }
                if c == 's' {
                    let surround = match op {
                        Op::Yank => Some(SubState::WaitingSurroundMotion),
                        Op::Delete => Some(SubState::WaitingDeleteSurround),
                        Op::Change => Some(SubState::WaitingChangeSurroundTarget),
                        Op::Upper | Op::Lower | Op::Toggle | Op::Indent | Op::Dedent => None,
                    };
                    if let Some(sub) = surround {
                        ctx.vim_state.sub = sub;
                        return Action::Consumed;
                    }
                }
                if c == 'i' || c == 'a' {
                    ctx.vim_state.sub = SubState::WaitingTextObj(op, c == 'i');
                    return Action::Consumed;
                }
            }
            let repeat_key = RepeatKey::from_key(key);
            let count = ctx.vim_state.count1.unwrap_or(1) * ctx.vim_state.count2.unwrap_or(1);
            let result = execute_op_motion(key, op, ctx);
            if op != Op::Yank
                && !matches!(
                    ctx.vim_state.sub,
                    SubState::WaitingOpFind(_, _) | SubState::WaitingOpG(_)
                )
            {
                if let Some(motion) = repeat_key {
                    ctx.vim_state
                        .record_change(RepeatCommand::OpMotion { op, motion, count });
                }
            }
            // Don't reset if a new substate was set (e.g. WaitingOpFind for df/dt).
            if matches!(ctx.vim_state.sub, SubState::WaitingOp(_)) {
                ctx.vim_state.reset_pending();
            }
            return result;
        }
        // Only viewers wait for a block kind.
        SubState::WaitingBracket(..)
        | SubState::WaitingMark(_)
        | SubState::WaitingRegister
        | SubState::WaitingVisualTextObj(_)
        | SubState::Ready => {}
    }

    if let KeyCode::Char(c) = key.code {
        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
            return handle_normal_char(c, ctx);
        }
    }

    match key.code {
        KeyCode::Esc => {
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        KeyCode::Enter => Action::Submit,
        KeyCode::Left => {
            *ctx.cpos = move_left(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Right => {
            *ctx.cpos = move_right_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Up => Action::HistoryPrev,
        KeyCode::Down => Action::HistoryNext,
        KeyCode::Home => {
            *ctx.cpos = line_start(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::End => {
            *ctx.cpos = line_end_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Backspace => {
            *ctx.cpos = move_left(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        _ => Action::Consumed,
    }
}

pub(super) fn repeat_last_change(ctx: &mut VimContext<'_>) -> Action {
    let Some((command, register)) = ctx.vim_state.last_change else {
        ctx.vim_state.reset_pending();
        return Action::Consumed;
    };
    // A register named before `.` replaces the one the change used.
    ctx.vim_state.register = ctx.vim_state.register.or(register);

    let was_replaying = ctx.vim_state.replaying_change;
    ctx.vim_state.replaying_change = true;
    let action = match command {
        RepeatCommand::Direct { command, count } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            handle_normal_char(command, ctx)
        }
        RepeatCommand::Replace { count, replacement } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            handle_waiting_r(RepeatKey::Char(replacement).key_event(), ctx)
        }
        RepeatCommand::OpMotion { op, motion, count } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            execute_op_motion(motion.key_event(), op, ctx)
        }
        RepeatCommand::OpFind {
            op,
            kind,
            target,
            count,
        } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            handle_waiting_op_find(RepeatKey::Char(target).key_event(), op, kind, ctx)
        }
        RepeatCommand::Linewise { op, count } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            execute_linewise_op(op, ctx)
        }
        RepeatCommand::JoinWithoutSpaces { count } => {
            let n = ctx.vim_state.repeat_count(count);
            join_lines(ctx, n, false);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        RepeatCommand::Add { subtract, count } => {
            let n = ctx.vim_state.repeat_count(count);
            add_to_number(ctx, add_amount(if subtract { 'x' } else { 'a' }, n));
            Action::Consumed
        }
        RepeatCommand::TextObject {
            op,
            inner,
            object,
            count,
        } => {
            let n = ctx.vim_state.repeat_count(count);
            ctx.vim_state.count1 = Some(n);
            handle_waiting_textobj(RepeatKey::Char(object).key_event(), op, inner, ctx)
        }
        RepeatCommand::Visual { action, shape } => {
            shape.select(ctx);
            visual_command(action, false, ctx)
        }
        RepeatCommand::DeleteSurround { target } => {
            delete_surround(ctx, target);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        RepeatCommand::ChangeSurround {
            target,
            replacement,
        } => {
            change_surround(ctx, target, replacement);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
    };
    if *ctx.mode == VimMode::Insert {
        // The repeated change ended in Insert mode: type what was typed last
        // time, then leave as the user would.
        if let Some(text) = ctx.clipboard.kill_ring.last_insert().map(str::to_owned) {
            let start = ctx.insert_str(*ctx.cpos, &text);
            *ctx.cpos = start + text.len();
        }
        enter_normal(ctx);
    }
    ctx.vim_state.replaying_change = was_replaying;
    action
}

pub(super) fn handle_normal_char(c: char, ctx: &mut VimContext<'_>) -> Action {
    if c != 'j' && c != 'k' && !c.is_ascii_digit() {
        *ctx.curswant = None;
    }

    if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count1.is_some()) {
        ctx.vim_state.count1 =
            Some(ctx.vim_state.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
        return Action::Consumed;
    }

    match c {
        // ── Repeat last change ───────────────────────────────────────
        '.' => repeat_last_change(ctx),
        '"' => {
            ctx.vim_state.sub = SubState::WaitingRegister;
            Action::Consumed
        }

        // ── Operators ───────────────────────────────────────────────
        'd' => {
            ctx.vim_state.sub = SubState::WaitingOp(Op::Delete);
            Action::Consumed
        }
        'c' => {
            ctx.vim_state.sub = SubState::WaitingOp(Op::Change);
            Action::Consumed
        }
        'y' => {
            ctx.vim_state.sub = SubState::WaitingOp(Op::Yank);
            Action::Consumed
        }
        '>' => {
            ctx.vim_state.sub = SubState::WaitingOp(Op::Indent);
            Action::Consumed
        }
        '<' => {
            ctx.vim_state.sub = SubState::WaitingOp(Op::Dedent);
            Action::Consumed
        }

        // ── Operator shortcuts ──────────────────────────────────────
        'D' => shortcut_operator(ctx, c, Op::Delete, '$'),
        'C' => shortcut_operator(ctx, c, Op::Change, '$'),
        'Y' => shortcut_operator(ctx, c, Op::Yank, '$'),

        // ── Direct edits ────────────────────────────────────────────
        'x' => shortcut_operator(ctx, c, Op::Delete, 'l'),
        'X' => shortcut_operator(ctx, c, Op::Delete, 'h'),
        's' => shortcut_operator(ctx, c, Op::Change, 'l'),
        'S' => {
            let count = ctx.vim_state.count1.unwrap_or(1);
            let action = execute_linewise_op(Op::Change, ctx);
            ctx.vim_state
                .record_change(RepeatCommand::Direct { command: c, count });
            action
        }
        'r' => {
            ctx.vim_state.sub = SubState::WaitingR;
            Action::Consumed
        }
        '~' => {
            let n = ctx.vim_state.take_count();
            let start = *ctx.cpos;
            let eol = line_end(ctx.buf.as_str(), start);
            if start < eol {
                let end = advance_chars(ctx.buf.as_str(), start, n).min(eol);
                let toggled = Op::Toggle
                    .recase(smelt_buffer::text::slice(ctx.buf.as_str(), start..end))
                    .unwrap_or_default();
                ctx.save_undo();
                ctx.replace_range(start, end, &toggled);
                *ctx.cpos = start + toggled.len();
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
            }
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: '~',
                count: n,
            });
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        // ── Paste ───────────────────────────────────────────────────
        'p' | 'P' => {
            let count = ctx.vim_state.take_count();
            paste(ctx, c == 'p', count);
            ctx.vim_state
                .record_change(RepeatCommand::Direct { command: c, count });
            Action::Consumed
        }

        // ── Undo / Redo ─────────────────────────────────────────────
        'u' => {
            ctx.undo();
            Action::Consumed
        }

        // ── Visual mode ─────────────────────────────────────────────
        'v' => {
            ctx.vim_state.visual_anchor = *ctx.cpos;
            *ctx.mode = VimMode::Visual;
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        'V' => {
            ctx.vim_state.visual_anchor = *ctx.cpos;
            *ctx.mode = VimMode::VisualLine;
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        // ── Enter insert mode ───────────────────────────────────────
        'i' | 'I' | 'a' | 'A' | 'o' | 'O' => {
            let count = ctx.vim_state.take_count();
            ctx.save_undo();
            match c {
                'I' => *ctx.cpos = first_non_blank(ctx.buf.as_str(), *ctx.cpos),
                'a' if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() => {
                    *ctx.cpos = advance_chars(ctx.buf.as_str(), *ctx.cpos, 1)
                        .min(line_end(ctx.buf.as_str(), *ctx.cpos));
                }
                'A' => *ctx.cpos = line_end(ctx.buf.as_str(), *ctx.cpos),
                'o' | 'O' => open_line(ctx, c == 'o'),
                _ => {}
            }
            ctx.vim_state.insert_count = count;
            enter_insert_mode(ctx);
            ctx.vim_state
                .record_change(RepeatCommand::Direct { command: c, count });
            Action::Consumed
        }

        // ── Find ────────────────────────────────────────────────────
        'f' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::Forward);
            Action::Consumed
        }
        'F' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::Backward);
            Action::Consumed
        }
        't' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::ForwardTill);
            Action::Consumed
        }
        'T' => {
            ctx.vim_state.sub = SubState::WaitingFind(FindKind::BackwardTill);
            Action::Consumed
        }

        // ── Wait-for-second-char ────────────────────────────────────
        'g' => {
            ctx.vim_state.sub = SubState::WaitingG;
            Action::Consumed
        }
        'z' => {
            ctx.vim_state.sub = SubState::WaitingZ;
            Action::Consumed
        }

        // ── Motions ─────────────────────────────────────────────────
        'j' => {
            let n = ctx.vim_state.take_count();
            if ctx.buf.contains('\n') {
                let (new_pos, col) = move_down_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                if new_pos == *ctx.cpos && n <= 1 {
                    ctx.vim_state.reset_pending();
                    return Action::HistoryNext;
                }
                *ctx.curswant = Some(col);
                *ctx.cpos = new_pos;
                for _ in 1..n {
                    (*ctx.cpos, _) = move_down_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                }
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
                return Action::Consumed;
            }
            ctx.vim_state.reset_pending();
            if n <= 1 {
                Action::HistoryNext
            } else {
                Action::Consumed
            }
        }
        'k' => {
            let n = ctx.vim_state.take_count();
            if ctx.buf.contains('\n') {
                let (new_pos, col) = move_up_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                if new_pos == *ctx.cpos && n <= 1 {
                    ctx.vim_state.reset_pending();
                    return Action::HistoryPrev;
                }
                *ctx.curswant = Some(col);
                *ctx.cpos = new_pos;
                for _ in 1..n {
                    (*ctx.cpos, _) = move_up_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                }
                clamp_normal(ctx.buf.as_str(), ctx.cpos);
                return Action::Consumed;
            }
            ctx.vim_state.reset_pending();
            if n <= 1 {
                Action::HistoryPrev
            } else {
                Action::Consumed
            }
        }
        ';' | ',' => {
            if let Some(motion) = Motion::repeat_find(ctx.vim_state.last_find, c == ',') {
                move_cursor(ctx, motion);
            }
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        'J' => {
            let count = ctx.vim_state.take_count();
            join_lines(ctx, count, true);
            ctx.vim_state.record_change(RepeatCommand::Direct {
                command: 'J',
                count,
            });
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        _ => {
            if let Some(motion) = Motion::from_char(c) {
                move_cursor(ctx, motion);
            }
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
    }
}

/// Move the cursor with `motion` in normal or visual mode, consuming the typed
/// count. A motion that cannot move leaves the cursor in place.
pub(super) fn move_cursor(ctx: &mut VimContext<'_>, motion: Motion) {
    let count = ctx.vim_state.take_typed_count();
    let column = (motion.keeps_column(count) && !matches!(motion, Motion::Up | Motion::Down))
        .then(|| cursor_cell(ctx));
    *ctx.curswant = if motion == Motion::LineEnd {
        Some(LINE_END_COLUMN)
    } else {
        column
    };
    // Visual `l` may step onto the line break, as an operator's `l` reaches it.
    let usage =
        if matches!(*ctx.mode, VimMode::Visual | VimMode::VisualLine) && motion == Motion::Right {
            Use::Operator
        } else {
            Use::Move
        };
    if let Some((pos, _)) = resolve(ctx.buf.as_str(), *ctx.cpos, motion, count, usage) {
        match column {
            Some(cell) => place_on_line(ctx, pos, cell),
            None => {
                *ctx.cpos = pos;
                clamp_cursor(ctx);
            }
        }
    }
}

/// `o` / `O`: open an empty line below or above the cursor's, carrying over
/// the cursor line's indent, and put the cursor on it.
pub(super) fn open_line(ctx: &mut VimContext<'_>, below: bool) {
    let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
    let indent_end = first_non_blank_at(ctx.buf.as_str(), sol);
    let indent = ctx.buf.as_str()[sol..indent_end].to_string();
    if below {
        let eol = line_end(ctx.buf.as_str(), *ctx.cpos);
        let at = ctx.buf.insert(eol, '\n');
        let start = ctx.insert_str(at + 1, &indent);
        *ctx.cpos = start + indent.len();
    } else {
        let start = ctx.insert_str(sol, &format!("{indent}\n"));
        *ctx.cpos = start + indent.len();
    }
    ctx.vim_state.auto_indent = indent.len();
    ctx.vim_state.opens_line = true;
}

/// `p` (after the cursor, or after its line for a linewise register) and `P`
/// (before), repeated `count` times.
pub(super) fn paste(ctx: &mut VimContext<'_>, after: bool, count: usize) {
    let (text, linewise) = ctx.put_text();
    if text.is_empty() && !linewise {
        return;
    }
    ctx.save_undo();
    if linewise {
        let lines = vec![text.as_str(); count].join("\n");
        let first_line = if after {
            let eol = line_end(ctx.buf.as_str(), *ctx.cpos);
            ctx.insert_str(eol, &format!("\n{lines}")) + 1
        } else {
            let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
            ctx.insert_str(sol, &format!("{lines}\n"))
        };
        *ctx.cpos = first_non_blank_at(ctx.buf.as_str(), first_line);
        return;
    }
    let text = text.repeat(count);
    let at = if after {
        advance_chars(ctx.buf.as_str(), *ctx.cpos, 1).min(line_end(ctx.buf.as_str(), *ctx.cpos))
    } else {
        *ctx.cpos
    };
    let start = ctx.insert_str(at, &text);
    // The cursor ends on the last pasted character, or at the start of text
    // that spans lines.
    *ctx.cpos = if text.contains('\n') {
        start
    } else {
        prev_grapheme_boundary(ctx.buf.as_str(), start + text.len()).max(start)
    };
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

/// `J` and `gJ`: join `count` lines (at least two) onto the cursor's line.
/// `J` also drops the next line's indent and puts one space between the
/// lines, unless the first ends in blank, the next is empty, or the next
/// starts with `)`. A count past the last line joins what is there.
pub(super) fn join_lines(ctx: &mut VimContext<'_>, count: usize, spaces: bool) {
    let count = count.max(2);
    let mut eol = line_end(ctx.buf.as_str(), *ctx.cpos);
    if eol >= ctx.buf.len() {
        // A count past the last line still "joins" it, which moves to its start.
        if count > 2 {
            *ctx.cpos = line_start(ctx.buf.as_str(), *ctx.cpos);
        }
        return;
    }
    ctx.save_undo();
    let mut join_at = eol;
    for _ in 1..count {
        eol = line_end(ctx.buf.as_str(), join_at);
        if eol >= ctx.buf.len() {
            break;
        }
        let next = eol + 1;
        let text_start = if spaces {
            first_non_blank_at(ctx.buf.as_str(), next)
        } else {
            next
        };
        let next_text = &ctx.buf.as_str()[text_start..line_end(ctx.buf.as_str(), next)];
        let current = &ctx.buf.as_str()[line_start(ctx.buf.as_str(), eol)..eol];
        let separator = if spaces
            && !next_text.is_empty()
            && !next_text.starts_with(')')
            && !current.is_empty()
            && !current.ends_with([' ', '\t'])
        {
            " "
        } else {
            ""
        };
        ctx.replace_range(eol, text_start, separator);
        join_at = eol;
    }
    *ctx.cpos = join_at;
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

/// `D`, `C`, `Y`, `x`, `X` and `s`: an operator over a fixed motion, which
/// takes the typed count and repeats with `.` like the operator it stands for.
pub(super) fn shortcut_operator(
    ctx: &mut VimContext<'_>,
    command: char,
    op: Op,
    motion: char,
) -> Action {
    let count = ctx.vim_state.count1.unwrap_or(1);
    let action = execute_op_motion(
        KeyEvent::new(KeyCode::Char(motion), KeyModifiers::empty()),
        op,
        ctx,
    );
    if op != Op::Yank {
        ctx.vim_state
            .record_change(RepeatCommand::Direct { command, count });
    }
    if !matches!(*ctx.mode, VimMode::Insert) {
        ctx.vim_state.reset_pending();
    }
    action
}
