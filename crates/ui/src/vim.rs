use crate::clipboard::Clipboard;
use crate::motions::{
    advance_chars, clamp_normal, current_line_content_range, current_line_range, find_char,
    find_matching_bracket, first_non_blank, first_non_blank_at, goto_line, line_end_normal,
    move_down, move_down_col, move_left, move_right_inclusive, move_right_normal, move_up,
    move_up_col, repeat_find, retreat_chars, word_end_pos, FindKind,
};
use crate::text::{
    char_class, line_end, line_start, next_char_boundary, prev_char_boundary, word_backward_pos,
    word_forward_pos, CharClass,
};
use crate::text_objects::text_object;
use crate::undo::{UndoEntry, UndoHistory};
use crate::AttachmentId;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VimMode {
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

/// Shared mutable state that vim needs to operate on.
///
/// `mode` is the **single global** App-owned `VimMode`; vim reads and
/// writes it through this reference rather than owning a private copy.
/// `clipboard` is the App-owned `Clipboard` subsystem (kill ring +
/// platform sink): yanks mirror into the sink, pastes pull from it
/// when it was updated externally (see `KillRing::last_clipboard_write`).
/// `curswant` is the per-Window preferred vertical-motion column (in
/// terminal cells, so wide glyphs don't throw column off); vim's
/// `j`/`k` motions read and write it. `vim_state` carries the
/// per-Window persistent vim state (Visual anchor, last `f`/`t`
/// target) plus in-flight key-sequence state.
pub struct VimContext<'a> {
    pub buf: &'a mut String,
    pub cpos: &'a mut usize,
    pub attachments: &'a mut Vec<AttachmentId>,
    pub history: &'a mut UndoHistory,
    pub clipboard: &'a mut Clipboard,
    pub mode: &'a mut VimMode,
    pub curswant: &'a mut Option<usize>,
    pub vim_state: &'a mut VimWindowState,
}

impl VimContext<'_> {
    /// Snapshot buffer state into undo history before mutating.
    fn save_undo(&mut self) {
        self.history
            .save(UndoEntry::snapshot(self.buf, *self.cpos, self.attachments));
    }

    /// Undo: pop the most recent snapshot, stashing the current state on redo.
    fn undo(&mut self) {
        let current = UndoEntry::snapshot(self.buf, *self.cpos, self.attachments);
        if let Some(entry) = self.history.undo(current) {
            *self.buf = entry.buf;
            *self.cpos = entry.cpos;
            *self.attachments = entry.attachments;
            clamp_normal(self.buf, self.cpos);
        }
    }

    /// Redo: pop the most recent redo, stashing the current state on undo.
    fn redo(&mut self) {
        let current = UndoEntry::snapshot(self.buf, *self.cpos, self.attachments);
        if let Some(entry) = self.history.redo(current) {
            *self.buf = entry.buf;
            *self.cpos = entry.cpos;
            *self.attachments = entry.attachments;
            clamp_normal(self.buf, self.cpos);
        }
    }

    /// Copy `buf[start..end]` into the kill ring with the given linewise flag.
    /// Also mirrors the text to the system clipboard so a `y` in smelt is
    /// immediately pasteable in other apps, matching nvim's
    /// `clipboard=unnamedplus`.
    fn yank_range(&mut self, start: usize, end: usize, linewise: bool) {
        let text = self.buf[start..end].to_string();
        self.clipboard
            .kill_ring
            .set_with_source(text.clone(), linewise, start, end);
        if self.clipboard.write(&text).is_ok() {
            self.clipboard.kill_ring.record_clipboard_write(text);
        }
    }

    fn register(&self) -> &str {
        self.clipboard.kill_ring.current()
    }

    fn register_linewise(&self) -> bool {
        self.clipboard.kill_ring.is_linewise()
    }

    /// Before reading the register for a paste, reconcile with the
    /// system clipboard. If the clipboard holds text different from
    /// what we last pushed, an external source updated it — overwrite
    /// the kill ring with that text (charwise, since external sources
    /// don't carry vim's linewise flag). When they match, the kill
    /// ring stays authoritative so `p` vs `P` + linewise still work.
    fn sync_paste_from_clipboard(&mut self) {
        let current = self.clipboard.read();
        let Some(text) = current else { return };
        let prev = self
            .clipboard
            .kill_ring
            .last_clipboard_write()
            .map(str::to_owned);
        if prev.as_deref() == Some(text.as_str()) {
            return;
        }
        self.clipboard
            .kill_ring
            .set_with_linewise(text.clone(), false);
        self.clipboard.kill_ring.record_clipboard_write(text);
    }
}

// ── Internal types ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Op {
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

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum SubState {
    #[default]
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

/// Per-Window vim state. Holds both the persistent slots that outlive any
/// single key sequence (`visual_anchor`, `last_find`) and the in-flight
/// key-sequence accumulators (`sub`, `count1`, `count2`) that are reset
/// between commands. The split was historical — both live here now since
/// both are per-Window and neither needs to survive Window destruction.
#[derive(Clone, Copy, Debug, Default)]
pub struct VimWindowState {
    /// Byte position of the Visual-mode anchor (where the most recent
    /// `v`/`V` was pressed). Only meaningful while
    /// `mode ∈ {Visual, VisualLine}`; carries a stale value otherwise.
    pub visual_anchor: usize,
    /// Last `f`/`t`/`F`/`T` target, replayed by `;` (same direction) or
    /// `,` (reversed).
    pub last_find: Option<(FindKind, char)>,
    /// In-flight sub-state for multi-key sequences (operator pending, find
    /// pending, text-object pending). Reset to `Ready` at command boundaries.
    pub(crate) sub: SubState,
    /// Count accumulated before the operator (or before a standalone motion).
    pub(crate) count1: Option<usize>,
    /// Count accumulated after the operator, before the motion.
    pub(crate) count2: Option<usize>,
}

impl VimWindowState {
    /// Pop count1 (defaulting to 1), clearing both count accumulators.
    pub(crate) fn take_count(&mut self) -> usize {
        let n = self.count1.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        n
    }

    /// Pop count1 * count2 (each defaulting to 1) and clear both.
    pub(crate) fn effective_count(&mut self) -> usize {
        let c1 = self.count1.unwrap_or(1);
        let c2 = self.count2.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        c1 * c2
    }

    /// Clear count accumulators only — leaves `sub` untouched.
    pub(crate) fn reset_counts(&mut self) {
        self.count1 = None;
        self.count2 = None;
    }

    /// Reset the entire pending sequence: `sub = Ready`, both counts cleared.
    pub(crate) fn reset_pending(&mut self) {
        self.sub = SubState::Ready;
        self.reset_counts();
    }

    /// Write a new mode through `mode_ref` and clear the pending sequence.
    /// Use when the caller has the mode handy outside a `VimContext`.
    pub fn set_mode(&mut self, mode_ref: &mut VimMode, mode: VimMode) {
        *mode_ref = mode;
        self.reset_pending();
    }

    /// Anchor a visual selection at `cpos` and enter the requested visual
    /// mode (`Visual` or `VisualLine`). Used by mouse drag-select so the
    /// selection originates at the click rather than the previous cursor
    /// position.
    pub fn begin_visual(&mut self, mode_ref: &mut VimMode, mode: VimMode, cpos: usize) {
        *mode_ref = mode;
        self.reset_pending();
        self.visual_anchor = cpos;
    }
}

/// Returns the visual selection range (start, end) as byte offsets when
/// `mode` is Visual or VisualLine. Range is always ordered (start <= end).
pub fn visual_range(
    state: &VimWindowState,
    buf: &str,
    cpos: usize,
    mode: VimMode,
) -> Option<(usize, usize)> {
    match mode {
        VimMode::Visual => {
            let anchor = state.visual_anchor.min(buf.len());
            let cursor = cpos.min(buf.len());
            let (a, b) = if anchor <= cursor {
                (anchor, next_char_boundary(buf, cursor).min(buf.len()))
            } else {
                (cursor, next_char_boundary(buf, anchor).min(buf.len()))
            };
            Some((a, b))
        }
        VimMode::VisualLine => {
            let anchor = state.visual_anchor.min(buf.len());
            let cursor = cpos.min(buf.len());
            let start = line_start(buf, anchor).min(line_start(buf, cursor));
            let end = line_end(buf, anchor).max(line_end(buf, cursor));
            Some((start, end))
        }
        _ => None,
    }
}

/// Read the Visual-mode anchor byte. Returns `Some(byte)` only
/// while in `Visual`/`VisualLine`; `None` in Normal/Insert. Used by
/// the prompt mouse adapter to translate between source-byte and
/// wrapped-byte spaces across `Window::handle_mouse` calls.
pub fn visual_anchor(state: &VimWindowState, mode: VimMode) -> Option<usize> {
    match mode {
        VimMode::Visual | VimMode::VisualLine => Some(state.visual_anchor),
        _ => None,
    }
}

/// Process a key event. Reads and mutates `ctx` (buffer, cursor,
/// attachments, kill ring, undo history, mode) as needed.
pub fn handle_key(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    match *ctx.mode {
        VimMode::Insert => handle_insert(key, ctx),
        VimMode::Normal => handle_normal(key, ctx),
        VimMode::Visual | VimMode::VisualLine => handle_visual(key, ctx),
    }
}

// ── Insert mode ─────────────────────────────────────────────────────

fn handle_insert(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
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
            enter_normal(ctx);
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

fn handle_normal(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    // Ctrl+key handling in normal mode.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('r') => {
                ctx.redo();
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

    // Shift+arrow / Shift+Home/End pass through so the keymap's
    // shared shift-selection actions (`SelectLeft`, …) run —
    // selection extension is the same operation whether vim is on
    // or off, so it lives in one place.
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

    // Handle sub-states first.
    match ctx.vim_state.sub {
        SubState::WaitingR => return handle_waiting_r(key, ctx),
        SubState::WaitingZ => {
            ctx.vim_state.sub = SubState::Ready;
            return if matches!(key.code, KeyCode::Char('z')) {
                Action::CenterScroll
            } else {
                Action::Consumed
            };
        }
        SubState::WaitingFind(kind) => return handle_waiting_find(key, kind, ctx),
        SubState::WaitingOpFind(op, kind) => return handle_waiting_op_find(key, op, kind, ctx),
        SubState::WaitingG => return handle_waiting_g(key, ctx),
        SubState::WaitingOpG(op) => return handle_waiting_op_g(key, op, ctx),
        SubState::WaitingTextObj(op, inner) => return handle_waiting_textobj(key, op, inner, ctx),
        SubState::WaitingOp(op) => {
            // Could be digit, motion, text object prefix (i/a), or same-key (dd/cc/yy).
            if let KeyCode::Char(c) = key.code {
                // Digit accumulation for count2.
                if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count2.is_some()) {
                    ctx.vim_state.count2 = Some(
                        ctx.vim_state.count2.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize,
                    );
                    return Action::Consumed;
                }
                // Same operator key → linewise (dd, cc, yy).
                if c == op.char() {
                    return execute_linewise_op(op, ctx);
                }
                // Text object prefix.
                if c == 'i' || c == 'a' {
                    ctx.vim_state.sub = SubState::WaitingTextObj(op, c == 'i');
                    return Action::Consumed;
                }
            }
            // Otherwise try as a motion.
            let result = execute_op_motion(key, op, ctx);
            // Don't reset if execute_op_motion transitioned to a new substate
            // (e.g. WaitingOpFind for df/dt combos).
            if matches!(ctx.vim_state.sub, SubState::WaitingOp(_)) {
                ctx.vim_state.reset_pending();
            }
            return result;
        }
        SubState::WaitingVisualTextObj(_) | SubState::Ready => {}
    }

    // Ready state — handle count digits, commands, motions.
    if let KeyCode::Char(c) = key.code {
        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
            return handle_normal_char(c, ctx);
        }
    }

    // Non-char keys in normal mode.
    match key.code {
        KeyCode::Esc => {
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        KeyCode::Enter => Action::Submit,
        KeyCode::Left => {
            *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Right => {
            *ctx.cpos = move_right_normal(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Up => Action::HistoryPrev,
        KeyCode::Down => Action::HistoryNext,
        KeyCode::Home => {
            *ctx.cpos = line_start(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::End => {
            *ctx.cpos = line_end_normal(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Backspace => {
            *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        _ => Action::Consumed,
    }
}

fn handle_normal_char(c: char, ctx: &mut VimContext<'_>) -> Action {
    // Clear desired column for any non-vertical motion.
    if c != 'j' && c != 'k' && !c.is_ascii_digit() {
        *ctx.curswant = None;
    }

    // Count digit accumulation.
    if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count1.is_some()) {
        ctx.vim_state.count1 =
            Some(ctx.vim_state.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
        return Action::Consumed;
    }

    match c {
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

        // ── Operator shortcuts ──────────────────────────────────────
        'D' => {
            ctx.save_undo();
            let end = line_end(ctx.buf, *ctx.cpos);
            ctx.yank_range(*ctx.cpos, end, false);
            ctx.buf.drain(*ctx.cpos..end);
            clamp_normal(ctx.buf, ctx.cpos);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        'C' => {
            ctx.save_undo();
            let end = line_end(ctx.buf, *ctx.cpos);
            ctx.yank_range(*ctx.cpos, end, false);
            ctx.buf.drain(*ctx.cpos..end);
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'Y' => {
            let (start, end) = current_line_range(ctx.buf, *ctx.cpos);
            ctx.yank_range(start, end, true);
            ctx.clipboard.kill_ring.mark_yanked();
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        // ── Direct edits ────────────────────────────────────────────
        'x' => {
            let n = ctx.vim_state.take_count();
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                ctx.save_undo();
                let end = advance_chars(ctx.buf, *ctx.cpos, n);
                ctx.yank_range(*ctx.cpos, end, false);
                ctx.buf.drain(*ctx.cpos..end);
                clamp_normal(ctx.buf, ctx.cpos);
            }
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        'X' => {
            let n = ctx.vim_state.take_count();
            if *ctx.cpos > 0 {
                ctx.save_undo();
                let start = retreat_chars(ctx.buf, *ctx.cpos, n);
                ctx.yank_range(start, *ctx.cpos, false);
                ctx.buf.drain(start..*ctx.cpos);
                *ctx.cpos = start;
                clamp_normal(ctx.buf, ctx.cpos);
            }
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        's' => {
            let n = ctx.vim_state.take_count();
            ctx.save_undo();
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                let end = advance_chars(ctx.buf, *ctx.cpos, n);
                ctx.yank_range(*ctx.cpos, end, false);
                ctx.buf.drain(*ctx.cpos..end);
            }
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'S' => {
            ctx.save_undo();
            let (start, end) = current_line_content_range(ctx.buf, *ctx.cpos);
            ctx.yank_range(start, end, false);
            ctx.buf.drain(start..end);
            *ctx.cpos = start;
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'r' => {
            ctx.vim_state.sub = SubState::WaitingR;
            Action::Consumed
        }
        '~' => {
            let n = ctx.vim_state.take_count();
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                ctx.save_undo();
                for _ in 0..n {
                    if *ctx.cpos >= ctx.buf.len() {
                        break;
                    }
                    let ch = ctx.buf[*ctx.cpos..].chars().next().unwrap();
                    let end = *ctx.cpos + ch.len_utf8();
                    let toggled: String = if ch.is_uppercase() {
                        ch.to_lowercase().collect()
                    } else {
                        ch.to_uppercase().collect()
                    };
                    ctx.buf.replace_range(*ctx.cpos..end, &toggled);
                    *ctx.cpos += toggled.len();
                }
                clamp_normal(ctx.buf, ctx.cpos);
            }
            ctx.vim_state.reset_pending();
            Action::Consumed
        }

        // ── Paste ───────────────────────────────────────────────────
        'p' => {
            ctx.sync_paste_from_clipboard();
            if !ctx.register().is_empty() {
                ctx.save_undo();
                if ctx.register_linewise() {
                    let eol = line_end(ctx.buf, *ctx.cpos);
                    let text = ctx.register().to_string();
                    let insert = format!("\n{}", text);
                    ctx.buf.insert_str(eol, &insert);
                    *ctx.cpos = eol + 1;
                    // Move to first non-blank.
                    *ctx.cpos += ctx.buf[*ctx.cpos..]
                        .bytes()
                        .take_while(|b| *b == b' ' || *b == b'\t')
                        .count();
                } else {
                    let after = advance_chars(ctx.buf, *ctx.cpos, 1).min(ctx.buf.len());
                    let text = ctx.register().to_string();
                    ctx.buf.insert_str(after, &text);
                    let paste_end = after + text.len();
                    *ctx.cpos = prev_char_boundary(ctx.buf, paste_end).max(after);
                    clamp_normal(ctx.buf, ctx.cpos);
                }
            }
            Action::Consumed
        }
        'P' => {
            ctx.sync_paste_from_clipboard();
            if !ctx.register().is_empty() {
                ctx.save_undo();
                if ctx.register_linewise() {
                    let sol = line_start(ctx.buf, *ctx.cpos);
                    let text = ctx.register().to_string();
                    let insert = format!("{}\n", text);
                    ctx.buf.insert_str(sol, &insert);
                    *ctx.cpos = sol;
                    *ctx.cpos += ctx.buf[*ctx.cpos..]
                        .bytes()
                        .take_while(|b| *b == b' ' || *b == b'\t')
                        .count();
                } else {
                    let text = ctx.register().to_string();
                    ctx.buf.insert_str(*ctx.cpos, &text);
                    let plen = text.len();
                    if plen > 0 {
                        let paste_end = *ctx.cpos + plen;
                        *ctx.cpos = prev_char_boundary(ctx.buf, paste_end).max(*ctx.cpos);
                        clamp_normal(ctx.buf, ctx.cpos);
                    }
                }
            }
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
        'i' => {
            ctx.vim_state.take_count();
            ctx.save_undo();
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'I' => {
            ctx.vim_state.take_count();
            ctx.save_undo();
            *ctx.cpos = first_non_blank(ctx.buf, *ctx.cpos);
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'a' => {
            ctx.vim_state.take_count();
            ctx.save_undo();
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                *ctx.cpos = advance_chars(ctx.buf, *ctx.cpos, 1);
            }
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'A' => {
            ctx.vim_state.take_count();
            ctx.save_undo();
            *ctx.cpos = line_end(ctx.buf, *ctx.cpos);
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'o' => {
            ctx.save_undo();
            let eol = line_end(ctx.buf, *ctx.cpos);
            ctx.buf.insert(eol, '\n');
            *ctx.cpos = eol + 1;
            enter_insert_mode(ctx);
            Action::Consumed
        }
        'O' => {
            ctx.save_undo();
            let sol = line_start(ctx.buf, *ctx.cpos);
            ctx.buf.insert(sol, '\n');
            *ctx.cpos = sol;
            enter_insert_mode(ctx);
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
        ';' => {
            if let Some((kind, ch)) = ctx.vim_state.last_find {
                let n = ctx.vim_state.take_count();
                *ctx.cpos = repeat_find(ctx.buf, *ctx.cpos, kind, ch, n);
            }
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        ',' => {
            if let Some((kind, ch)) = ctx.vim_state.last_find {
                let n = ctx.vim_state.take_count();
                *ctx.cpos = repeat_find(ctx.buf, *ctx.cpos, kind.reversed(), ch, n);
            }
            ctx.vim_state.reset_pending();
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
        'h' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
            }
            Action::Consumed
        }
        'l' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = move_right_normal(ctx.buf, *ctx.cpos);
            }
            Action::Consumed
        }
        'j' => {
            let n = ctx.vim_state.take_count();
            if ctx.buf.contains('\n') {
                let (new_pos, col) = move_down_col(ctx.buf, *ctx.cpos, *ctx.curswant);
                if new_pos == *ctx.cpos && n <= 1 {
                    ctx.vim_state.reset_pending();
                    return Action::HistoryNext;
                }
                *ctx.curswant = Some(col);
                *ctx.cpos = new_pos;
                for _ in 1..n {
                    (*ctx.cpos, _) = move_down_col(ctx.buf, *ctx.cpos, *ctx.curswant);
                }
                clamp_normal(ctx.buf, ctx.cpos);
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
                let (new_pos, col) = move_up_col(ctx.buf, *ctx.cpos, *ctx.curswant);
                if new_pos == *ctx.cpos && n <= 1 {
                    ctx.vim_state.reset_pending();
                    return Action::HistoryPrev;
                }
                *ctx.curswant = Some(col);
                *ctx.cpos = new_pos;
                for _ in 1..n {
                    (*ctx.cpos, _) = move_up_col(ctx.buf, *ctx.cpos, *ctx.curswant);
                }
                clamp_normal(ctx.buf, ctx.cpos);
                return Action::Consumed;
            }
            ctx.vim_state.reset_pending();
            if n <= 1 {
                Action::HistoryPrev
            } else {
                Action::Consumed
            }
        }
        'w' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_forward_pos(ctx.buf, *ctx.cpos, CharClass::Word);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        'W' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_forward_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        'b' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_backward_pos(ctx.buf, *ctx.cpos, CharClass::Word);
            }
            Action::Consumed
        }
        'B' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_backward_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
            }
            Action::Consumed
        }
        'e' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_end_pos(ctx.buf, *ctx.cpos, CharClass::Word);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        'E' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_end_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        '0' => {
            *ctx.cpos = line_start(ctx.buf, *ctx.cpos);
            *ctx.curswant = None;
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        '^' | '_' => {
            *ctx.cpos = first_non_blank(ctx.buf, *ctx.cpos);
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
        '$' => {
            let n = ctx.vim_state.take_count();
            // n$ moves down n-1 lines then to end.
            for _ in 1..n {
                *ctx.cpos = move_down(ctx.buf, *ctx.cpos);
            }
            *ctx.cpos = line_end_normal(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        'G' => {
            let had_count = ctx.vim_state.count1.is_some();
            let n = ctx.vim_state.take_count();
            *ctx.cpos = if had_count {
                goto_line(ctx.buf, n.saturating_sub(1))
            } else {
                ctx.buf.len()
            };
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }

        // ── Match bracket ────────────────────────────────────────────
        '%' => {
            ctx.vim_state.reset_counts();
            if let Some(p) = find_matching_bracket(ctx.buf, *ctx.cpos) {
                *ctx.cpos = p;
            }
            Action::Consumed
        }

        'J' => {
            let count = ctx.vim_state.take_count().max(2);
            let eol = line_end(ctx.buf, *ctx.cpos);
            if eol < ctx.buf.len() {
                ctx.save_undo();
                let mut join_pos = *ctx.cpos;
                for _ in 1..count {
                    let after = &ctx.buf[join_pos..];
                    if let Some(nl) = after.find('\n') {
                        let abs = join_pos + nl;
                        let mut end = abs + 1;
                        while end < ctx.buf.len() && ctx.buf.as_bytes()[end] == b' ' {
                            end += 1;
                        }
                        ctx.buf.replace_range(abs..end, " ");
                        join_pos = abs;
                    } else {
                        break;
                    }
                }
                *ctx.cpos = join_pos;
            }
            Action::Consumed
        }

        // Unknown — swallow it.
        _ => {
            ctx.vim_state.reset_pending();
            Action::Consumed
        }
    }
}

// ── Visual mode ──────────────────────────────────────────────────────

fn handle_visual(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    // Handle sub-states.
    if let SubState::WaitingVisualTextObj(inner) = ctx.vim_state.sub {
        ctx.vim_state.sub = SubState::Ready;
        if let KeyCode::Char(c) = key.code {
            if let Some((start, end)) = text_object(ctx.buf, *ctx.cpos, inner, c) {
                ctx.vim_state.visual_anchor = start;
                *ctx.cpos = end.saturating_sub(1);
            }
        }
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
        return if matches!(key.code, KeyCode::Char('z')) {
            Action::CenterScroll
        } else {
            Action::Consumed
        };
    }

    // Pass through Ctrl keys.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Passthrough;
    }

    // Count digit accumulation.
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

    // Non-char keys.
    match key.code {
        KeyCode::Esc => {
            exit_visual(ctx);
            Action::Consumed
        }
        KeyCode::Left => {
            *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Right => {
            *ctx.cpos = move_right_normal(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Up => {
            *ctx.cpos = move_up(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Down => {
            *ctx.cpos = move_down(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::Home => {
            *ctx.cpos = line_start(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        KeyCode::End => {
            *ctx.cpos = line_end_normal(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        _ => Action::Consumed,
    }
}

fn handle_visual_char(c: char, ctx: &mut VimContext<'_>) -> Action {
    if c != 'j' && c != 'k' && !c.is_ascii_digit() {
        *ctx.curswant = None;
    }
    match c {
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

        // ── Substitute (s → change, S → linewise change) ────────
        's' => {
            // Visual s is the same as c.
            handle_visual_char('c', ctx)
        }
        'S' => {
            // Visual S forces linewise, then changes.
            *ctx.mode = VimMode::VisualLine;
            handle_visual_char('c', ctx)
        }

        // ── Operators on selection ──────────────────────────────────
        'd' | 'x' => {
            if let Some((start, end)) = visual_range(ctx.vim_state, ctx.buf, *ctx.cpos, *ctx.mode) {
                let linewise = *ctx.mode == VimMode::VisualLine;
                ctx.save_undo();
                ctx.yank_range(start, end, linewise);
                if linewise {
                    // Include trailing newline if present.
                    let drain_end = if end < ctx.buf.len() && ctx.buf.as_bytes()[end] == b'\n' {
                        end + 1
                    } else if start > 0 && ctx.buf.as_bytes()[start - 1] == b'\n' {
                        // Last line(s) — remove preceding newline.
                        let s = start - 1;
                        ctx.buf.drain(s..end);
                        *ctx.cpos = s.min(ctx.buf.len());
                        clamp_normal(ctx.buf, ctx.cpos);
                        if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                            *ctx.cpos = first_non_blank_at(ctx.buf, line_start(ctx.buf, *ctx.cpos));
                        }
                        exit_visual(ctx);
                        return Action::Consumed;
                    } else {
                        end
                    };
                    ctx.buf.drain(start..drain_end);
                } else {
                    ctx.buf.drain(start..end);
                }
                *ctx.cpos = start.min(ctx.buf.len());
                clamp_normal(ctx.buf, ctx.cpos);
            }
            exit_visual(ctx);
            Action::Consumed
        }
        'c' => {
            if let Some((start, end)) = visual_range(ctx.vim_state, ctx.buf, *ctx.cpos, *ctx.mode) {
                let linewise = *ctx.mode == VimMode::VisualLine;
                ctx.save_undo();
                ctx.yank_range(start, end, linewise);
                if linewise {
                    // Like cc: clear line content but keep the line structure.
                    // Find the content range (excluding leading/trailing newlines).
                    let content_start = first_non_blank_at(ctx.buf, start);
                    ctx.buf.drain(content_start..end);
                    *ctx.cpos = content_start;
                } else {
                    ctx.buf.drain(start..end);
                    *ctx.cpos = start;
                }
                *ctx.mode = VimMode::Insert;
                ctx.vim_state.sub = SubState::Ready;
                ctx.vim_state.reset_counts();
                return Action::Consumed;
            }
            exit_visual(ctx);
            Action::Consumed
        }
        'y' => {
            if let Some((start, end)) = visual_range(ctx.vim_state, ctx.buf, *ctx.cpos, *ctx.mode) {
                let linewise = *ctx.mode == VimMode::VisualLine;
                ctx.yank_range(start, end, linewise);
                ctx.clipboard.kill_ring.mark_yanked();
                *ctx.cpos = start;
            }
            exit_visual(ctx);
            Action::Consumed
        }

        // ── Case toggling on selection ─────────────────────────────
        '~' => {
            if let Some((start, end)) = visual_range(ctx.vim_state, ctx.buf, *ctx.cpos, *ctx.mode) {
                ctx.save_undo();
                let toggled: String = ctx.buf[start..end]
                    .chars()
                    .map(|ch| {
                        if ch.is_uppercase() {
                            ch.to_lowercase().next().unwrap_or(ch)
                        } else {
                            ch.to_uppercase().next().unwrap_or(ch)
                        }
                    })
                    .collect();
                ctx.buf.replace_range(start..end, &toggled);
            }
            exit_visual(ctx);
            Action::Consumed
        }
        'U' => {
            if let Some((start, end)) = visual_range(ctx.vim_state, ctx.buf, *ctx.cpos, *ctx.mode) {
                ctx.save_undo();
                let upper = ctx.buf[start..end].to_uppercase();
                ctx.buf.replace_range(start..end, &upper);
            }
            exit_visual(ctx);
            Action::Consumed
        }
        'u' => {
            if let Some((start, end)) = visual_range(ctx.vim_state, ctx.buf, *ctx.cpos, *ctx.mode) {
                ctx.save_undo();
                let lower = ctx.buf[start..end].to_lowercase();
                ctx.buf.replace_range(start..end, &lower);
            }
            exit_visual(ctx);
            Action::Consumed
        }

        // ── Join lines ─────────────────────────────────────────────
        'J' => {
            if let Some((start, end)) = visual_range(ctx.vim_state, ctx.buf, *ctx.cpos, *ctx.mode) {
                ctx.save_undo();
                let mut pos = start;
                let mut remaining = end;
                while pos < remaining.min(ctx.buf.len()) {
                    if let Some(nl) = ctx.buf[pos..remaining.min(ctx.buf.len())].find('\n') {
                        let abs = pos + nl;
                        let mut ws_end = abs + 1;
                        while ws_end < ctx.buf.len() && ctx.buf.as_bytes()[ws_end] == b' ' {
                            ws_end += 1;
                        }
                        let removed = ws_end - abs;
                        ctx.buf.replace_range(abs..ws_end, " ");
                        remaining -= removed - 1; // replaced N chars with 1
                        pos = abs + 1;
                    } else {
                        break;
                    }
                }
                *ctx.cpos = start;
            }
            exit_visual(ctx);
            Action::Consumed
        }

        // ── Paste over selection ───────────────────────────────────
        'p' | 'P' => {
            ctx.sync_paste_from_clipboard();
            if !ctx.register().is_empty() {
                if let Some((start, end)) =
                    visual_range(ctx.vim_state, ctx.buf, *ctx.cpos, *ctx.mode)
                {
                    ctx.save_undo();
                    let old = ctx.buf[start..end].to_string();
                    let text = ctx.register().to_string();
                    ctx.buf.replace_range(start..end, &text);
                    *ctx.cpos = start;
                    clamp_normal(ctx.buf, ctx.cpos);
                    // The replaced text goes into register (like vim).
                    // Also mirror to clipboard so subsequent external
                    // pastes pick it up.
                    ctx.clipboard
                        .kill_ring
                        .set_with_linewise(old.clone(), false);
                    if ctx.clipboard.write(&old).is_ok() {
                        ctx.clipboard.kill_ring.record_clipboard_write(old);
                    }
                }
            }
            exit_visual(ctx);
            Action::Consumed
        }

        // ── Motions (move cursor, anchor stays) ────────────────────
        'h' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = move_left(ctx.buf, *ctx.cpos);
            }
            Action::Consumed
        }
        'l' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = move_right_normal(ctx.buf, *ctx.cpos);
            }
            Action::Consumed
        }
        'j' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                let col;
                (*ctx.cpos, col) = move_down_col(ctx.buf, *ctx.cpos, *ctx.curswant);
                *ctx.curswant = Some(col);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        'k' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                let col;
                (*ctx.cpos, col) = move_up_col(ctx.buf, *ctx.cpos, *ctx.curswant);
                *ctx.curswant = Some(col);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        'w' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_forward_pos(ctx.buf, *ctx.cpos, CharClass::Word);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        'W' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_forward_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        'b' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_backward_pos(ctx.buf, *ctx.cpos, CharClass::Word);
            }
            Action::Consumed
        }
        'B' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_backward_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
            }
            Action::Consumed
        }
        'e' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_end_pos(ctx.buf, *ctx.cpos, CharClass::Word);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        'E' => {
            let n = ctx.vim_state.take_count();
            for _ in 0..n {
                *ctx.cpos = word_end_pos(ctx.buf, *ctx.cpos, CharClass::WORD);
            }
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        '0' => {
            *ctx.cpos = line_start(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        '^' | '_' => {
            *ctx.cpos = first_non_blank(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        '$' => {
            *ctx.cpos = line_end_normal(ctx.buf, *ctx.cpos);
            Action::Consumed
        }
        'G' => {
            let had_count = ctx.vim_state.count1.is_some();
            let n = ctx.vim_state.take_count();
            *ctx.cpos = if had_count {
                goto_line(ctx.buf, n.saturating_sub(1))
            } else {
                ctx.buf.len()
            };
            clamp_normal(ctx.buf, ctx.cpos);
            Action::Consumed
        }
        '%' => {
            ctx.vim_state.reset_counts();
            if let Some(p) = find_matching_bracket(ctx.buf, *ctx.cpos) {
                *ctx.cpos = p;
            }
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
        ';' => {
            if let Some((kind, ch)) = ctx.vim_state.last_find {
                let n = ctx.vim_state.take_count();
                *ctx.cpos = repeat_find(ctx.buf, *ctx.cpos, kind, ch, n);
            }
            Action::Consumed
        }
        ',' => {
            if let Some((kind, ch)) = ctx.vim_state.last_find {
                let n = ctx.vim_state.take_count();
                *ctx.cpos = repeat_find(ctx.buf, *ctx.cpos, kind.reversed(), ch, n);
            }
            Action::Consumed
        }

        // ── Count digits ───────────────────────────────────────────
        c if c.is_ascii_digit() && (c != '0' || ctx.vim_state.count1.is_some()) => {
            ctx.vim_state.count1 =
                Some(ctx.vim_state.count1.unwrap_or(0) * 10 + c.to_digit(10).unwrap() as usize);
            Action::Consumed
        }

        // ── Swap anchor and cursor ─────────────────────────────────
        'o' => {
            std::mem::swap(&mut ctx.vim_state.visual_anchor, ctx.cpos);
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

        // Unknown — swallow.
        _ => Action::Consumed,
    }
}

// ── Sub-state handlers ──────────────────────────────────────────────

fn handle_waiting_r(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    let replacement_char = match key.code {
        KeyCode::Char(c) => Some(c),
        KeyCode::Enter => Some('\n'),
        _ => None,
    };
    if let Some(c) = replacement_char {
        if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
            let n = ctx.vim_state.take_count();
            ctx.save_undo();
            let mut pos = *ctx.cpos;
            for _ in 0..n {
                if pos >= ctx.buf.len() {
                    break;
                }
                let old = ctx.buf[pos..].chars().next().unwrap();
                let end = pos + old.len_utf8();
                let replacement = c.to_string();
                ctx.buf.replace_range(pos..end, &replacement);
                pos += replacement.len();
            }
            *ctx.cpos = prev_char_boundary(ctx.buf, pos).max(*ctx.cpos);
            clamp_normal(ctx.buf, ctx.cpos);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_find(key: KeyEvent, kind: FindKind, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(ch) = key.code {
        let n = ctx.vim_state.take_count();
        ctx.vim_state.last_find = Some((kind, ch));
        let mut pos = *ctx.cpos;
        for _ in 0..n {
            if let Some(p) = find_char(ctx.buf, pos, kind, ch) {
                pos = p;
            }
        }
        *ctx.cpos = pos;
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_op_find(
    key: KeyEvent,
    op: Op,
    kind: FindKind,
    ctx: &mut VimContext<'_>,
) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(ch) = key.code {
        let n = ctx.vim_state.effective_count();
        ctx.vim_state.last_find = Some((kind, ch));
        let origin = *ctx.cpos;
        // For operators, always find the actual char position (Forward/Backward),
        // then adjust the range for till variants.
        let raw_kind = match kind {
            FindKind::ForwardTill => FindKind::Forward,
            FindKind::BackwardTill => FindKind::Backward,
            other => other,
        };
        let mut pos = origin;
        for _ in 0..n {
            if let Some(p) = find_char(ctx.buf, pos, raw_kind, ch) {
                pos = p;
            }
        }
        if pos != origin {
            // f is inclusive (include target char), t excludes target char.
            let (start, end) = match kind {
                FindKind::Forward => (*ctx.cpos, advance_chars(ctx.buf, pos, 1)),
                FindKind::ForwardTill => (*ctx.cpos, pos),
                FindKind::Backward => (pos, *ctx.cpos),
                FindKind::BackwardTill => (advance_chars(ctx.buf, pos, 1), *ctx.cpos),
            };
            if start < end {
                return apply_charwise_op(op, ctx, start, end);
            }
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_g(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    let action = match key.code {
        KeyCode::Char('g') => {
            // gg → start of buffer.
            if let Some(n) = ctx.vim_state.count1.take() {
                *ctx.cpos = goto_line(ctx.buf, n.saturating_sub(1));
            } else {
                *ctx.cpos = 0;
            }
            Action::Consumed
        }
        _ => Action::Consumed,
    };
    ctx.vim_state.count1 = None;
    ctx.vim_state.count2 = None;
    action
}

fn handle_waiting_op_g(key: KeyEvent, op: Op, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char('g') = key.code {
        let target = if let Some(n) = ctx.vim_state.count1.take() {
            goto_line(ctx.buf, n.saturating_sub(1))
        } else {
            0
        };
        let origin = *ctx.cpos;
        if target != origin {
            let (s, e) = if target < origin {
                (target, origin)
            } else {
                (origin, target)
            };
            let ls = line_start(ctx.buf, s);
            let le = line_end(ctx.buf, e);
            ctx.vim_state.reset_pending();
            return apply_linewise_op(op, ctx, ls, le);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

fn handle_waiting_textobj(key: KeyEvent, op: Op, inner: bool, ctx: &mut VimContext<'_>) -> Action {
    ctx.vim_state.sub = SubState::Ready;
    if let KeyCode::Char(c) = key.code {
        if let Some((start, end)) = text_object(ctx.buf, *ctx.cpos, inner, c) {
            let n = ctx.vim_state.effective_count();
            let _ = n;
            return apply_charwise_op(op, ctx, start, end);
        }
    }
    ctx.vim_state.reset_pending();
    Action::Consumed
}

/// Operator pending + a motion key.
fn execute_op_motion(key: KeyEvent, op: Op, ctx: &mut VimContext<'_>) -> Action {
    let n = ctx.vim_state.effective_count();
    let origin = *ctx.cpos;

    // Resolve motion target and whether the motion is linewise.
    let (target, linewise) = match key.code {
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
            let mut p = origin;
            for _ in 0..n {
                p = move_left(ctx.buf, p);
            }
            (Some(p), false)
        }
        KeyCode::Char('l') | KeyCode::Right => {
            let mut p = origin;
            for _ in 0..n {
                p = move_right_inclusive(ctx.buf, p);
            }
            (Some(p), false)
        }
        KeyCode::Char('j') => {
            let mut p = origin;
            for _ in 0..n {
                p = move_down(ctx.buf, p);
            }
            (Some(p), true)
        }
        KeyCode::Char('k') => {
            let mut p = origin;
            for _ in 0..n {
                p = move_up(ctx.buf, p);
            }
            (Some(p), true)
        }
        KeyCode::Char('w') => {
            let mut p = origin;
            // vim special case: cw behaves like ce when cursor is on a word char.
            let use_end = op == Op::Change
                && p < ctx.buf.len()
                && char_class(ctx.buf[p..].chars().next().unwrap(), CharClass::Word) != 0;
            for _ in 0..n {
                if use_end {
                    p = word_end_pos(ctx.buf, p, CharClass::Word);
                    p = advance_chars(ctx.buf, p, 1); // inclusive end
                } else {
                    p = word_forward_pos(ctx.buf, p, CharClass::Word);
                }
            }
            (Some(p), false)
        }
        KeyCode::Char('W') => {
            let mut p = origin;
            let use_end = op == Op::Change
                && p < ctx.buf.len()
                && char_class(ctx.buf[p..].chars().next().unwrap(), CharClass::WORD) != 0;
            for _ in 0..n {
                if use_end {
                    p = word_end_pos(ctx.buf, p, CharClass::WORD);
                    p = advance_chars(ctx.buf, p, 1);
                } else {
                    p = word_forward_pos(ctx.buf, p, CharClass::WORD);
                }
            }
            (Some(p), false)
        }
        KeyCode::Char('b') => {
            let mut p = origin;
            for _ in 0..n {
                p = word_backward_pos(ctx.buf, p, CharClass::Word);
            }
            (Some(p), false)
        }
        KeyCode::Char('B') => {
            let mut p = origin;
            for _ in 0..n {
                p = word_backward_pos(ctx.buf, p, CharClass::WORD);
            }
            (Some(p), false)
        }
        KeyCode::Char('e') => {
            let mut p = origin;
            for _ in 0..n {
                p = word_end_pos(ctx.buf, p, CharClass::Word);
            }
            (Some(advance_chars(ctx.buf, p, 1)), false)
        }
        KeyCode::Char('E') => {
            let mut p = origin;
            for _ in 0..n {
                p = word_end_pos(ctx.buf, p, CharClass::WORD);
            }
            (Some(advance_chars(ctx.buf, p, 1)), false)
        }
        KeyCode::Char('0') => (Some(line_start(ctx.buf, origin)), false),
        KeyCode::Char('^' | '_') => (Some(first_non_blank(ctx.buf, origin)), false),
        KeyCode::Char('$') => (Some(line_end(ctx.buf, origin)), false),
        KeyCode::Char('%') => {
            if let Some(t) = find_matching_bracket(ctx.buf, origin) {
                let lo = origin.min(t);
                let hi = advance_chars(ctx.buf, origin.max(t), 1);
                return apply_charwise_op(op, ctx, lo, hi);
            }
            (None, false)
        }
        KeyCode::Char('G') => (Some(ctx.buf.len()), true), // linewise
        KeyCode::Char('g') => {
            ctx.vim_state.sub = SubState::WaitingOpG(op);
            return Action::Consumed;
        }
        KeyCode::Char('f') => {
            ctx.vim_state.sub = SubState::WaitingOpFind(op, FindKind::Forward);
            return Action::Consumed;
        }
        KeyCode::Char('F') => {
            ctx.vim_state.sub = SubState::WaitingOpFind(op, FindKind::Backward);
            return Action::Consumed;
        }
        KeyCode::Char('t') => {
            ctx.vim_state.sub = SubState::WaitingOpFind(op, FindKind::ForwardTill);
            return Action::Consumed;
        }
        KeyCode::Char('T') => {
            ctx.vim_state.sub = SubState::WaitingOpFind(op, FindKind::BackwardTill);
            return Action::Consumed;
        }
        KeyCode::Home => (Some(line_start(ctx.buf, origin)), false),
        KeyCode::End => (Some(line_end(ctx.buf, origin)), false),
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
        let ls = line_start(ctx.buf, start);
        let le = line_end(ctx.buf, end);
        return apply_linewise_op(op, ctx, ls, le);
    }

    let (start, end) = if target < origin {
        (target, origin)
    } else {
        (origin, target)
    };

    if start == end {
        return Action::Consumed;
    }

    apply_charwise_op(op, ctx, start, end)
}

fn execute_linewise_op(op: Op, ctx: &mut VimContext<'_>) -> Action {
    let n = ctx.vim_state.effective_count();
    ctx.vim_state.reset_counts();
    ctx.vim_state.sub = SubState::Ready;

    let start = line_start(ctx.buf, *ctx.cpos);
    let mut end_pos = *ctx.cpos;
    for _ in 1..n {
        let next = line_end(ctx.buf, end_pos);
        if next < ctx.buf.len() {
            end_pos = next + 1;
        }
    }
    let end = line_end(ctx.buf, end_pos);
    apply_linewise_op(op, ctx, start, end)
}

/// Apply a charwise operator over the byte range [start..end).
fn apply_charwise_op(op: Op, ctx: &mut VimContext<'_>, start: usize, end: usize) -> Action {
    match op {
        Op::Delete => {
            ctx.save_undo();
            ctx.yank_range(start, end, false);
            ctx.buf.drain(start..end);
            *ctx.cpos = start;
            clamp_normal(ctx.buf, ctx.cpos);
        }
        Op::Change => {
            ctx.save_undo();
            ctx.yank_range(start, end, false);
            ctx.buf.drain(start..end);
            *ctx.cpos = start;
            enter_insert_mode(ctx);
            ctx.vim_state.reset_counts();
            return Action::Consumed;
        }
        Op::Yank => {
            ctx.yank_range(start, end, false);
            ctx.clipboard.kill_ring.mark_yanked();
            *ctx.cpos = start;
        }
    }
    Action::Consumed
}

/// Apply a linewise operator over the content range [start..end].
/// `start` is the first byte of the first line, `end` is the last byte
/// of the last line (before its newline). This function handles newline
/// inclusion at buffer boundaries and cursor placement.
fn apply_linewise_op(op: Op, ctx: &mut VimContext<'_>, start: usize, end: usize) -> Action {
    let mut s = start;
    let mut e = end;
    let mut has_trailing_nl = false;
    // Include trailing newline if present.
    if e < ctx.buf.len() && ctx.buf.as_bytes()[e] == b'\n' {
        e += 1;
        has_trailing_nl = true;
    } else if e < ctx.buf.len() {
        e = line_end(ctx.buf, e);
        if e < ctx.buf.len() {
            e += 1;
            has_trailing_nl = true;
        }
    }
    // At end of buffer with no trailing newline — include preceding
    // newline to avoid leaving a dangling one.
    if !has_trailing_nl && e >= ctx.buf.len() && s > 0 {
        s -= 1;
    }

    match op {
        Op::Delete => {
            ctx.save_undo();
            ctx.yank_range(s, e, true);
            ctx.buf.drain(s..e);
            *ctx.cpos = s.min(ctx.buf.len());
            if !ctx.buf.is_empty() && *ctx.cpos < ctx.buf.len() {
                *ctx.cpos = first_non_blank_at(ctx.buf, *ctx.cpos);
            }
            clamp_normal(ctx.buf, ctx.cpos);
        }
        Op::Change => {
            ctx.save_undo();
            // Clear line content but keep the line structure.
            let content_start = first_non_blank_at(ctx.buf, s);
            let content_end = line_end(ctx.buf, e.saturating_sub(1).max(s));
            ctx.yank_range(content_start, content_end, true);
            ctx.buf.drain(content_start..content_end);
            *ctx.cpos = content_start;
            enter_insert_mode(ctx);
            return Action::Consumed;
        }
        Op::Yank => {
            // `yy` / `Y`: linewise yank leaves the cursor in place,
            // matching vim's default cpoptions. Only delete / change
            // operators (and visual-mode yank) reposition.
            ctx.yank_range(s, e, true);
            ctx.clipboard.kill_ring.mark_yanked();
        }
    }
    Action::Consumed
}

// ── Mode transitions ────────────────────────────────────────────────

fn enter_insert_mode(ctx: &mut VimContext<'_>) {
    *ctx.mode = VimMode::Insert;
    ctx.vim_state.sub = SubState::Ready;
}

fn exit_visual(ctx: &mut VimContext<'_>) {
    *ctx.mode = VimMode::Normal;
    ctx.vim_state.reset_pending();
}

fn enter_normal(ctx: &mut VimContext<'_>) {
    *ctx.mode = VimMode::Normal;
    ctx.vim_state.sub = SubState::Ready;
    ctx.vim_state.reset_counts();
    // Standard vim: cursor moves left one when leaving insert mode,
    // unless at the start of a line.
    let sol = line_start(ctx.buf, *ctx.cpos);
    if *ctx.cpos > sol {
        *ctx.cpos = prev_char_boundary(ctx.buf, *ctx.cpos);
    }
    clamp_normal(ctx.buf, ctx.cpos);
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

    /// In-memory sink for testing the kill-ring ↔ system-clipboard
    /// sync without shelling out. Stored behind a `Rc<RefCell<…>>` so
    /// the test can inspect the latest write while the `Clipboard`
    /// owns the boxed sink.
    struct MemSinkInner {
        text: Option<String>,
        writes: usize,
    }
    struct MemSink(std::rc::Rc<std::cell::RefCell<MemSinkInner>>);
    impl crate::clipboard::Sink for MemSink {
        fn read(&mut self) -> Option<String> {
            self.0.borrow().text.clone()
        }
        fn write(&mut self, text: &str) -> Result<(), String> {
            let mut inner = self.0.borrow_mut();
            inner.text = Some(text.to_string());
            inner.writes += 1;
            Ok(())
        }
    }
    // `Sink` requires `Send` so `Clipboard` can carry `Box<dyn Sink + Send>`.
    // Tests run single-threaded and `Rc` keeps the inspection handle
    // local to the test thread.
    unsafe impl Send for MemSink {}

    fn mem_sink(initial: Option<&str>) -> std::rc::Rc<std::cell::RefCell<MemSinkInner>> {
        std::rc::Rc::new(std::cell::RefCell::new(MemSinkInner {
            text: initial.map(str::to_string),
            writes: 0,
        }))
    }

    /// Owns the cross-call state (clipboard + kill ring + undo history +
    /// mode + curswant + per-window vim state) that vim borrows.
    /// `mode` mirrors the App-owned single-global VimMode in production
    /// code; tests own one locally. `curswant` and `vim_state` mirror
    /// the per-Window state that production carries on `ui::Window`.
    struct TestHarness {
        buf: String,
        cpos: usize,
        attachments: Vec<AttachmentId>,
        clipboard: Clipboard,
        history: UndoHistory,
        mode: VimMode,
        curswant: Option<usize>,
        vim_state: VimWindowState,
    }

    impl TestHarness {
        fn new(text: &str) -> Self {
            Self::with_clipboard(text, Clipboard::null())
        }

        fn with_clipboard(text: &str, clipboard: Clipboard) -> Self {
            Self {
                buf: text.to_string(),
                cpos: 0,
                attachments: Vec::new(),
                clipboard,
                history: UndoHistory::new(None),
                mode: VimMode::Normal,
                curswant: None,
                vim_state: VimWindowState::default(),
            }
        }

        fn handle(&mut self, k: KeyEvent) -> Action {
            let mut ctx = VimContext {
                buf: &mut self.buf,
                cpos: &mut self.cpos,
                attachments: &mut self.attachments,
                history: &mut self.history,
                clipboard: &mut self.clipboard,
                mode: &mut self.mode,
                curswant: &mut self.curswant,
                vim_state: &mut self.vim_state,
            };
            handle_key(k, &mut ctx)
        }
    }

    #[test]
    fn test_word_forward() {
        let mut h = TestHarness::new("hello world foo");
        h.handle(key('w'));
        assert_eq!(h.cpos, 6);
        h.handle(key('w'));
        assert_eq!(h.cpos, 12);
    }

    #[test]
    fn test_word_backward() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 6;
        h.handle(key('b'));
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_word_end() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('e'));
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_delete_word() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "world");
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_delete_inner_word() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
    }

    #[test]
    fn test_change_word() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_dd_single_line() {
        let mut h = TestHarness::new("hello");
        h.handle(key('d'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_dd_multiline() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('d'));
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa\nccc");
    }

    #[test]
    fn test_dd_middle_line_with_empty_neighbors() {
        let mut h = TestHarness::new("\nfoo\n");
        h.cpos = 1;
        h.handle(key('d'));
        h.handle(key('d'));
        assert_eq!(h.buf, "\n");
    }

    #[test]
    fn test_undo_redo() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "world");
        h.handle(key('u'));
        assert_eq!(h.buf, "hello world");
        h.handle(key_ctrl('r'));
        assert_eq!(h.buf, "world");
    }

    #[test]
    fn test_count_motion() {
        let mut h = TestHarness::new("one two three four");
        h.handle(key('2'));
        h.handle(key('w'));
        assert_eq!(h.cpos, 8);
    }

    #[test]
    fn test_count_delete() {
        let mut h = TestHarness::new("one two three four");
        h.handle(key('2'));
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "three four");
    }

    #[test]
    fn test_find_char() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('f'));
        h.handle(key('o'));
        assert_eq!(h.cpos, 4);
        h.handle(key(';'));
        assert_eq!(h.cpos, 7);
        h.handle(key(','));
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_till_char() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('t'));
        h.handle(key('o'));
        assert_eq!(h.cpos, 3);
    }

    #[test]
    fn test_text_object_pair() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('('));
        assert_eq!(h.buf, "foo()baz");
    }

    #[test]
    fn test_text_object_quote() {
        let mut h = TestHarness::new("foo \"bar\" baz");
        h.cpos = 6;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('"'));
        assert_eq!(h.buf, "foo \"\" baz");
    }

    #[test]
    fn test_paste() {
        let mut h = TestHarness::new("hello");
        h.clipboard
            .kill_ring
            .set_with_linewise(" world".to_string(), false);
        h.cpos = 4;
        h.handle(key('p'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn test_tilde() {
        let mut h = TestHarness::new("hello");
        h.handle(key('~'));
        assert_eq!(h.buf, "Hello");
        assert_eq!(h.cpos, 1);
    }

    #[test]
    fn test_replace() {
        let mut h = TestHarness::new("hello");
        h.handle(key('r'));
        h.handle(key('X'));
        assert_eq!(h.buf, "Xello");
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_replace_with_enter() {
        let mut h = TestHarness::new("hello");
        h.handle(key('r'));
        h.handle(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        });
        assert_eq!(h.buf, "\nello");
    }

    #[test]
    fn test_insert_ctrl_w_passthrough() {
        let mut h = TestHarness::new("hello");
        h.handle(key('i'));
        assert_eq!(h.mode, VimMode::Insert);
        let result = h.handle(key_ctrl('w'));
        assert_eq!(result, Action::Passthrough);
    }

    #[test]
    fn test_line_movement() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('j'));
        assert_eq!(h.cpos, 4);
        h.handle(key('j'));
        assert_eq!(h.cpos, 8);
        h.handle(key('k'));
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_open_line_and_navigate() {
        // 'o' from normal mode opens line below, press Esc, then navigate with j/k.
        let mut h = TestHarness::new("hello");
        // 'o' opens line below → buf = "hello\n", cpos = 6, insert mode.
        h.handle(key('o'));
        assert_eq!(h.buf, "hello\n");
        assert_eq!(h.cpos, 6);
        assert_eq!(h.mode, VimMode::Insert);

        // Esc → normal mode, cursor stays on empty trailing line.
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.mode, VimMode::Normal);
        assert_eq!(h.cpos, 6); // On the empty second line.

        // 'k' should go up to "hello" line.
        h.handle(key('k'));
        assert_eq!(h.cpos, 0);

        // 'j' should go back down to the empty line.
        h.handle(key('j'));
        assert_eq!(h.cpos, 6);
    }

    #[test]
    fn test_esc_moves_cursor_back() {
        let mut h = TestHarness::new("hello");
        h.mode = VimMode::Insert;
        h.cpos = 5;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.cpos, 4);
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_esc_at_line_start_stays() {
        let mut h = TestHarness::new("hello");
        h.mode = VimMode::Insert;
        h.cpos = 0;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_h_l_stay_within_line() {
        let mut h = TestHarness::new("aa\nbb");
        h.handle(key('$'));
        assert_eq!(h.cpos, 1);
        h.handle(key('l'));
        assert_eq!(h.cpos, 1);
        h.handle(key('j'));
        h.handle(key('0'));
        assert_eq!(h.cpos, 3);
        h.handle(key('h'));
        assert_eq!(h.cpos, 3);
    }

    #[test]
    fn test_empty_buffer() {
        let mut h = TestHarness::new("");
        h.handle(key('x'));
        assert_eq!(h.buf, "");
        h.handle(key('d'));
        h.handle(key('w'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_gg() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('g'));
        h.handle(key('g'));
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_dollar_and_zero() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('$'));
        assert_eq!(h.cpos, 10);
        h.handle(key('0'));
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_yank_paste() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('y'));
        h.handle(key('w'));
        h.handle(key('$'));
        h.handle(key('p'));
        assert_eq!(h.buf, "hello worldhello ");
    }

    #[test]
    fn yank_mirrors_to_clipboard() {
        let inner = mem_sink(None);
        let clipboard = Clipboard::new(Box::new(MemSink(inner.clone())));
        let mut h = TestHarness::with_clipboard("hello world", clipboard);
        h.handle(key('y'));
        h.handle(key('w'));
        let s = inner.borrow();
        assert_eq!(s.text.as_deref(), Some("hello "));
        assert_eq!(s.writes, 1);
        drop(s);
        assert_eq!(h.clipboard.kill_ring.last_clipboard_write(), Some("hello "));
    }

    #[test]
    fn paste_prefers_external_clipboard_when_updated() {
        // External tool put "pasted" on the clipboard. `p` should use
        // that instead of whatever is in the kill ring.
        let inner = mem_sink(Some("pasted"));
        let clipboard = Clipboard::new(Box::new(MemSink(inner)));
        let mut h = TestHarness::with_clipboard("abc", clipboard);
        h.clipboard
            .kill_ring
            .set_with_linewise("stale".to_string(), false);
        // Move cursor to end so `p` inserts after.
        h.handle(key('$'));
        h.handle(key('p'));
        assert_eq!(h.buf, "abcpasted");
    }

    #[test]
    fn paste_keeps_kill_ring_when_clipboard_matches_last_write() {
        // Kill ring was the last writer — its linewise flag matters
        // for `p` placement, so we must not overwrite charwise.
        let inner = mem_sink(Some("line\n"));
        let clipboard = Clipboard::new(Box::new(MemSink(inner)));
        let mut h = TestHarness::with_clipboard("abc\n", clipboard);
        // Simulate a prior `yy`: linewise + clipboard mirror.
        h.clipboard
            .kill_ring
            .set_with_linewise("line\n".to_string(), true);
        h.clipboard
            .kill_ring
            .record_clipboard_write("line\n".to_string());
        // Position on first line, then `p` — linewise pastes below.
        h.handle(key('p'));
        assert!(h.buf.contains("line\n"));
        assert!(h.clipboard.kill_ring.is_linewise());
    }

    #[test]
    fn test_yy_keeps_cursor_in_place() {
        // Regression: `yy` used to snap the cursor to column 0 of the
        // yanked line. Vim's default behavior is "linewise yank does
        // not move the cursor"; both `yy` and `Y` should leave the
        // cursor exactly where it was.
        let mut h = TestHarness::new("hello world\nsecond line");
        h.handle(key('l')); // cpos=1
        h.handle(key('l')); // cpos=2
        h.handle(key('l')); // cpos=3
        let before = h.cpos;
        h.handle(key('y'));
        h.handle(key('y'));
        assert_eq!(h.cpos, before, "yy must not move cursor");
        assert_eq!(h.clipboard.kill_ring.current(), "hello world\n");
    }

    #[test]
    fn test_capital_y_keeps_cursor_in_place() {
        let mut h = TestHarness::new("hello world\nsecond line");
        h.handle(key('l'));
        h.handle(key('l'));
        let before = h.cpos;
        h.handle(key('Y'));
        assert_eq!(h.cpos, before, "Y must not move cursor");
    }

    #[test]
    fn test_visual_select_and_delete() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('e'));
        h.handle(key('d'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_yank() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('y'));
        assert_eq!(h.buf, "hello world");
        assert_eq!(h.mode, VimMode::Normal);
        h.handle(key('$'));
        h.handle(key('p'));
        assert_eq!(h.buf, "hello worldhello");
    }

    #[test]
    fn test_visual_change() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('c'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_visual_line_delete() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('V'));
        assert_eq!(h.mode, VimMode::VisualLine);
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa\nccc");
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_swap_anchor() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('w'));
        assert_eq!(h.cpos, 6);
        h.handle(key('o'));
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_visual_esc_returns_to_normal() {
        let mut h = TestHarness::new("hello");
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Visual);
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_tilde() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('~'));
        assert_eq!(h.buf, "HELLO world");
    }

    #[test]
    fn test_visual_switch_modes() {
        let mut h = TestHarness::new("hello");
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('V'));
        assert_eq!(h.mode, VimMode::VisualLine);
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_delete_multiline() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('v'));
        h.handle(key('j'));
        h.handle(key('d'));
        assert_eq!(h.buf, "bb\nccc");
        assert_eq!(h.cpos, 0);
    }

    #[test]
    fn test_visual_select_backwards() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 10;
        h.handle(key('v'));
        h.handle(key('b'));
        assert_eq!(h.cpos, 6);
        h.handle(key('d'));
        assert_eq!(h.buf, "hello ");
    }

    #[test]
    fn test_visual_line_multiline() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('V'));
        h.handle(key('j'));
        h.handle(key('d'));
        assert_eq!(h.buf, "ccc");
    }

    #[test]
    fn test_visual_line_last_line() {
        let mut h = TestHarness::new("aaa\nbbb");
        h.cpos = 4;
        h.handle(key('V'));
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa");
    }

    #[test]
    fn test_visual_empty_buffer() {
        let mut h = TestHarness::new("");
        h.handle(key('v'));
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('d'));
        assert_eq!(h.buf, "");
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_single_char() {
        let mut h = TestHarness::new("x");
        h.handle(key('v'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_visual_paste_replaces() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('y'));
        h.handle(key('w'));
        h.handle(key('w'));
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('p'));
        assert_eq!(h.buf, "hello hello ");
    }

    #[test]
    fn test_visual_join_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('V'));
        h.handle(key('j'));
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb\nccc");
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_yank_cursor_goes_to_start() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 6;
        h.handle(key('v'));
        h.handle(key('e'));
        assert_eq!(h.cpos, 10);
        h.handle(key('y'));
        assert_eq!(h.cpos, 6);
    }

    #[test]
    fn test_visual_count_motion() {
        let mut h = TestHarness::new("one two three four");
        h.handle(key('v'));
        h.handle(key('2'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "hree four");
    }

    #[test]
    fn test_visual_find_motion() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('f'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "orld");
    }

    #[test]
    fn test_visual_dollar_motion() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('$'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_visual_range_anchor_after_cursor() {
        let mut h = TestHarness::new("abcdef");
        h.cpos = 3;
        h.handle(key('v'));
        h.handle(key('h'));
        h.handle(key('h'));
        assert_eq!(h.cpos, 1);
        h.handle(key('d'));
        assert_eq!(h.buf, "aef");
    }

    #[test]
    fn test_visual_uppercase() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('U'));
        assert_eq!(h.buf, "HELLO world");
        assert_eq!(h.mode, VimMode::Normal);
    }

    #[test]
    fn test_visual_lowercase() {
        let mut h = TestHarness::new("HELLO world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('u'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn test_visual_line_single_line_buffer() {
        let mut h = TestHarness::new("hello");
        h.handle(key('V'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_visual_line_first_line() {
        let mut h = TestHarness::new("aaa\nbbb");
        h.handle(key('V'));
        h.handle(key('d'));
        assert_eq!(h.buf, "bbb");
    }

    #[test]
    fn test_visual_undo() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('d'));
        assert_eq!(h.buf, " world");
        h.handle(key('u'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn test_visual_line_yank_and_paste() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('V'));
        h.handle(key('y'));
        h.handle(key('G'));
        h.handle(key('p'));
        assert_eq!(h.buf, "aaa\nbbb\nccc\naaa");
    }

    #[test]
    fn test_visual_ctrl_c_passes_through() {
        let mut h = TestHarness::new("hello");
        h.handle(key('v'));
        let result = h.handle(key_ctrl('c'));
        assert_eq!(result, Action::Passthrough);
    }

    #[test]
    fn test_open_line_above() {
        let mut h = TestHarness::new("hello");
        h.handle(key('O'));
        assert_eq!(h.buf, "\nhello");
        assert_eq!(h.cpos, 0);
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_open_line_above_multiline() {
        let mut h = TestHarness::new("aaa\nbbb");
        h.cpos = 4;
        h.handle(key('O'));
        assert_eq!(h.buf, "aaa\n\nbbb");
        assert_eq!(h.cpos, 4);
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_visual_gg() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('v'));
        h.handle(key('g'));
        h.handle(key('g'));
        assert_eq!(h.cpos, 0);
        assert_eq!(h.mode, VimMode::Visual);
        h.handle(key('d'));
        assert_eq!(h.buf, "cc");
    }

    #[test]
    fn test_visual_go_end() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('v'));
        h.handle(key('G'));
        h.handle(key('d'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_visual_line_change_middle() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('V'));
        h.handle(key('c'));
        assert_eq!(h.buf, "aaa\n\nccc");
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_visual_join_three_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('V'));
        h.handle(key('j'));
        h.handle(key('j'));
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb ccc");
    }

    #[test]
    fn test_visual_join_with_leading_spaces() {
        let mut h = TestHarness::new("aaa\n  bbb\n  ccc");
        h.handle(key('V'));
        h.handle(key('j'));
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb\n  ccc");
    }

    #[test]
    fn test_iw_single_line() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
    }

    #[test]
    fn test_iw_does_not_cross_newline() {
        let mut h = TestHarness::new("hello\nworld");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, "\nworld");
    }

    #[test]
    fn test_aw_includes_trailing_space() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('a'));
        h.handle(key('w'));
        assert_eq!(h.buf, "world");
    }

    #[test]
    fn test_aw_does_not_cross_newline() {
        let mut h = TestHarness::new("hello\nworld");
        h.cpos = 2;
        h.handle(key('d'));
        h.handle(key('a'));
        h.handle(key('w'));
        assert_eq!(h.buf, "\nworld");
    }

    #[test]
    fn test_viw_selects_word() {
        let mut h = TestHarness::new("hello world");
        h.cpos = 7;
        h.handle(key('v'));
        h.handle(key('i'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "hello ");
    }

    #[test]
    fn test_viw_does_not_cross_newline() {
        let mut h = TestHarness::new("hello\nworld");
        h.cpos = 2;
        h.handle(key('v'));
        h.handle(key('i'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "\nworld");
    }

    #[test]
    fn test_iw_on_whitespace() {
        let mut h = TestHarness::new("hello   world");
        h.cpos = 6;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, "helloworld");
    }

    #[test]
    fn test_iw_on_newline() {
        let mut h = TestHarness::new("hello\nworld");
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('i'));
        h.handle(key('w'));
        assert_eq!(h.buf, "helloworld");
    }

    #[test]
    fn test_viw_middle_of_line() {
        let mut h = TestHarness::new("aaa bbb ccc");
        h.cpos = 5;
        h.handle(key('v'));
        h.handle(key('i'));
        h.handle(key('w'));
        h.handle(key('d'));
        assert_eq!(h.buf, "aaa  ccc");
    }

    #[test]
    fn test_cw_on_word_acts_like_ce() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_cw_on_whitespace_acts_normally() {
        let mut h = TestHarness::new("hello   world");
        h.cpos = 5;
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, "helloworld");
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_semicolon_after_t_not_stuck() {
        let mut h = TestHarness::new("abcxdefxghi");
        h.handle(key('t'));
        h.handle(key('x'));
        assert_eq!(h.cpos, 2);
        h.handle(key(';'));
        assert_eq!(h.cpos, 6);
    }

    #[test]
    fn test_p_cursor_on_last_pasted_char() {
        let mut h = TestHarness::new("world");
        h.handle(key('y'));
        h.handle(key('w'));
        h.handle(key('$'));
        h.handle(key('p'));
        assert_eq!(h.buf, "worldworld");
        assert_eq!(h.cpos, 9);
    }

    #[test]
    fn test_curswant_through_short_line() {
        let mut h = TestHarness::new("abcde\nf\nghijk");
        h.cpos = 4;
        h.handle(key('j'));
        assert_eq!(h.cpos, 6);
        h.handle(key('j'));
        assert_eq!(h.cpos, 12);
    }

    #[test]
    fn test_curswant_cleared_by_horizontal_motion() {
        let mut h = TestHarness::new("abcde\nf\nghijk");
        h.cpos = 4;
        h.handle(key('j'));
        assert_eq!(h.cpos, 6);
        h.handle(key('0'));
        assert_eq!(h.cpos, 6);
        h.handle(key('j'));
        assert_eq!(h.cpos, 8);
    }

    #[test]
    fn test_dj_deletes_two_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('d'));
        h.handle(key('j'));
        assert_eq!(h.buf, "ccc");
    }

    #[test]
    fn test_dk_deletes_two_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('d'));
        h.handle(key('k'));
        assert_eq!(h.buf, "ccc");
    }

    #[test]
    fn test_d_big_g_deletes_to_end_linewise() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 5;
        h.handle(key('d'));
        h.handle(key('G'));
        assert_eq!(h.buf, "aaa");
    }

    #[test]
    fn test_dgg_deletes_to_start_linewise() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('d'));
        h.handle(key('g'));
        h.handle(key('g'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_insert_undo_groups_entire_session() {
        let mut h = TestHarness::new("");
        h.handle(key('i'));
        assert_eq!(h.mode, VimMode::Insert);
        h.buf.push_str("abc");
        h.cpos = 3;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.mode, VimMode::Normal);
        assert_eq!(h.buf, "abc");
        h.handle(key('u'));
        assert_eq!(h.buf, "");
    }

    #[test]
    fn test_insert_after_change_single_undo() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('c'));
        h.handle(key('w'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.mode, VimMode::Insert);
        h.buf.insert_str(0, "hi");
        h.cpos = 2;
        let esc = KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        h.handle(esc);
        assert_eq!(h.buf, "hi world");
        h.handle(key('u'));
        assert_eq!(h.buf, "hello world");
    }

    #[test]
    fn test_visual_s_substitutes() {
        let mut h = TestHarness::new("hello world");
        h.handle(key('v'));
        h.handle(key('e'));
        h.handle(key('s'));
        assert_eq!(h.buf, " world");
        assert_eq!(h.mode, VimMode::Insert);
    }

    #[test]
    fn test_visual_s_capital_linewise() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 4;
        h.handle(key('v'));
        h.handle(key('l'));
        h.handle(key('S'));
        assert_eq!(h.mode, VimMode::Insert);
        assert!(h.buf.contains("aaa"));
        assert!(h.buf.contains("ccc"));
        assert!(!h.buf.contains("bbb"));
    }

    #[test]
    fn test_g_with_count() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.cpos = 8;
        h.handle(key('2'));
        h.handle(key('G'));
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_g_without_count_goes_to_end() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('G'));
        assert_eq!(h.cpos, 10);
    }

    #[test]
    fn test_r_with_count_cursor_on_last_replaced() {
        let mut h = TestHarness::new("hello");
        h.handle(key('3'));
        h.handle(key('r'));
        h.handle(key('x'));
        assert_eq!(h.buf, "xxxlo");
        assert_eq!(h.cpos, 2);
    }

    #[test]
    fn test_capital_p_cursor_on_last_pasted_char() {
        let mut h = TestHarness::new("world");
        h.clipboard
            .kill_ring
            .set_with_linewise("hello".to_string(), false);
        h.handle(key('P'));
        assert_eq!(h.buf, "helloworld");
        assert_eq!(h.cpos, 4);
    }

    #[test]
    fn test_j_with_count() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('3'));
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb ccc");
    }

    #[test]
    fn test_j_default_joins_two_lines() {
        let mut h = TestHarness::new("aaa\nbbb\nccc");
        h.handle(key('J'));
        assert_eq!(h.buf, "aaa bbb\nccc");
    }

    #[test]
    fn test_percent_forward() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 3;
        h.handle(key('%'));
        assert_eq!(h.cpos, 7);
    }

    #[test]
    fn test_percent_backward() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 7;
        h.handle(key('%'));
        assert_eq!(h.cpos, 3);
    }

    #[test]
    fn test_percent_from_before_bracket() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 0;
        h.handle(key('%'));
        assert_eq!(h.cpos, 7);
    }

    #[test]
    fn test_d_percent() {
        let mut h = TestHarness::new("foo(bar)baz");
        h.cpos = 3;
        h.handle(key('d'));
        h.handle(key('%'));
        assert_eq!(h.buf, "foobaz");
        assert_eq!(h.cpos, 3);
    }

    #[test]
    fn test_visual_semicolon_till_advances() {
        let mut h = TestHarness::new("abcabc");
        h.handle(key('t'));
        h.handle(key('c'));
        assert_eq!(h.cpos, 1);
        h.handle(key('v'));
        h.handle(key(';'));
        assert_eq!(h.cpos, 4);
    }
}
