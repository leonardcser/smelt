use super::*;

pub(super) fn handle_visual(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.remember_visual(*ctx.mode, *ctx.cpos);
    if let SubState::WaitingVisualTextObj(inner) = ctx.vim_state.sub {
        ctx.vim_state.sub = SubState::Ready;
        let count = ctx.vim_state.take_count();
        if let Some(spec) = text_object_spec(key, inner) {
            let area = VisualArea {
                anchor: ctx.vim_state.visual_anchor,
                linewise: *ctx.mode == VimMode::VisualLine,
            };
            match select_object(ctx.buf.as_str(), *ctx.cpos, Some(area), spec, count) {
                Ok(sel) => {
                    ctx.vim_state.visual_anchor = sel.start;
                    *ctx.cpos = sel.end;
                    *ctx.mode = if sel.linewise {
                        VimMode::VisualLine
                    } else {
                        VimMode::Visual
                    };
                }
                Err(stranded) => {
                    ctx.vim_state.visual_anchor = stranded.anchor.unwrap_or(area.anchor);
                    *ctx.cpos = stranded.cursor;
                }
            }
        }
        return Action::Consumed;
    }
    if let SubState::WaitingR = ctx.vim_state.sub {
        ctx.vim_state.sub = SubState::Ready;
        if let KeyCode::Char(c) = key.code {
            if c != ATTACHMENT_MARKER {
                return visual_command(VisualAction::Replace(c), false, ctx);
            }
        }
        exit_visual(ctx);
        return Action::Consumed;
    }
    if let SubState::WaitingFind(kind) = ctx.vim_state.sub {
        return handle_waiting_find(key, kind, ctx);
    }
    if let SubState::WaitingG = ctx.vim_state.sub {
        return handle_waiting_g(key, ctx);
    }
    if let SubState::WaitingZ = ctx.vim_state.sub {
        ctx.vim_state.sub = SubState::Ready;
        return match key.code {
            KeyCode::Char('z') => Action::CenterScroll,
            KeyCode::Char('h') => Action::PanColumns(-1),
            KeyCode::Char('l') => Action::PanColumns(1),
            _ => Action::Consumed,
        };
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char(c @ ('a' | 'x')) => {
                let amount = add_amount(c, ctx.vim_state.take_count());
                visual_command(VisualAction::Add(amount), false, ctx)
            }
            _ => Action::Passthrough,
        };
    }

    if let KeyCode::Char(c) = key.code {
        if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count1.is_some()) {
            ctx.vim_state.count1 =
                Some(ctx.vim_state.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
            return Action::Consumed;
        }
        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
            return handle_visual_char(c, ctx);
        }
    }

    match key.code {
        KeyCode::Esc => {
            exit_visual(ctx);
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
        KeyCode::Up => {
            *ctx.cpos = move_up(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Down => {
            *ctx.cpos = move_down(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Home => {
            *ctx.cpos = line_start(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        KeyCode::End => {
            *ctx.cpos = line_end_normal(ctx.buf.as_str(), *ctx.cpos);
            Action::Consumed
        }
        _ => Action::Consumed,
    }
}

/// The column `$` wants, which every line is shorter than.
pub(super) const LINE_END_COLUMN: usize = u16::MAX as usize;

/// What a Visual command does to the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VisualAction {
    Operator(Op),
    Replace(char),
    Join {
        spaces: bool,
    },
    /// Ctrl-A and Ctrl-X: add this to the first number on each line.
    Add(i64),
}

/// The size of a selection, which `.` selects again from the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VisualShape {
    linewise: bool,
    lines: usize,
    /// The display width of a one-line selection, or the column the last line
    /// of a longer one reaches; `usize::MAX` after `$`.
    width: usize,
    /// Whether the selection ends on the line break.
    to_break: bool,
}

impl VisualShape {
    pub(super) fn of(ctx: &VimContext<'_>) -> Option<Self> {
        let (start, end) = visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)?;
        let buf = ctx.buf.as_str();
        let end = end.min(buf.len());
        let linewise = *ctx.mode == VimMode::VisualLine;
        let last = prev_grapheme_boundary(buf, end).max(start);
        let to_break = !linewise && end > start && buf[last..].starts_with('\n');
        let body_end = if to_break { last } else { end };
        let lines = buf[start..body_end].matches('\n').count() + 1;
        let width = if to_break && *ctx.curswant == Some(LINE_END_COLUMN) {
            usize::MAX
        } else if lines == 1 {
            byte_to_cell(&buf[start..body_end], body_end - start) + usize::from(to_break)
        } else {
            let sol = line_start(buf, last);
            byte_to_cell(&buf[sol..line_end(buf, last)], last - sol)
        };
        Some(Self {
            linewise,
            lines,
            width,
            to_break,
        })
    }

    /// Select this much text starting at the cursor.
    pub(super) fn select(self, ctx: &mut VimContext<'_>) {
        let buf = ctx.buf.as_str();
        let anchor = *ctx.cpos;
        let mut last_line = anchor;
        for _ in 1..self.lines {
            let eol = line_end(buf, last_line);
            if eol >= buf.len() {
                break;
            }
            last_line = eol + 1;
        }
        let sol = line_start(buf, last_line);
        let eol = line_end(buf, last_line);
        let line = &buf[sol..eol];
        let anchor_sol = line_start(buf, anchor);
        let anchor_cell = byte_to_cell(&buf[anchor_sol..anchor], anchor - anchor_sol);
        let cell = if self.lines == 1 {
            anchor_cell.saturating_add(self.width.saturating_sub(1))
        } else {
            self.width
        };
        let end = if self.linewise {
            sol
        } else if self.width == usize::MAX {
            eol
        } else {
            let at = sol + cell_to_byte(line, cell);
            if at < eol {
                at
            } else if self.to_break {
                eol
            } else {
                prev_grapheme_boundary(buf, eol).max(sol)
            }
        };
        // Linewise commands keep the column the cursor started in.
        *ctx.curswant = Some(anchor_cell);
        ctx.vim_state.visual_anchor = anchor;
        *ctx.cpos = end;
        *ctx.mode = if self.linewise {
            VimMode::VisualLine
        } else {
            VimMode::Visual
        };
    }
}

/// Run `action` over the selection and leave Visual mode, remembering it for
/// `.`. `whole_lines` widens a charwise selection to its lines first (`D`,
/// `X`, `C`, `S`, `R`, `Y`).
pub(super) fn visual_command(
    action: VisualAction,
    whole_lines: bool,
    ctx: &mut VimContext<'_>,
) -> Action {
    if whole_lines {
        *ctx.mode = VimMode::VisualLine;
    }
    if action != VisualAction::Operator(Op::Yank) {
        if let Some(shape) = VisualShape::of(ctx) {
            ctx.vim_state
                .record_change(RepeatCommand::Visual { action, shape });
        }
    }
    match action {
        VisualAction::Operator(op) => visual_operator(op, ctx),
        VisualAction::Replace(c) => {
            visual_replace(c, ctx);
            exit_visual(ctx);
            Action::Consumed
        }
        VisualAction::Join { spaces } => visual_join(ctx, spaces),
        VisualAction::Add(amount) => visual_add(ctx, amount),
    }
}

/// The selection as the span an operator takes.
pub(super) fn visual_span(ctx: &VimContext<'_>) -> Option<Span> {
    let (start, end) = visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)?;
    Some(if *ctx.mode == VimMode::VisualLine {
        Span::Lines(start, end)
    } else {
        Span::Chars(start, end)
    })
}

pub(super) fn visual_operator(op: Op, ctx: &mut VimContext<'_>) -> Action {
    let span = visual_span(ctx);
    // A count shifts the selected lines that many times.
    let times = ctx.vim_state.take_count();
    exit_visual(ctx);
    // Deleting from an empty buffer leaves the register alone.
    if op == Op::Delete && ctx.buf.is_empty() {
        return Action::Consumed;
    }
    match span {
        Some(
            Span::Chars(start, end) | Span::Lines(start, end) | Span::AdjustedLines(start, end),
        ) if op.recase("").is_some() => {
            recase_range(op, ctx, start, end, start);
            Action::Consumed
        }
        Some(Span::Chars(start, end)) if matches!(op, Op::Indent | Op::Dedent) => {
            shift_lines(
                op,
                ctx,
                start,
                last_char(ctx.buf.as_str(), start, end),
                times,
            );
            Action::Consumed
        }
        Some(Span::Lines(start, end) | Span::AdjustedLines(start, end))
            if matches!(op, Op::Indent | Op::Dedent) =>
        {
            shift_lines(op, ctx, start, end, times);
            Action::Consumed
        }
        Some(Span::Chars(start, end)) => {
            // The change starts at the selection's start, which undo returns to.
            *ctx.cpos = start;
            apply_charwise_op(op, ctx, start, end)
        }
        Some(Span::Lines(start, end) | Span::AdjustedLines(start, end)) => {
            if op == Op::Yank {
                *ctx.cpos = start;
            }
            apply_linewise_op(op, ctx, start, end, true)
        }
        None => Action::Consumed,
    }
}

/// Visual `J` and `gJ`: join the selected lines, or the line and the next
/// one when the selection is on a single line.
pub(super) fn visual_join(ctx: &mut VimContext<'_>, spaces: bool) -> Action {
    if let (Some(shape), Some((start, _))) = (
        VisualShape::of(ctx),
        visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode),
    ) {
        *ctx.cpos = start;
        exit_visual(ctx);
        join_lines(ctx, shape.lines, spaces);
    } else {
        exit_visual(ctx);
    }
    Action::Consumed
}

/// Visual `r{char}`: every character of the selection becomes `c`, line
/// breaks stay.
pub(super) fn visual_replace(c: char, ctx: &mut VimContext<'_>) {
    let Some((start, end)) = visual_range(ctx.vim_state, ctx.buf.as_str(), *ctx.cpos, *ctx.mode)
    else {
        return;
    };
    let end = end.min(ctx.buf.len());
    let buf = ctx.buf.as_str();
    let mut replaced = String::new();
    let mut pos = start;
    while pos < end {
        let next = next_grapheme_boundary(buf, pos).min(end);
        replaced.push(if &buf[pos..next] == "\n" { '\n' } else { c });
        pos = next;
    }
    *ctx.cpos = start;
    ctx.save_undo();
    ctx.replace_range(start, end, &replaced);
}

/// Visual `p` and `P`: replace the selection with the register. `p` leaves
/// the replaced text in the register.
pub(super) fn visual_put(keep_register: bool, ctx: &mut VimContext<'_>) -> Action {
    let (text, put_lines) = ctx.put_text();
    let span = visual_span(ctx);
    exit_visual(ctx);
    let Some(span) = span.filter(|_| !text.is_empty() || put_lines) else {
        return Action::Consumed;
    };
    let (Span::Chars(start, end) | Span::Lines(start, end) | Span::AdjustedLines(start, end)) =
        span;
    let selected_lines = matches!(span, Span::Lines(..));
    let end = end.min(ctx.buf.len());
    let replaced = ctx.buf.as_str()[start..end].to_string();
    // Lines put into a charwise selection split it; text put into whole lines
    // takes their place.
    let put = if put_lines && !selected_lines {
        format!("\n{text}\n")
    } else {
        text
    };
    *ctx.cpos = start;
    ctx.save_undo();
    ctx.replace_range(start, end, &put);
    let first = if put_lines && !selected_lines {
        start + 1
    } else {
        start
    };
    *ctx.cpos = if put_lines || selected_lines {
        first_non_blank_at(ctx.buf.as_str(), first)
    } else if put.contains('\n') {
        start
    } else {
        prev_grapheme_boundary(ctx.buf.as_str(), start + put.len()).max(start)
    };
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
    if !keep_register {
        ctx.clipboard
            .kill_ring
            .set_with_linewise(replaced.clone(), selected_lines);
        if ctx.clipboard.write(&replaced).is_ok() {
            ctx.clipboard.kill_ring.record_clipboard_write(replaced);
        }
    }
    Action::Consumed
}

pub(super) fn handle_visual_char(c: char, ctx: &mut VimContext<'_>) -> Action {
    match c {
        '"' => {
            ctx.vim_state.sub = SubState::WaitingRegister;
            Action::Consumed
        }
        // ── Escape visual mode ─────────────────────────────────────
        'v' if *ctx.mode == VimMode::Visual => {
            exit_visual(ctx);
            Action::Consumed
        }
        'V' if *ctx.mode == VimMode::VisualLine => {
            exit_visual(ctx);
            Action::Consumed
        }
        // Switch between visual modes
        'v' if *ctx.mode == VimMode::VisualLine => {
            *ctx.mode = VimMode::Visual;
            Action::Consumed
        }
        'V' if *ctx.mode == VimMode::Visual => {
            *ctx.mode = VimMode::VisualLine;
            Action::Consumed
        }

        // ── Operators on selection ──────────────────────────────────
        'd' | 'x' => visual_command(VisualAction::Operator(Op::Delete), false, ctx),
        'D' | 'X' => visual_command(VisualAction::Operator(Op::Delete), true, ctx),
        'c' | 's' => visual_command(VisualAction::Operator(Op::Change), false, ctx),
        'C' | 'S' | 'R' => visual_command(VisualAction::Operator(Op::Change), true, ctx),
        'y' => visual_command(VisualAction::Operator(Op::Yank), false, ctx),
        'Y' => visual_command(VisualAction::Operator(Op::Yank), true, ctx),
        '>' => visual_command(VisualAction::Operator(Op::Indent), false, ctx),
        '<' => visual_command(VisualAction::Operator(Op::Dedent), false, ctx),

        // ── Case change on selection ───────────────────────────────
        '~' => visual_command(VisualAction::Operator(Op::Toggle), false, ctx),
        'U' => visual_command(VisualAction::Operator(Op::Upper), false, ctx),
        'u' => visual_command(VisualAction::Operator(Op::Lower), false, ctx),

        // ── Join lines ─────────────────────────────────────────────
        'J' => visual_command(VisualAction::Join { spaces: true }, false, ctx),

        // ── Paste over selection ───────────────────────────────────
        'p' => visual_put(false, ctx),
        'P' => visual_put(true, ctx),

        // ── Motions (move cursor, anchor stays) ────────────────────
        // ── Motions (move cursor, anchor stays) ────────────────────
        'j' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                let col;
                (*ctx.cpos, col) = move_down_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                *ctx.curswant = Some(col);
            }
            clamp_cursor(ctx);
            Action::Consumed
        }
        'k' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                let col;
                (*ctx.cpos, col) = move_up_col(ctx.buf.as_str(), *ctx.cpos, *ctx.curswant);
                *ctx.curswant = Some(col);
            }
            clamp_cursor(ctx);
            Action::Consumed
        }
        'g' => {
            ctx.vim_state.sub = SubState::WaitingG;
            Action::Consumed
        }
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
        ';' | ',' => {
            if let Some(motion) = Motion::repeat_find(ctx.vim_state.last_find, c == ',') {
                move_cursor(ctx, motion);
            }
            Action::Consumed
        }

        // ── Count digits ───────────────────────────────────────────
        c if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count1.is_some()) => {
            ctx.vim_state.count1 =
                Some(ctx.vim_state.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
            Action::Consumed
        }

        'r' => {
            ctx.vim_state.sub = SubState::WaitingR;
            Action::Consumed
        }

        // ── Swap anchor and cursor ─────────────────────────────────
        'o' => {
            // Snap through `visual_anchor_at` first: the raw field outlives
            // buffer mutations (e.g. a paste that replaces the visual
            // selection shrinks `source` below the old anchor), and a raw
            // swap would land `cpos` past `source.len()`.
            let anchor = ctx.vim_state.visual_anchor_at(ctx.buf.as_str());
            ctx.vim_state.visual_anchor = *ctx.cpos;
            *ctx.cpos = anchor;
            *ctx.curswant = None;
            Action::Consumed
        }

        // ── Text objects (iw, aw, i", a( etc.) ────────────────────
        'i' => {
            ctx.vim_state.sub = SubState::WaitingVisualTextObj(true);
            Action::Consumed
        }
        'a' => {
            ctx.vim_state.sub = SubState::WaitingVisualTextObj(false);
            Action::Consumed
        }

        _ => {
            if let Some(motion) = Motion::from_char(c) {
                move_cursor(ctx, motion);
            }
            Action::Consumed
        }
    }
}
