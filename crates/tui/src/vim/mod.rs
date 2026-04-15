mod motions;
mod text_objects;

pub(crate) use motions::{move_down, move_down_col, move_up, move_up_col};

use crate::attachment::AttachmentId;
use crate::text_utils::{
    char_class, line_end, line_start, word_backward_pos, word_forward_pos, CharClass,
};
use crate::undo::{UndoEntry, UndoHistory};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use motions::{
    advance_chars, clamp_normal, current_line_content_range, current_line_range, find_char,
    find_matching_bracket, first_non_blank, first_non_blank_at, goto_line, line_end_normal,
    move_left, move_right_inclusive, move_right_normal, next_char_boundary, prev_char_boundary,
    repeat_find, retreat_chars, word_end_pos,
};
use text_objects::text_object;

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ViMode {
    Insert,
    Normal,
    Visual,
    VisualLine,
}

/// What the caller should do after a key is processed.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// Key consumed, buf/cpos may have changed.
    Consumed,
    /// Submit the input (Enter).
    Submit,
    /// Navigate history up.
    HistoryPrev,
    /// Navigate history down.
    HistoryNext,
    /// Open buffer in $EDITOR.
    EditInEditor,
    /// Center the input viewport on the cursor (zz).
    CenterScroll,
    /// Key not handled — caller should use its own logic.
    Passthrough,
}

// ── Internal types ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Delete,
    Change,
    Yank,
}

impl Op {
    fn char(self) -> char {
        match self {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
        }
    }
}

#[derive(Clone, Copy)]
enum FindKind {
    Forward,
    ForwardTill,
    Backward,
    BackwardTill,
}

impl FindKind {
    fn reversed(self) -> Self {
        match self {
            FindKind::Forward => FindKind::Backward,
            FindKind::ForwardTill => FindKind::BackwardTill,
            FindKind::Backward => FindKind::Forward,
            FindKind::BackwardTill => FindKind::ForwardTill,
        }
    }
}

#[derive(Clone, Copy)]
enum SubState {
    Ready,
    WaitingOp(Op),
    WaitingG,
    WaitingZ,
    /// Operator pending + `g` pressed, waiting for `g` to complete `gg` motion.
    WaitingOpG(Op),
    WaitingR,
    WaitingFind(FindKind),
    /// Operator pending + find motion (e.g. `df`, `dt`), waiting for the target char.
    WaitingOpFind(Op, FindKind),
    /// Operator + `i`/`a` pressed, waiting for object type char.
    WaitingTextObj(Op, bool),
    /// Visual mode `i`/`a` pressed, waiting for object type char.
    WaitingVisualTextObj(bool),
}

// ── Vim state ───────────────────────────────────────────────────────────────

pub struct Vim {
    mode: ViMode,
    sub: SubState,
    /// Count accumulated before the operator (or before a standalone motion).
    count1: Option<usize>,
    /// Count accumulated after the operator, before the motion.
    count2: Option<usize>,
    last_find: Option<(FindKind, char)>,
    register: String,
    register_linewise: bool,
    history: UndoHistory,
    /// Byte position of the visual mode anchor (where 'v'/'V' was pressed).
    visual_anchor: usize,
    /// Desired column for vertical motions (j/k). Preserved across vertical
    /// moves so the cursor snaps back after passing through short lines.
    /// Cleared by any horizontal motion.
    curswant: Option<usize>,
}

impl Default for Vim {
    fn default() -> Self {
        Self::new()
    }
}

impl Vim {
    pub fn new() -> Self {
        Self {
            mode: ViMode::Insert,
            sub: SubState::Ready,
            count1: None,
            count2: None,
            last_find: None,
            register: String::new(),
            register_linewise: false,
            history: UndoHistory::new(None),
            visual_anchor: 0,
            curswant: None,
        }
    }

    pub fn mode(&self) -> ViMode {
        self.mode
    }

    /// Returns the visual selection range (start, end) as byte offsets when
    /// in visual or visual-line mode. The range is always ordered (start <= end).
    pub fn visual_range(&self, buf: &str, cpos: usize) -> Option<(usize, usize)> {
        match self.mode {
            ViMode::Visual => {
                let anchor = self.visual_anchor.min(buf.len());
                let cursor = cpos.min(buf.len());
                let (a, b) = if anchor <= cursor {
                    (anchor, next_char_boundary(buf, cursor).min(buf.len()))
                } else {
                    (cursor, next_char_boundary(buf, anchor).min(buf.len()))
                };
                Some((a, b))
            }
            ViMode::VisualLine => {
                let anchor = self.visual_anchor.min(buf.len());
                let cursor = cpos.min(buf.len());
                let start = line_start(buf, anchor).min(line_start(buf, cursor));
                let end = line_end(buf, anchor).max(line_end(buf, cursor));
                Some((start, end))
            }
            _ => None,
        }
    }

    pub fn set_mode(&mut self, mode: ViMode) {
        self.mode = mode;
        self.sub = SubState::Ready;
        self.reset_counts();
    }

    /// Get the current register contents (for syncing with the shared kill ring).
    pub fn register(&self) -> &str {
        &self.register
    }

    /// Set the register contents (for syncing from the shared kill ring).
    pub fn set_register(&mut self, text: String) {
        self.register = text;
        self.register_linewise = false;
    }

    /// Process a key event. Mutates `buf`, `cpos`, and `attachments` as needed.
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut Vec<AttachmentId>,
    ) -> Action {
        match self.mode {
            ViMode::Insert => self.handle_insert(key, buf, cpos, attachments),
            ViMode::Normal => self.handle_normal(key, buf, cpos, attachments),
            ViMode::Visual | ViMode::VisualLine => self.handle_visual(key, buf, cpos, attachments),
        }
    }

    // ── Insert mode ─────────────────────────────────────────────────────

    fn handle_insert(
        &mut self,
        key: KeyEvent,
        buf: &mut str,
        cpos: &mut usize,
        _attachments: &mut [AttachmentId],
    ) -> Action {
        match key {
            // Esc or Ctrl+[ → normal mode
            KeyEvent {
                code: KeyCode::Esc, ..
            }
            | KeyEvent {
                code: KeyCode::Char('['),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.enter_normal(buf, cpos);
                Action::Consumed
            }
            // Ctrl+W / Ctrl+U → pass through to main handler (kill ring support).
            KeyEvent {
                code: KeyCode::Char('w' | 'u'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => Action::Passthrough,
            // Ctrl+H → backspace (same as Backspace, but let caller handle)
            KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => Action::Passthrough,
            // Everything else → let caller handle normal insert editing
            _ => Action::Passthrough,
        }
    }

    // ── Normal mode ─────────────────────────────────────────────────────

    fn handle_normal(
        &mut self,
        key: KeyEvent,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut Vec<AttachmentId>,
    ) -> Action {
        // Ctrl+key handling in normal mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('r') => {
                    self.redo(buf, cpos, attachments);
                    return Action::Consumed;
                }
                // Pass through keys that the main handler needs.
                KeyCode::Char(
                    'c' | 'd' | 'u' | 't' | 'k' | 'l' | 'f' | 'b' | 'j' | 'n' | 'p' | 's',
                ) => return Action::Passthrough,
                _ => return Action::Consumed,
            }
        }

        // BackTab passes through for mode toggle.
        if key.code == KeyCode::BackTab {
            return Action::Passthrough;
        }

        // Handle sub-states first.
        match self.sub {
            SubState::WaitingR => return self.handle_waiting_r(key, buf, cpos, attachments),
            SubState::WaitingZ => {
                self.sub = SubState::Ready;
                return if matches!(key.code, KeyCode::Char('z')) {
                    Action::CenterScroll
                } else {
                    Action::Consumed
                };
            }
            SubState::WaitingFind(kind) => return self.handle_waiting_find(key, kind, buf, cpos),
            SubState::WaitingOpFind(op, kind) => {
                return self.handle_waiting_op_find(key, op, kind, buf, cpos, attachments)
            }
            SubState::WaitingG => return self.handle_waiting_g(key, buf, cpos),
            SubState::WaitingOpG(op) => {
                return self.handle_waiting_op_g(key, op, buf, cpos, attachments)
            }
            SubState::WaitingTextObj(op, inner) => {
                return self.handle_waiting_textobj(key, op, inner, buf, cpos, attachments)
            }
            SubState::WaitingOp(op) => {
                // Could be digit, motion, text object prefix (i/a), or same-key (dd/cc/yy).
                if let KeyCode::Char(c) = key.code {
                    // Digit accumulation for count2.
                    if c.is_ascii_digit() && (c != '0' || self.count2.is_some()) {
                        self.count2 =
                            Some(self.count2.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
                        return Action::Consumed;
                    }
                    // Same operator key → linewise (dd, cc, yy).
                    if c == op.char() {
                        return self.execute_linewise_op(op, buf, cpos, attachments);
                    }
                    // Text object prefix.
                    if c == 'i' || c == 'a' {
                        self.sub = SubState::WaitingTextObj(op, c == 'i');
                        return Action::Consumed;
                    }
                }
                // Otherwise try as a motion.
                let result = self.execute_op_motion(key, op, buf, cpos, attachments);
                // Don't reset if execute_op_motion transitioned to a new substate
                // (e.g. WaitingOpFind for df/dt combos).
                if matches!(self.sub, SubState::WaitingOp(_)) {
                    self.reset_pending();
                }
                return result;
            }
            SubState::WaitingVisualTextObj(_) | SubState::Ready => {}
        }

        // Ready state — handle count digits, commands, motions.
        if let KeyCode::Char(c) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                return self.handle_normal_char(c, buf, cpos, attachments);
            }
        }

        // Non-char keys in normal mode.
        match key.code {
            KeyCode::Esc => {
                self.reset_pending();
                Action::Consumed
            }
            KeyCode::Enter => Action::Submit,
            KeyCode::Left => {
                *cpos = move_left(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Right => {
                *cpos = move_right_normal(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Up => Action::HistoryPrev,
            KeyCode::Down => Action::HistoryNext,
            KeyCode::Home => {
                *cpos = line_start(buf, *cpos);
                Action::Consumed
            }
            KeyCode::End => {
                *cpos = line_end_normal(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Backspace => {
                *cpos = move_left(buf, *cpos);
                Action::Consumed
            }
            _ => Action::Consumed,
        }
    }

    fn handle_normal_char(
        &mut self,
        c: char,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut Vec<AttachmentId>,
    ) -> Action {
        // Clear desired column for any non-vertical motion.
        if c != 'j' && c != 'k' && !c.is_ascii_digit() {
            self.curswant = None;
        }

        // Count digit accumulation.
        if c.is_ascii_digit() && (c != '0' || self.count1.is_some()) {
            self.count1 = Some(self.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
            return Action::Consumed;
        }

        match c {
            // ── Operators ───────────────────────────────────────────────
            'd' => {
                self.sub = SubState::WaitingOp(Op::Delete);
                Action::Consumed
            }
            'c' => {
                self.sub = SubState::WaitingOp(Op::Change);
                Action::Consumed
            }
            'y' => {
                self.sub = SubState::WaitingOp(Op::Yank);
                Action::Consumed
            }

            // ── Operator shortcuts ──────────────────────────────────────
            'D' => {
                self.save_undo(buf, *cpos, attachments);
                let end = line_end(buf, *cpos);
                self.yank(&buf[*cpos..end], false);
                buf.drain(*cpos..end);
                clamp_normal(buf, cpos);
                self.reset_pending();
                Action::Consumed
            }
            'C' => {
                self.save_undo(buf, *cpos, attachments);
                let end = line_end(buf, *cpos);
                self.yank(&buf[*cpos..end], false);
                buf.drain(*cpos..end);
                self.enter_insert_mode();
                Action::Consumed
            }
            'Y' => {
                let (start, end) = current_line_range(buf, *cpos);
                self.yank(&buf[start..end], true);
                self.reset_pending();
                Action::Consumed
            }

            // ── Direct edits ────────────────────────────────────────────
            'x' => {
                let n = self.take_count();
                if !buf.is_empty() && *cpos < buf.len() {
                    self.save_undo(buf, *cpos, attachments);
                    let end = advance_chars(buf, *cpos, n);
                    self.yank(&buf[*cpos..end], false);
                    buf.drain(*cpos..end);
                    clamp_normal(buf, cpos);
                }
                self.reset_pending();
                Action::Consumed
            }
            'X' => {
                let n = self.take_count();
                if *cpos > 0 {
                    self.save_undo(buf, *cpos, attachments);
                    let start = retreat_chars(buf, *cpos, n);
                    self.yank(&buf[start..*cpos], false);
                    buf.drain(start..*cpos);
                    *cpos = start;
                    clamp_normal(buf, cpos);
                }
                self.reset_pending();
                Action::Consumed
            }
            's' => {
                let n = self.take_count();
                self.save_undo(buf, *cpos, attachments);
                if !buf.is_empty() && *cpos < buf.len() {
                    let end = advance_chars(buf, *cpos, n);
                    self.yank(&buf[*cpos..end], false);
                    buf.drain(*cpos..end);
                }
                self.enter_insert_mode();
                Action::Consumed
            }
            'S' => {
                self.save_undo(buf, *cpos, attachments);
                let (start, end) = current_line_content_range(buf, *cpos);
                self.yank(&buf[start..end], false);
                buf.drain(start..end);
                *cpos = start;
                self.enter_insert_mode();
                Action::Consumed
            }
            'r' => {
                self.sub = SubState::WaitingR;
                Action::Consumed
            }
            '~' => {
                let n = self.take_count();
                if !buf.is_empty() && *cpos < buf.len() {
                    self.save_undo(buf, *cpos, attachments);
                    for _ in 0..n {
                        if *cpos >= buf.len() {
                            break;
                        }
                        let ch = buf[*cpos..].chars().next().unwrap();
                        let end = *cpos + ch.len_utf8();
                        let toggled: String = if ch.is_uppercase() {
                            ch.to_lowercase().collect()
                        } else {
                            ch.to_uppercase().collect()
                        };
                        buf.replace_range(*cpos..end, &toggled);
                        *cpos += toggled.len();
                    }
                    clamp_normal(buf, cpos);
                }
                self.reset_pending();
                Action::Consumed
            }
            'J' => {
                let count = self.take_count().max(2);
                let eol = line_end(buf, *cpos);
                if eol < buf.len() {
                    self.save_undo(buf, *cpos, attachments);
                    let mut join_pos = *cpos;
                    for _ in 1..count {
                        let after = &buf[join_pos..];
                        if let Some(nl) = after.find('\n') {
                            let abs = join_pos + nl;
                            let mut end = abs + 1;
                            while end < buf.len() && buf.as_bytes()[end] == b' ' {
                                end += 1;
                            }
                            buf.replace_range(abs..end, " ");
                            join_pos = abs;
                        } else {
                            break;
                        }
                    }
                    *cpos = join_pos;
                }
                Action::Consumed
            }

            // ── Paste ───────────────────────────────────────────────────
            'p' => {
                if !self.register.is_empty() {
                    self.save_undo(buf, *cpos, attachments);
                    if self.register_linewise {
                        let eol = line_end(buf, *cpos);
                        let insert = format!("\n{}", self.register);
                        buf.insert_str(eol, &insert);
                        *cpos = eol + 1;
                        // Move to first non-blank.
                        *cpos += buf[*cpos..]
                            .bytes()
                            .take_while(|b| *b == b' ' || *b == b'\t')
                            .count();
                    } else {
                        let after = advance_chars(buf, *cpos, 1).min(buf.len());
                        buf.insert_str(after, &self.register);
                        let paste_end = after + self.register.len();
                        *cpos = prev_char_boundary(buf, paste_end).max(after);
                        clamp_normal(buf, cpos);
                    }
                }
                Action::Consumed
            }
            'P' => {
                if !self.register.is_empty() {
                    self.save_undo(buf, *cpos, attachments);
                    if self.register_linewise {
                        let sol = line_start(buf, *cpos);
                        let insert = format!("{}\n", self.register);
                        buf.insert_str(sol, &insert);
                        *cpos = sol;
                        *cpos += buf[*cpos..]
                            .bytes()
                            .take_while(|b| *b == b' ' || *b == b'\t')
                            .count();
                    } else {
                        buf.insert_str(*cpos, &self.register);
                        let plen = self.register.len();
                        if plen > 0 {
                            let paste_end = *cpos + plen;
                            *cpos = prev_char_boundary(buf, paste_end).max(*cpos);
                            clamp_normal(buf, cpos);
                        }
                    }
                }
                Action::Consumed
            }

            // ── Undo / Redo ─────────────────────────────────────────────
            'u' => {
                self.undo(buf, cpos, attachments);
                Action::Consumed
            }

            // ── Visual mode ─────────────────────────────────────────────
            'v' => {
                self.visual_anchor = *cpos;
                self.mode = ViMode::Visual;
                self.reset_pending();
                Action::Consumed
            }
            'V' => {
                self.visual_anchor = *cpos;
                self.mode = ViMode::VisualLine;
                self.reset_pending();
                Action::Consumed
            }

            // ── Enter insert mode ───────────────────────────────────────
            'i' => {
                self.take_count();
                self.save_undo(buf, *cpos, attachments);
                self.enter_insert_mode();
                Action::Consumed
            }
            'I' => {
                self.take_count();
                self.save_undo(buf, *cpos, attachments);
                *cpos = first_non_blank(buf, *cpos);
                self.enter_insert_mode();
                Action::Consumed
            }
            'a' => {
                self.take_count();
                self.save_undo(buf, *cpos, attachments);
                if !buf.is_empty() && *cpos < buf.len() {
                    *cpos = advance_chars(buf, *cpos, 1);
                }
                self.enter_insert_mode();
                Action::Consumed
            }
            'A' => {
                self.take_count();
                self.save_undo(buf, *cpos, attachments);
                *cpos = line_end(buf, *cpos);
                self.enter_insert_mode();
                Action::Consumed
            }
            'o' => {
                self.save_undo(buf, *cpos, attachments);
                let eol = line_end(buf, *cpos);
                buf.insert(eol, '\n');
                *cpos = eol + 1;
                self.enter_insert_mode();
                Action::Consumed
            }
            'O' => {
                self.save_undo(buf, *cpos, attachments);
                let sol = line_start(buf, *cpos);
                buf.insert(sol, '\n');
                *cpos = sol;
                self.enter_insert_mode();
                Action::Consumed
            }

            // ── Find ────────────────────────────────────────────────────
            'f' => {
                self.sub = SubState::WaitingFind(FindKind::Forward);
                Action::Consumed
            }
            'F' => {
                self.sub = SubState::WaitingFind(FindKind::Backward);
                Action::Consumed
            }
            't' => {
                self.sub = SubState::WaitingFind(FindKind::ForwardTill);
                Action::Consumed
            }
            'T' => {
                self.sub = SubState::WaitingFind(FindKind::BackwardTill);
                Action::Consumed
            }
            ';' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    *cpos = repeat_find(buf, *cpos, kind, ch, n);
                }
                self.reset_pending();
                Action::Consumed
            }
            ',' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    *cpos = repeat_find(buf, *cpos, kind.reversed(), ch, n);
                }
                self.reset_pending();
                Action::Consumed
            }

            // ── Wait-for-second-char ────────────────────────────────────
            'g' => {
                self.sub = SubState::WaitingG;
                Action::Consumed
            }
            'z' => {
                self.sub = SubState::WaitingZ;
                Action::Consumed
            }

            // ── Motions ─────────────────────────────────────────────────
            'h' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = move_left(buf, *cpos);
                }
                Action::Consumed
            }
            'l' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = move_right_normal(buf, *cpos);
                }
                Action::Consumed
            }
            'j' => {
                let n = self.take_count();
                if buf.contains('\n') {
                    let (new_pos, col) = move_down_col(buf, *cpos, self.curswant);
                    if new_pos == *cpos && n <= 1 {
                        self.reset_pending();
                        return Action::HistoryNext;
                    }
                    self.curswant = Some(col);
                    *cpos = new_pos;
                    for _ in 1..n {
                        (*cpos, _) = move_down_col(buf, *cpos, self.curswant);
                    }
                    clamp_normal(buf, cpos);
                    return Action::Consumed;
                }
                self.reset_pending();
                if n <= 1 {
                    Action::HistoryNext
                } else {
                    Action::Consumed
                }
            }
            'k' => {
                let n = self.take_count();
                if buf.contains('\n') {
                    let (new_pos, col) = move_up_col(buf, *cpos, self.curswant);
                    if new_pos == *cpos && n <= 1 {
                        self.reset_pending();
                        return Action::HistoryPrev;
                    }
                    self.curswant = Some(col);
                    *cpos = new_pos;
                    for _ in 1..n {
                        (*cpos, _) = move_up_col(buf, *cpos, self.curswant);
                    }
                    clamp_normal(buf, cpos);
                    return Action::Consumed;
                }
                self.reset_pending();
                if n <= 1 {
                    Action::HistoryPrev
                } else {
                    Action::Consumed
                }
            }
            'w' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_forward_pos(buf, *cpos, CharClass::Word);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'W' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_forward_pos(buf, *cpos, CharClass::WORD);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'b' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_backward_pos(buf, *cpos, CharClass::Word);
                }
                Action::Consumed
            }
            'B' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_backward_pos(buf, *cpos, CharClass::WORD);
                }
                Action::Consumed
            }
            'e' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_end_pos(buf, *cpos, CharClass::Word);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'E' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_end_pos(buf, *cpos, CharClass::WORD);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            '0' => {
                *cpos = line_start(buf, *cpos);
                self.curswant = None;
                self.reset_pending();
                Action::Consumed
            }
            '^' | '_' => {
                *cpos = first_non_blank(buf, *cpos);
                self.reset_pending();
                Action::Consumed
            }
            '$' => {
                let n = self.take_count();
                // n$ moves down n-1 lines then to end.
                for _ in 1..n {
                    *cpos = move_down(buf, *cpos);
                }
                *cpos = line_end_normal(buf, *cpos);
                Action::Consumed
            }
            'G' => {
                let had_count = self.count1.is_some();
                let n = self.take_count();
                *cpos = if had_count {
                    goto_line(buf, n.saturating_sub(1))
                } else {
                    buf.len()
                };
                clamp_normal(buf, cpos);
                Action::Consumed
            }

            // ── Match bracket ────────────────────────────────────────────
            '%' => {
                self.reset_counts();
                if let Some(p) = find_matching_bracket(buf, *cpos) {
                    *cpos = p;
                }
                Action::Consumed
            }

            // Unknown — swallow it.
            _ => {
                self.reset_pending();
                Action::Consumed
            }
        }
    }

    // ── Visual mode ──────────────────────────────────────────────────────

    fn handle_visual(
        &mut self,
        key: KeyEvent,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
    ) -> Action {
        // Ctrl+key handling in visual mode — pass through the same keys
        // as normal mode so the keymap can handle them (scroll, history, etc.).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(
                'c' | 'd' | 'u' | 't' | 'k' | 'l' | 'f' | 'b' | 'j' | 'n' | 'p' | 's',
            ) = key.code
            {
                return Action::Passthrough;
            }
        }
        // Cmd+key (SUPER) passes through for clipboard operations.
        if key.modifiers.contains(KeyModifiers::SUPER) {
            return Action::Passthrough;
        }

        // BackTab passes through for mode toggle.
        if key.code == KeyCode::BackTab {
            return Action::Passthrough;
        }

        // Handle sub-states (WaitingG for gg, WaitingFind for f/t, text objects).
        match self.sub {
            SubState::WaitingG => return self.handle_waiting_g(key, buf, cpos),
            SubState::WaitingFind(kind) => return self.handle_waiting_find(key, kind, buf, cpos),
            SubState::WaitingVisualTextObj(inner) => {
                self.sub = SubState::Ready;
                if let KeyCode::Char(c) = key.code {
                    if let Some((start, end)) = text_object(buf, *cpos, inner, c) {
                        self.visual_anchor = start;
                        // Cursor on the last char of the object (inclusive).
                        *cpos = if end > start {
                            prev_char_boundary(buf, end)
                        } else {
                            start
                        };
                    }
                }
                return Action::Consumed;
            }
            _ => {}
        }

        if let KeyCode::Char(c) = key.code {
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                return self.handle_visual_char(c, buf, cpos, attachments);
            }
        }

        match key.code {
            KeyCode::Esc => {
                self.exit_visual();
                Action::Consumed
            }
            KeyCode::Left => {
                *cpos = move_left(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Right => {
                *cpos = move_right_normal(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Up => {
                *cpos = move_up(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Down => {
                *cpos = move_down(buf, *cpos);
                Action::Consumed
            }
            KeyCode::Home => {
                *cpos = line_start(buf, *cpos);
                Action::Consumed
            }
            KeyCode::End => {
                *cpos = line_end_normal(buf, *cpos);
                Action::Consumed
            }
            _ => Action::Consumed,
        }
    }

    fn handle_visual_char(
        &mut self,
        c: char,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
    ) -> Action {
        if c != 'j' && c != 'k' && !c.is_ascii_digit() {
            self.curswant = None;
        }
        match c {
            // ── Escape visual mode ─────────────────────────────────────
            'v' if self.mode == ViMode::Visual => {
                self.exit_visual();
                Action::Consumed
            }
            'V' if self.mode == ViMode::VisualLine => {
                self.exit_visual();
                Action::Consumed
            }
            // Switch between visual modes
            'v' if self.mode == ViMode::VisualLine => {
                self.mode = ViMode::Visual;
                Action::Consumed
            }
            'V' if self.mode == ViMode::Visual => {
                self.mode = ViMode::VisualLine;
                Action::Consumed
            }

            // ── Substitute (s → change, S → linewise change) ────────
            's' => {
                // Visual s is the same as c.
                self.handle_visual_char('c', buf, cpos, attachments)
            }
            'S' => {
                // Visual S forces linewise, then changes.
                self.mode = ViMode::VisualLine;
                self.handle_visual_char('c', buf, cpos, attachments)
            }

            // ── Operators on selection ──────────────────────────────────
            'd' | 'x' => {
                if let Some((start, end)) = self.visual_range(buf, *cpos) {
                    let linewise = self.mode == ViMode::VisualLine;
                    self.save_undo(buf, *cpos, attachments);
                    self.yank(&buf[start..end], linewise);
                    if linewise {
                        // Include trailing newline if present.
                        let drain_end = if end < buf.len() && buf.as_bytes()[end] == b'\n' {
                            end + 1
                        } else if start > 0 && buf.as_bytes()[start - 1] == b'\n' {
                            // Last line(s) — remove preceding newline.
                            let s = start - 1;
                            buf.drain(s..end);
                            *cpos = s.min(buf.len());
                            clamp_normal(buf, cpos);
                            if !buf.is_empty() && *cpos < buf.len() {
                                *cpos = first_non_blank_at(buf, line_start(buf, *cpos));
                            }
                            self.exit_visual();
                            return Action::Consumed;
                        } else {
                            end
                        };
                        buf.drain(start..drain_end);
                    } else {
                        buf.drain(start..end);
                    }
                    *cpos = start.min(buf.len());
                    clamp_normal(buf, cpos);
                }
                self.exit_visual();
                Action::Consumed
            }
            'c' => {
                if let Some((start, end)) = self.visual_range(buf, *cpos) {
                    let linewise = self.mode == ViMode::VisualLine;
                    self.save_undo(buf, *cpos, attachments);
                    self.yank(&buf[start..end], linewise);
                    if linewise {
                        // Like cc: clear line content but keep the line structure.
                        // Find the content range (excluding leading/trailing newlines).
                        let content_start = first_non_blank_at(buf, start);
                        buf.drain(content_start..end);
                        *cpos = content_start;
                    } else {
                        buf.drain(start..end);
                        *cpos = start;
                    }
                    self.mode = ViMode::Insert;
                    self.sub = SubState::Ready;
                    self.reset_counts();
                    return Action::Consumed;
                }
                self.exit_visual();
                Action::Consumed
            }
            'y' => {
                if let Some((start, end)) = self.visual_range(buf, *cpos) {
                    let linewise = self.mode == ViMode::VisualLine;
                    self.yank(&buf[start..end], linewise);
                    *cpos = start;
                }
                self.exit_visual();
                Action::Consumed
            }

            // ── Case toggling on selection ─────────────────────────────
            '~' => {
                if let Some((start, end)) = self.visual_range(buf, *cpos) {
                    self.save_undo(buf, *cpos, attachments);
                    let toggled: String = buf[start..end]
                        .chars()
                        .map(|ch| {
                            if ch.is_uppercase() {
                                ch.to_lowercase().next().unwrap_or(ch)
                            } else {
                                ch.to_uppercase().next().unwrap_or(ch)
                            }
                        })
                        .collect();
                    buf.replace_range(start..end, &toggled);
                }
                self.exit_visual();
                Action::Consumed
            }
            'U' => {
                if let Some((start, end)) = self.visual_range(buf, *cpos) {
                    self.save_undo(buf, *cpos, attachments);
                    let upper = buf[start..end].to_uppercase();
                    buf.replace_range(start..end, &upper);
                }
                self.exit_visual();
                Action::Consumed
            }
            'u' => {
                if let Some((start, end)) = self.visual_range(buf, *cpos) {
                    self.save_undo(buf, *cpos, attachments);
                    let lower = buf[start..end].to_lowercase();
                    buf.replace_range(start..end, &lower);
                }
                self.exit_visual();
                Action::Consumed
            }

            // ── Join lines ─────────────────────────────────────────────
            'J' => {
                if let Some((start, end)) = self.visual_range(buf, *cpos) {
                    self.save_undo(buf, *cpos, attachments);
                    let mut pos = start;
                    let mut remaining = end;
                    while pos < remaining.min(buf.len()) {
                        if let Some(nl) = buf[pos..remaining.min(buf.len())].find('\n') {
                            let abs = pos + nl;
                            let mut ws_end = abs + 1;
                            while ws_end < buf.len() && buf.as_bytes()[ws_end] == b' ' {
                                ws_end += 1;
                            }
                            let removed = ws_end - abs;
                            buf.replace_range(abs..ws_end, " ");
                            remaining -= removed - 1; // replaced N chars with 1
                            pos = abs + 1;
                        } else {
                            break;
                        }
                    }
                    *cpos = start;
                }
                self.exit_visual();
                Action::Consumed
            }

            // ── Paste over selection ───────────────────────────────────
            'p' | 'P' => {
                if !self.register.is_empty() {
                    if let Some((start, end)) = self.visual_range(buf, *cpos) {
                        self.save_undo(buf, *cpos, attachments);
                        let old = buf[start..end].to_string();
                        buf.replace_range(start..end, &self.register);
                        *cpos = start;
                        clamp_normal(buf, cpos);
                        // The replaced text goes into register (like vim).
                        self.register = old;
                        self.register_linewise = false;
                    }
                }
                self.exit_visual();
                Action::Consumed
            }

            // ── Motions (move cursor, anchor stays) ────────────────────
            'h' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = move_left(buf, *cpos);
                }
                Action::Consumed
            }
            'l' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = move_right_normal(buf, *cpos);
                }
                Action::Consumed
            }
            'j' => {
                let n = self.take_count();
                for _ in 0..n {
                    let col;
                    (*cpos, col) = move_down_col(buf, *cpos, self.curswant);
                    self.curswant = Some(col);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'k' => {
                let n = self.take_count();
                for _ in 0..n {
                    let col;
                    (*cpos, col) = move_up_col(buf, *cpos, self.curswant);
                    self.curswant = Some(col);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'w' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_forward_pos(buf, *cpos, CharClass::Word);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'W' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_forward_pos(buf, *cpos, CharClass::WORD);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'b' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_backward_pos(buf, *cpos, CharClass::Word);
                }
                Action::Consumed
            }
            'B' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_backward_pos(buf, *cpos, CharClass::WORD);
                }
                Action::Consumed
            }
            'e' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_end_pos(buf, *cpos, CharClass::Word);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            'E' => {
                let n = self.take_count();
                for _ in 0..n {
                    *cpos = word_end_pos(buf, *cpos, CharClass::WORD);
                }
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            '0' => {
                *cpos = line_start(buf, *cpos);
                Action::Consumed
            }
            '^' | '_' => {
                *cpos = first_non_blank(buf, *cpos);
                Action::Consumed
            }
            '$' => {
                *cpos = line_end_normal(buf, *cpos);
                Action::Consumed
            }
            'G' => {
                let had_count = self.count1.is_some();
                let n = self.take_count();
                *cpos = if had_count {
                    goto_line(buf, n.saturating_sub(1))
                } else {
                    buf.len()
                };
                clamp_normal(buf, cpos);
                Action::Consumed
            }
            '%' => {
                self.reset_counts();
                if let Some(p) = find_matching_bracket(buf, *cpos) {
                    *cpos = p;
                }
                Action::Consumed
            }
            'g' => {
                self.sub = SubState::WaitingG;
                Action::Consumed
            }
            'f' => {
                self.sub = SubState::WaitingFind(FindKind::Forward);
                Action::Consumed
            }
            'F' => {
                self.sub = SubState::WaitingFind(FindKind::Backward);
                Action::Consumed
            }
            't' => {
                self.sub = SubState::WaitingFind(FindKind::ForwardTill);
                Action::Consumed
            }
            'T' => {
                self.sub = SubState::WaitingFind(FindKind::BackwardTill);
                Action::Consumed
            }
            ';' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    *cpos = repeat_find(buf, *cpos, kind, ch, n);
                }
                Action::Consumed
            }
            ',' => {
                if let Some((kind, ch)) = self.last_find {
                    let n = self.take_count();
                    *cpos = repeat_find(buf, *cpos, kind.reversed(), ch, n);
                }
                Action::Consumed
            }

            // ── Count digits ───────────────────────────────────────────
            c if c.is_ascii_digit() && (c != '0' || self.count1.is_some()) => {
                self.count1 =
                    Some(self.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
                Action::Consumed
            }

            // ── Swap anchor and cursor ─────────────────────────────────
            'o' => {
                std::mem::swap(&mut self.visual_anchor, cpos);
                Action::Consumed
            }

            // ── Text objects (iw, aw, i", a( etc.) ────────────────────
            'i' => {
                self.sub = SubState::WaitingVisualTextObj(true);
                Action::Consumed
            }
            'a' => {
                self.sub = SubState::WaitingVisualTextObj(false);
                Action::Consumed
            }

            // Unknown — swallow.
            _ => Action::Consumed,
        }
    }

    // ── Sub-state handlers ──────────────────────────────────────────────

    fn handle_waiting_r(
        &mut self,
        key: KeyEvent,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
    ) -> Action {
        self.sub = SubState::Ready;
        let replacement_char = match key.code {
            KeyCode::Char(c) => Some(c),
            KeyCode::Enter => Some('\n'),
            _ => None,
        };
        if let Some(c) = replacement_char {
            if !buf.is_empty() && *cpos < buf.len() {
                let n = self.take_count();
                self.save_undo(buf, *cpos, attachments);
                let mut pos = *cpos;
                for _ in 0..n {
                    if pos >= buf.len() {
                        break;
                    }
                    let old = buf[pos..].chars().next().unwrap();
                    let end = pos + old.len_utf8();
                    let replacement = c.to_string();
                    buf.replace_range(pos..end, &replacement);
                    pos += replacement.len();
                }
                *cpos = prev_char_boundary(buf, pos).max(*cpos);
                clamp_normal(buf, cpos);
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_find(
        &mut self,
        key: KeyEvent,
        kind: FindKind,
        buf: &mut str,
        cpos: &mut usize,
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(ch) = key.code {
            let n = self.take_count();
            self.last_find = Some((kind, ch));
            let mut pos = *cpos;
            for _ in 0..n {
                if let Some(p) = find_char(buf, pos, kind, ch) {
                    pos = p;
                }
            }
            *cpos = pos;
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_op_find(
        &mut self,
        key: KeyEvent,
        op: Op,
        kind: FindKind,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(ch) = key.code {
            let n = self.effective_count();
            self.last_find = Some((kind, ch));
            let origin = *cpos;
            // For operators, always find the actual char position (Forward/Backward),
            // then adjust the range for till variants.
            let raw_kind = match kind {
                FindKind::ForwardTill => FindKind::Forward,
                FindKind::BackwardTill => FindKind::Backward,
                other => other,
            };
            let mut pos = origin;
            for _ in 0..n {
                if let Some(p) = find_char(buf, pos, raw_kind, ch) {
                    pos = p;
                }
            }
            if pos != origin {
                // f is inclusive (include target char), t excludes target char.
                let (start, end) = match kind {
                    FindKind::Forward => (*cpos, advance_chars(buf, pos, 1)),
                    FindKind::ForwardTill => (*cpos, pos),
                    FindKind::Backward => (pos, *cpos),
                    FindKind::BackwardTill => (advance_chars(buf, pos, 1), *cpos),
                };
                if start < end {
                    return self.apply_charwise_op(op, buf, cpos, attachments, start, end);
                }
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_g(&mut self, key: KeyEvent, buf: &mut str, cpos: &mut usize) -> Action {
        self.sub = SubState::Ready;
        let action = match key.code {
            KeyCode::Char('g') => {
                // gg → start of buffer.
                if let Some(n) = self.count1.take() {
                    *cpos = goto_line(buf, n.saturating_sub(1));
                } else {
                    *cpos = 0;
                }
                Action::Consumed
            }
            _ => Action::Consumed,
        };
        self.count1 = None;
        self.count2 = None;
        action
    }

    fn handle_waiting_op_g(
        &mut self,
        key: KeyEvent,
        op: Op,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char('g') = key.code {
            let target = if let Some(n) = self.count1.take() {
                goto_line(buf, n.saturating_sub(1))
            } else {
                0
            };
            let origin = *cpos;
            if target != origin {
                let (s, e) = if target < origin {
                    (target, origin)
                } else {
                    (origin, target)
                };
                let ls = line_start(buf, s);
                let le = line_end(buf, e);
                self.reset_pending();
                return self.apply_linewise_op(op, buf, cpos, attachments, ls, le);
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    fn handle_waiting_textobj(
        &mut self,
        key: KeyEvent,
        op: Op,
        inner: bool,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
    ) -> Action {
        self.sub = SubState::Ready;
        if let KeyCode::Char(c) = key.code {
            if let Some((start, end)) = text_object(buf, *cpos, inner, c) {
                let n = self.effective_count();
                let _ = n;
                return self.apply_charwise_op(op, buf, cpos, attachments, start, end);
            }
        }
        self.reset_pending();
        Action::Consumed
    }

    /// Operator pending + a motion key.
    fn execute_op_motion(
        &mut self,
        key: KeyEvent,
        op: Op,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
    ) -> Action {
        let n = self.effective_count();
        let origin = *cpos;

        // Resolve motion target and whether the motion is linewise.
        let (target, linewise) = match key.code {
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_left(buf, p);
                }
                (Some(p), false)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_right_inclusive(buf, p);
                }
                (Some(p), false)
            }
            KeyCode::Char('j') => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_down(buf, p);
                }
                (Some(p), true)
            }
            KeyCode::Char('k') => {
                let mut p = origin;
                for _ in 0..n {
                    p = move_up(buf, p);
                }
                (Some(p), true)
            }
            KeyCode::Char('w') => {
                let mut p = origin;
                // vim special case: cw behaves like ce when cursor is on a word char.
                let use_end = op == Op::Change
                    && p < buf.len()
                    && char_class(buf[p..].chars().next().unwrap(), CharClass::Word) != 0;
                for _ in 0..n {
                    if use_end {
                        p = word_end_pos(buf, p, CharClass::Word);
                        p = advance_chars(buf, p, 1); // inclusive end
                    } else {
                        p = word_forward_pos(buf, p, CharClass::Word);
                    }
                }
                (Some(p), false)
            }
            KeyCode::Char('W') => {
                let mut p = origin;
                let use_end = op == Op::Change
                    && p < buf.len()
                    && char_class(buf[p..].chars().next().unwrap(), CharClass::WORD) != 0;
                for _ in 0..n {
                    if use_end {
                        p = word_end_pos(buf, p, CharClass::WORD);
                        p = advance_chars(buf, p, 1);
                    } else {
                        p = word_forward_pos(buf, p, CharClass::WORD);
                    }
                }
                (Some(p), false)
            }
            KeyCode::Char('b') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_backward_pos(buf, p, CharClass::Word);
                }
                (Some(p), false)
            }
            KeyCode::Char('B') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_backward_pos(buf, p, CharClass::WORD);
                }
                (Some(p), false)
            }
            KeyCode::Char('e') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_end_pos(buf, p, CharClass::Word);
                }
                (Some(advance_chars(buf, p, 1)), false)
            }
            KeyCode::Char('E') => {
                let mut p = origin;
                for _ in 0..n {
                    p = word_end_pos(buf, p, CharClass::WORD);
                }
                (Some(advance_chars(buf, p, 1)), false)
            }
            KeyCode::Char('0') => (Some(line_start(buf, origin)), false),
            KeyCode::Char('^' | '_') => (Some(first_non_blank(buf, origin)), false),
            KeyCode::Char('$') => (Some(line_end(buf, origin)), false),
            KeyCode::Char('%') => {
                if let Some(t) = find_matching_bracket(buf, origin) {
                    let lo = origin.min(t);
                    let hi = advance_chars(buf, origin.max(t), 1);
                    return self.apply_charwise_op(op, buf, cpos, attachments, lo, hi);
                }
                (None, false)
            }
            KeyCode::Char('G') => (Some(buf.len()), true), // linewise
            KeyCode::Char('g') => {
                self.sub = SubState::WaitingOpG(op);
                return Action::Consumed;
            }
            KeyCode::Char('f') => {
                self.sub = SubState::WaitingOpFind(op, FindKind::Forward);
                return Action::Consumed;
            }
            KeyCode::Char('F') => {
                self.sub = SubState::WaitingOpFind(op, FindKind::Backward);
                return Action::Consumed;
            }
            KeyCode::Char('t') => {
                self.sub = SubState::WaitingOpFind(op, FindKind::ForwardTill);
                return Action::Consumed;
            }
            KeyCode::Char('T') => {
                self.sub = SubState::WaitingOpFind(op, FindKind::BackwardTill);
                return Action::Consumed;
            }
            KeyCode::Home => (Some(line_start(buf, origin)), false),
            KeyCode::End => (Some(line_end(buf, origin)), false),
            _ => (None, false),
        };

        let Some(target) = target else {
            // Invalid motion — cancel.
            return Action::Consumed;
        };

        if linewise {
            // Linewise: delegate to the existing linewise operator logic
            // which handles newline inclusion, first-non-blank, etc.
            let (start, end) = if target < origin {
                (target, origin)
            } else {
                (origin, target)
            };
            // Expand to full lines.
            let ls = line_start(buf, start);
            let le = line_end(buf, end);
            return self.apply_linewise_op(op, buf, cpos, attachments, ls, le);
        }

        let (start, end) = if target < origin {
            (target, origin)
        } else {
            (origin, target)
        };

        if start == end {
            return Action::Consumed;
        }

        self.apply_charwise_op(op, buf, cpos, attachments, start, end)
    }

    fn execute_linewise_op(
        &mut self,
        op: Op,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
    ) -> Action {
        let n = self.effective_count();
        self.reset_counts();
        self.sub = SubState::Ready;

        let start = line_start(buf, *cpos);
        let mut end_pos = *cpos;
        for _ in 1..n {
            let next = line_end(buf, end_pos);
            if next < buf.len() {
                end_pos = next + 1;
            }
        }
        let end = line_end(buf, end_pos);
        self.apply_linewise_op(op, buf, cpos, attachments, start, end)
    }

    /// Apply a charwise operator over the byte range [start..end).
    fn apply_charwise_op(
        &mut self,
        op: Op,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
        start: usize,
        end: usize,
    ) -> Action {
        match op {
            Op::Delete => {
                self.save_undo(buf, *cpos, attachments);
                self.yank(&buf[start..end], false);
                buf.drain(start..end);
                *cpos = start;
                clamp_normal(buf, cpos);
            }
            Op::Change => {
                self.save_undo(buf, *cpos, attachments);
                self.yank(&buf[start..end], false);
                buf.drain(start..end);
                *cpos = start;
                self.enter_insert_mode();
                self.reset_counts();
                return Action::Consumed;
            }
            Op::Yank => {
                self.yank(&buf[start..end], false);
                *cpos = start;
            }
        }
        Action::Consumed
    }

    /// Apply a linewise operator over the content range [start..end].
    /// `start` is the first byte of the first line, `end` is the last byte
    /// of the last line (before its newline). This function handles newline
    /// inclusion at buffer boundaries and cursor placement.
    fn apply_linewise_op(
        &mut self,
        op: Op,
        buf: &mut String,
        cpos: &mut usize,
        attachments: &mut [AttachmentId],
        start: usize,
        end: usize,
    ) -> Action {
        let mut s = start;
        let mut e = end;
        let mut has_trailing_nl = false;
        // Include trailing newline if present.
        if e < buf.len() && buf.as_bytes()[e] == b'\n' {
            e += 1;
            has_trailing_nl = true;
        } else if e < buf.len() {
            e = line_end(buf, e);
            if e < buf.len() {
                e += 1;
                has_trailing_nl = true;
            }
        }
        // At end of buffer with no trailing newline — include preceding
        // newline to avoid leaving a dangling one.
        if !has_trailing_nl && e >= buf.len() && s > 0 {
            s -= 1;
        }

        match op {
            Op::Delete => {
                self.save_undo(buf, *cpos, attachments);
                self.yank(&buf[s..e], true);
                buf.drain(s..e);
                *cpos = s.min(buf.len());
                if !buf.is_empty() && *cpos < buf.len() {
                    *cpos = first_non_blank_at(buf, *cpos);
                }
                clamp_normal(buf, cpos);
            }
            Op::Change => {
                self.save_undo(buf, *cpos, attachments);
                // Clear line content but keep the line structure.
                let content_start = first_non_blank_at(buf, s);
                let content_end = line_end(buf, e.saturating_sub(1).max(s));
                self.yank(&buf[content_start..content_end], true);
                buf.drain(content_start..content_end);
                *cpos = content_start;
                self.enter_insert_mode();
                return Action::Consumed;
            }
            Op::Yank => {
                self.yank(&buf[s..e], true);
                *cpos = s;
            }
        }
        Action::Consumed
    }

    // ── Mode transitions ────────────────────────────────────────────────

    fn enter_insert_mode(&mut self) {
        self.mode = ViMode::Insert;
        self.sub = SubState::Ready;
    }

    fn exit_visual(&mut self) {
        self.mode = ViMode::Normal;
        self.reset_pending();
    }

    fn enter_normal(&mut self, buf: &str, cpos: &mut usize) {
        self.mode = ViMode::Normal;
        self.sub = SubState::Ready;
        self.reset_counts();
        // Standard vim: cursor moves left one when leaving insert mode,
        // unless at the start of a line.
        let sol = line_start(buf, *cpos);
        if *cpos > sol {
            *cpos = prev_char_boundary(buf, *cpos);
        }
        clamp_normal(buf, cpos);
    }

    // ── Undo/redo ───────────────────────────────────────────────────────

    /// Save the current state for undo. Call this before making changes to buf/attachments.
    pub fn save_undo(&mut self, buf: &str, cpos: usize, att: &[AttachmentId]) {
        self.history.save(UndoEntry::snapshot(buf, cpos, att));
    }

    /// Undo to the previous state.
    pub fn undo(&mut self, buf: &mut String, cpos: &mut usize, att: &mut Vec<AttachmentId>) {
        let current = UndoEntry::snapshot(buf, *cpos, att);
        if let Some(entry) = self.history.undo(current) {
            *buf = entry.buf;
            *cpos = entry.cpos;
            *att = entry.attachments;
            clamp_normal(buf, cpos);
        }
    }

    /// Redo to the next state.
    pub fn redo(&mut self, buf: &mut String, cpos: &mut usize, att: &mut Vec<AttachmentId>) {
        let current = UndoEntry::snapshot(buf, *cpos, att);
        if let Some(entry) = self.history.redo(current) {
            *buf = entry.buf;
            *cpos = entry.cpos;
            *att = entry.attachments;
            clamp_normal(buf, cpos);
        }
    }

    // ── Register ────────────────────────────────────────────────────────

    fn yank(&mut self, text: &str, linewise: bool) {
        self.register = text.to_string();
        self.register_linewise = linewise;
    }

    // ── Count helpers ───────────────────────────────────────────────────

    fn take_count(&mut self) -> usize {
        let n = self.count1.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        n
    }

    fn effective_count(&mut self) -> usize {
        let c1 = self.count1.unwrap_or(1);
        let c2 = self.count2.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        c1 * c2
    }

    fn reset_counts(&mut self) {
        self.count1 = None;
        self.count2 = None;
    }

    fn reset_pending(&mut self) {
        self.sub = SubState::Ready;
        self.reset_counts();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(unused_assignments)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn key_ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn setup(text: &str) -> (Vim, String, usize, Vec<AttachmentId>) {
        let mut vim = Vim::new();
        vim.mode = ViMode::Normal;
        vim.sub = SubState::Ready;
        let buf = text.to_string();
        let cpos = 0;
        let attachments = Vec::new();
        (vim, buf, cpos, attachments)
    }

    #[test]
    fn test_word_forward() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world foo");
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 12);
    }

    #[test]
    fn test_word_backward() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        cpos = 6;
        vim.handle_key(key('b'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_word_end() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 4);
    }

    #[test]
    fn test_delete_word() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "world");
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_delete_inner_word() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
    }

    #[test]
    fn test_change_word() {
        // cw on a word char behaves like ce (vim special case).
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('c'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_dd_single_line() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_dd_multiline() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 4; // on 'bbb'
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa\nccc");
    }

    #[test]
    fn test_dd_middle_line_with_empty_neighbors() {
        // Three lines: empty, "foo", empty. Delete middle line.
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("\nfoo\n");
        cpos = 1; // on 'f'
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "\n"); // Two empty lines remain.
    }

    #[test]
    fn test_undo_redo() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "world");
        vim.handle_key(key('u'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello world");
        vim.handle_key(key_ctrl('r'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "world");
    }

    #[test]
    fn test_count_motion() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("one two three four");
        vim.handle_key(key('2'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 8); // start of "three"
    }

    #[test]
    fn test_count_delete() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("one two three four");
        vim.handle_key(key('2'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "three four");
    }

    #[test]
    fn test_find_char() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('f'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('o'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 4);
        vim.handle_key(key(';'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 7);
        vim.handle_key(key(','), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 4);
    }

    #[test]
    fn test_till_char() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('t'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('o'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 3);
    }

    #[test]
    fn test_text_object_pair() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("foo(bar)baz");
        cpos = 5; // on 'a' inside parens
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('('), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "foo()baz");
    }

    #[test]
    fn test_text_object_quote() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup(r#"say "hello" end"#);
        cpos = 6; // on 'e' inside quotes
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('"'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, r#"say "" end"#);
    }

    #[test]
    fn test_paste() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        // Delete word, then paste.
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "world");
        vim.handle_key(key('p'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "whello orld");
    }

    #[test]
    fn test_tilde() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        vim.handle_key(key('~'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "Hello");
        assert_eq!(cpos, 1); // ~ advances cursor after toggling
    }

    #[test]
    fn test_replace() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        vim.handle_key(key('r'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('X'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "Xello");
    }

    #[test]
    fn test_replace_with_enter() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        let enter = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(key('r'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(enter, &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "\nello");
    }

    #[test]
    fn test_insert_ctrl_w_passthrough() {
        // Ctrl+W in insert mode passes through to the main handler (kill ring).
        let mut vim = Vim::new(); // starts in Insert
        let mut buf = "hello world".to_string();
        let mut cpos = buf.len();
        let mut attachments = Vec::new();
        let action = vim.handle_key(key_ctrl('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(action, Action::Passthrough);
    }

    #[test]
    fn test_line_movement() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 4); // start of 'bbb'
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 8); // start of 'ccc'
        vim.handle_key(key('k'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 4);
    }

    #[test]
    fn test_open_line_and_navigate() {
        // Type 'o' to open line below, press Esc, then navigate with j/k.
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        // 'o' opens line below → buf = "hello\n", cpos = 6, insert mode.
        vim.handle_key(key('o'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello\n");
        assert_eq!(cpos, 6);
        assert_eq!(vim.mode(), ViMode::Insert);

        // Esc → normal mode, cursor stays on empty trailing line.
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Normal);
        assert_eq!(cpos, 6); // On the empty second line.

        // 'k' should go up to "hello" line.
        vim.handle_key(key('k'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 0);

        // 'j' should go back down to the empty line.
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6);
    }

    #[test]
    fn test_esc_moves_cursor_back() {
        // In vim, Esc moves cursor left one position.
        let mut vim = Vim::new(); // starts in Insert
        let mut buf = "hello".to_string();
        let mut cpos = 5; // cursor at end
        let mut attachments = Vec::new();
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 4); // Moved back one.
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_esc_at_line_start_stays() {
        // Esc at start of line should not move further left.
        let mut vim = Vim::new();
        let mut buf = "hello".to_string();
        let mut cpos = 0;
        let mut attachments = Vec::new();
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_h_l_stay_within_line() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aa\nbb");
        // Move to end of first line.
        vim.handle_key(key('$'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 1); // On second 'a'.
                             // 'l' should NOT cross to next line.
        vim.handle_key(key('l'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 1); // Still on 'a'.
                             // Move to start of second line.
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('0'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 3);
        // 'h' should NOT cross to previous line.
        vim.handle_key(key('h'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 3); // Still at start of "bb".
    }

    #[test]
    fn test_empty_buffer() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("");
        // All of these should be no-ops on empty buffer.
        vim.handle_key(key('x'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_gg() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 8;
        vim.handle_key(key('g'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('g'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_dollar_and_zero() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('$'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 10); // last char 'd'
        vim.handle_key(key('0'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_yank_paste() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('y'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('$'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('p'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello worldhello ");
    }

    // ── Visual mode tests ───────────────────────────────────────────

    #[test]
    fn test_visual_select_and_delete() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        // Enter visual mode.
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Visual);
        // Select "hello" (move to end of word).
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        // Delete selection.
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_yank() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('y'), &mut buf, &mut cpos, &mut attachments);
        // Buffer unchanged, back to normal mode.
        assert_eq!(buf, "hello world");
        assert_eq!(vim.mode(), ViMode::Normal);
        // Paste at end.
        vim.handle_key(key('$'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('p'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello worldhello");
    }

    #[test]
    fn test_visual_change() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('c'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_visual_line_delete() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 4; // on 'bbb'
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::VisualLine);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa\nccc");
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_swap_anchor() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6); // cursor at 'w'
        vim.handle_key(key('o'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 0); // cursor swapped to anchor
    }

    #[test]
    fn test_visual_esc_returns_to_normal() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Visual);
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_tilde() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('~'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "HELLO world");
    }

    #[test]
    fn test_visual_switch_modes() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Visual);
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::VisualLine);
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Visual);
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    // ── Visual mode edge cases ──────────────────────────────────────

    #[test]
    fn test_visual_delete_multiline() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        // Visual select from 'a' to 'b' on second line.
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        // Cursor is on 'b' at line 2, col 0 (pos 4).
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        // Should delete "aaa\nb" (positions 0..5).
        assert_eq!(buf, "bb\nccc");
        assert_eq!(cpos, 0);
    }

    #[test]
    fn test_visual_select_backwards() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        cpos = 10; // on 'd'
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        // Move backwards.
        vim.handle_key(key('b'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6); // on 'w'
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        // Should delete "world" (positions 6..11).
        assert_eq!(buf, "hello ");
    }

    #[test]
    fn test_visual_line_multiline() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        // Start on first line.
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        // Extend to second line.
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        // Should delete lines "aaa\nbbb\n".
        assert_eq!(buf, "ccc");
    }

    #[test]
    fn test_visual_line_last_line() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb");
        cpos = 4; // on 'bbb'
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        // Delete last line — should remove preceding newline.
        assert_eq!(buf, "aaa");
    }

    #[test]
    fn test_visual_empty_buffer() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Visual);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_single_char() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("x");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_visual_paste_replaces() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        // Yank "hello" first.
        vim.handle_key(key('y'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        // Visual select "world".
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('p'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello hello ");
    }

    #[test]
    fn test_visual_join_lines() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('J'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa bbb\nccc");
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_yank_cursor_goes_to_start() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        cpos = 6; // on 'w'
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 10); // on 'd'
        vim.handle_key(key('y'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6); // cursor goes back to start of selection
    }

    #[test]
    fn test_visual_count_motion() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("one two three four");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('2'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        // 2w from 0 = 8 (start of "three"), visual inclusive of cursor char.
        assert_eq!(buf, "hree four");
    }

    #[test]
    fn test_visual_find_motion() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('f'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        // Visual from 0 to 'w' (pos 6) inclusive.
        assert_eq!(buf, "orld");
    }

    #[test]
    fn test_visual_dollar_motion() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('$'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_visual_range_anchor_after_cursor() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("abcdef");
        cpos = 3; // on 'd'
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        // Move left so cursor < anchor.
        vim.handle_key(key('h'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('h'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 1); // on 'b'
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        // Deletes from pos 1 ('b') to pos 3 ('d') inclusive = "bcd".
        assert_eq!(buf, "aef");
    }

    #[test]
    fn test_visual_uppercase() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('U'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "HELLO world");
        assert_eq!(vim.mode(), ViMode::Normal);
    }

    #[test]
    fn test_visual_lowercase() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("HELLO world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('u'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello world");
    }

    #[test]
    fn test_visual_line_single_line_buffer() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_visual_line_first_line() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb");
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "bbb");
    }

    #[test]
    fn test_visual_undo() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
        vim.handle_key(key('u'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello world");
    }

    #[test]
    fn test_visual_line_yank_and_paste() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        // Yank first line.
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('y'), &mut buf, &mut cpos, &mut attachments);
        // Paste after last line.
        vim.handle_key(key('G'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('p'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa\nbbb\nccc\naaa");
    }

    #[test]
    fn test_visual_ctrl_c_passes_through() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        let result = vim.handle_key(key_ctrl('c'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(result, Action::Passthrough);
    }

    #[test]
    fn test_open_line_above() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        // O opens line above.
        vim.handle_key(key('O'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "\nhello");
        assert_eq!(cpos, 0);
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_open_line_above_multiline() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb");
        cpos = 4; // on 'bbb'
        vim.handle_key(key('O'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa\n\nbbb");
        assert_eq!(cpos, 4); // on the empty line
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_visual_gg() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 8; // on 'ccc'
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('g'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('g'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 0);
        // Should still be in visual mode with selection from 8 to 0.
        assert_eq!(vim.mode(), ViMode::Visual);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "cc");
    }

    #[test]
    fn test_visual_go_end() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('G'), &mut buf, &mut cpos, &mut attachments);
        // G moves to end, visual selects everything.
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_visual_line_change_middle() {
        // Vc on a middle line should clear content, keep the line, enter insert.
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 4; // on 'bbb'
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('c'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa\n\nccc");
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_visual_join_three_lines() {
        // VjJ should join all three lines, not stop early.
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('J'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa bbb ccc");
    }

    #[test]
    fn test_visual_join_with_leading_spaces() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\n  bbb\n  ccc");
        vim.handle_key(key('V'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('J'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa bbb\n  ccc");
    }

    // ── Text object tests ───────────────────────────────────────────

    #[test]
    fn test_iw_single_line() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        cpos = 2; // on 'l' in "hello"
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
    }

    #[test]
    fn test_iw_does_not_cross_newline() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello\nworld");
        cpos = 2; // on 'l' in "hello"
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "\nworld");
    }

    #[test]
    fn test_aw_includes_trailing_space() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        cpos = 2; // on 'l' in "hello"
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('a'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "world");
    }

    #[test]
    fn test_aw_does_not_cross_newline() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello\nworld");
        cpos = 2; // on 'l' in "hello"
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('a'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        // No trailing space before newline, so include leading (none) — just the word.
        assert_eq!(buf, "\nworld");
    }

    #[test]
    fn test_viw_selects_word() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        cpos = 7; // on 'o' in "world"
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello ");
    }

    #[test]
    fn test_viw_does_not_cross_newline() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello\nworld");
        cpos = 2; // on 'l' in "hello"
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "\nworld");
    }

    #[test]
    fn test_iw_on_whitespace() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello   world");
        cpos = 6; // on a space
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "helloworld");
    }

    #[test]
    fn test_iw_on_newline() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello\nworld");
        cpos = 5; // on '\n'
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "helloworld");
    }

    #[test]
    fn test_viw_middle_of_line() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa bbb ccc");
        cpos = 5; // on second 'b'
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa  ccc");
    }

    // ── Vim behavioral correctness tests ────────────────────────────

    #[test]
    fn test_cw_on_word_acts_like_ce() {
        // cw on a word char should only change to end of word, not include trailing space.
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('c'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_cw_on_whitespace_acts_normally() {
        // cw on whitespace should change to start of next word.
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello   world");
        cpos = 5; // on first space
        vim.handle_key(key('c'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "helloworld");
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_semicolon_after_t_not_stuck() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("abcxdefxghi");
        vim.handle_key(key('t'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('x'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 2); // 'c', one before first 'x' at 3
        vim.handle_key(key(';'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6); // 'f', one before second 'x' at 7
    }

    #[test]
    fn test_p_cursor_on_last_pasted_char() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("world");
        // Yank "world".
        vim.handle_key(key('y'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        // Go to end and paste.
        vim.handle_key(key('$'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('p'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "worldworld");
        // Cursor should be on last char of pasted "world" = 'd' at position 9.
        assert_eq!(cpos, 9);
    }

    // ── curswant tests ──────────────────────────────────────────────

    #[test]
    fn test_curswant_through_short_line() {
        // "abcde\nf\nghijk" — j from col 4 should land on col 0 of "f",
        // then j again should snap back to col 4 of "ghijk".
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("abcde\nf\nghijk");
        cpos = 4; // on 'e'
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6); // 'f' (col 0, line too short)
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 12); // 'k' (col 4 restored)
    }

    #[test]
    fn test_curswant_cleared_by_horizontal_motion() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("abcde\nf\nghijk");
        cpos = 4; // on 'e'
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6); // 'f'
        vim.handle_key(key('0'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 6); // still 'f' (col 0)
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 8); // 'g' (col 0, curswant was cleared by '0')
    }

    // ── linewise operator tests ─────────────────────────────────────

    #[test]
    fn test_dj_deletes_two_lines() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('j'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "ccc");
    }

    #[test]
    fn test_dk_deletes_two_lines() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 4; // on 'bbb'
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('k'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "ccc");
    }

    #[test]
    fn test_d_big_g_deletes_to_end_linewise() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 5; // middle of 'bbb'
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('G'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa");
    }

    #[test]
    fn test_dgg_deletes_to_start_linewise() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 8; // on 'ccc'
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('g'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('g'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
    }

    // ── insert undo grouping tests ──────────────────────────────────

    #[test]
    fn test_insert_undo_groups_entire_session() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("");
        // Enter insert mode and type characters.
        vim.handle_key(key('i'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Insert);
        // Simulate typing "abc" (caller inserts chars directly into buf).
        buf.push_str("abc");
        cpos = 3;
        // Exit insert mode.
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Normal);
        assert_eq!(buf, "abc");
        // Undo should restore to empty (the state before 'i').
        vim.handle_key(key('u'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "");
    }

    #[test]
    fn test_insert_after_change_single_undo() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        // cw deletes "hello" and enters insert.
        vim.handle_key(key('c'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('w'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
        assert_eq!(vim.mode(), ViMode::Insert);
        // Type replacement text.
        buf.insert_str(0, "hi");
        cpos = 2;
        // Esc.
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        vim.handle_key(esc, &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hi world");
        // Single undo should restore original.
        vim.handle_key(key('u'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "hello world");
    }

    // ── Tests for fixed bugs ────────────────────────────────────────

    #[test]
    fn test_visual_s_substitutes() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello world");
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('e'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('s'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, " world");
        assert_eq!(vim.mode(), ViMode::Insert);
    }

    #[test]
    fn test_visual_s_capital_linewise() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 4; // on 'bbb'
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('l'), &mut buf, &mut cpos, &mut attachments);
        // S forces linewise change.
        vim.handle_key(key('S'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(vim.mode(), ViMode::Insert);
        // The entire line "bbb" should be cleared (linewise change).
        assert!(buf.contains("aaa"));
        assert!(buf.contains("ccc"));
        assert!(!buf.contains("bbb"));
    }

    #[test]
    fn test_g_with_count() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        cpos = 8; // on 'ccc'
        vim.handle_key(key('2'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('G'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 4); // start of line 2 ("bbb")
    }

    #[test]
    fn test_g_without_count_goes_to_end() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('G'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 10); // last char 'c'
    }

    #[test]
    fn test_r_with_count_cursor_on_last_replaced() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("hello");
        vim.handle_key(key('3'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('r'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('x'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "xxxlo");
        assert_eq!(cpos, 2); // on the last replaced 'x'
    }

    #[test]
    fn test_capital_p_cursor_on_last_pasted_char() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("world");
        vim.register = "hello".to_string();
        vim.register_linewise = false;
        vim.handle_key(key('P'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "helloworld");
        assert_eq!(cpos, 4); // on 'o' of "hello"
    }

    #[test]
    fn test_j_with_count() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('3'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('J'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa bbb ccc");
    }

    #[test]
    fn test_j_default_joins_two_lines() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("aaa\nbbb\nccc");
        vim.handle_key(key('J'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "aaa bbb\nccc");
    }

    #[test]
    fn test_percent_forward() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("foo(bar)baz");
        cpos = 3; // on '('
        vim.handle_key(key('%'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 7); // on ')'
    }

    #[test]
    fn test_percent_backward() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("foo(bar)baz");
        cpos = 7; // on ')'
        vim.handle_key(key('%'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 3); // on '('
    }

    #[test]
    fn test_percent_from_before_bracket() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("foo(bar)baz");
        cpos = 0; // on 'f', should find first bracket on line
        vim.handle_key(key('%'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 7); // on ')'
    }

    #[test]
    fn test_d_percent() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("foo(bar)baz");
        cpos = 3; // on '('
        vim.handle_key(key('d'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('%'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(buf, "foobaz");
        assert_eq!(cpos, 3);
    }

    #[test]
    fn test_visual_semicolon_till_advances() {
        let (mut vim, mut buf, mut cpos, mut attachments) = setup("abcabc");
        // t to 'c' → lands on 'b' (pos 1)
        vim.handle_key(key('t'), &mut buf, &mut cpos, &mut attachments);
        vim.handle_key(key('c'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 1); // on 'b'
                             // Enter visual mode.
        vim.handle_key(key('v'), &mut buf, &mut cpos, &mut attachments);
        // Repeat ; should advance to next 'c' match (pos 4).
        vim.handle_key(key(';'), &mut buf, &mut cpos, &mut attachments);
        assert_eq!(cpos, 4); // on second 'b'
    }
}
