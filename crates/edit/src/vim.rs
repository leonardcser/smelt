use super::jumplist::JumpMove;
use super::motions::{
    advance_chars, clamp_normal, current_line_content_range, first_non_blank, first_non_blank_at,
    line_end_normal, move_down, move_down_col, move_left, move_right_normal, move_up, move_up_col,
    operator_span, resolve, FindKind, Motion, Reach, Span, Use,
};
use super::text::{
    byte_to_cell, cell_to_byte, line_end, line_start, next_grapheme_boundary,
    prev_grapheme_boundary,
};
use super::text_objects::{
    select_object, surrounding_delimiters, text_object, TextObjectKind, TextObjectSpec, VisualArea,
};
use super::window::{
    BlockMotion, DocumentCommand, DocumentKeyResult, DocumentTextObject, ScrollTo,
};
use super::{Clipboard, UndoEntry, UndoHistory};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use smelt_buffer::attached::AttachedTextMut;
use smelt_buffer::attachment::ATTACHMENT_MARKER;

mod normal;
mod numbers;
mod operator;
mod pending;
mod surround;
mod viewer;
mod visual;

use normal::*;
use numbers::*;
use operator::*;
use pending::*;
use surround::*;
use visual::*;

pub use viewer::handle_viewer_key;

/// Upper-case `c` to a single char, as vim does: `ß` becomes `ẞ`, and chars
/// whose full mapping is longer than one char stay as they are.
fn upper_char(c: char) -> char {
    if c == 'ß' {
        return 'ẞ';
    }
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// Lower-case `c` to a single char, as vim does: `İ` becomes `i`, the digraph
/// title-case letters stay, and chars whose full mapping is longer than one
/// char stay as they are.
fn lower_char(c: char) -> char {
    match c {
        'İ' => return 'i',
        'ǅ' | 'ǈ' | 'ǋ' | 'ǲ' => return c,
        _ => {}
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

fn toggle_char(c: char) -> char {
    if c.is_uppercase() {
        lower_char(c)
    } else {
        upper_char(c)
    }
}

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VimMode {
    Insert,
    #[default]
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
    /// Move the cursor by `delta` display rows, following soft wraps (`gj`, `gk`).
    DisplayRows(isize),
    /// Center the input viewport on the cursor (zz).
    CenterScroll,
    /// Pan the viewport horizontally by `delta` cells (vim `zh`/`zl`).
    /// Positive = pan right, negative = pan left. Cursor stays put - same
    /// semantics as nvim's `zh`/`zl`.
    PanColumns(isize),
    /// Key not handled - caller should use its own logic.
    Passthrough,
}

/// Shared mutable state for vim operations. Borrowed from the host per keypress.
/// `buf` exposes source + attachment ids together as one wrapper - there is no
/// `&mut String` to grab, so every text mutation goes through methods that keep
/// the two halves in sync.
pub struct VimContext<'a> {
    pub buf: AttachedTextMut<'a>,
    pub cpos: &'a mut usize,
    pub history: &'a mut UndoHistory,
    pub clipboard: &'a mut Clipboard,
    pub mode: &'a mut VimMode,
    pub curswant: &'a mut Option<usize>,
    pub vim_state: &'a mut VimWindowState,
    /// Host clock at dispatch time; used to stamp yank-flash deadlines so the
    /// flash window is observable via a virtual clock in sim/fuzz runs.
    pub now: std::time::Instant,
}

impl VimContext<'_> {
    /// Snapshot to undo history before mutating.
    fn save_undo(&mut self) {
        self.history.save(UndoEntry::snapshot(
            self.buf.as_str(),
            *self.cpos,
            self.buf.ids(),
        ));
    }

    /// Install an undo/redo entry: swap source + attachments, restore cpos,
    /// and clamp every offset that survived the source swap (cpos and the
    /// vim visual anchor - either can land past end-of-source if the entry
    /// shrunk the buffer).
    fn restore(&mut self, entry: UndoEntry) {
        self.buf.install(entry.buf, entry.attachments);
        *self.cpos = entry.cpos;
        clamp_normal(self.buf.as_str(), self.cpos);
        self.vim_state.clamp_visual_anchor(self.buf.as_str());
    }

    /// Undo: pop the most recent snapshot, stash current state for redo.
    fn undo(&mut self) {
        let current = UndoEntry::snapshot(self.buf.as_str(), *self.cpos, self.buf.ids());
        if let Some(entry) = self.history.undo(current) {
            self.restore(entry);
        }
    }

    /// Redo: pop the most recent redo, stash current state for undo.
    fn redo(&mut self) {
        let current = UndoEntry::snapshot(self.buf.as_str(), *self.cpos, self.buf.ids());
        if let Some(entry) = self.history.redo(current) {
            self.restore(entry);
        }
    }

    /// Stage `buf[start..end]`, which `op` yanks or removes, in the pending
    /// register and the kill ring with its source byte range. The system
    /// clipboard is **not** written here - `Window::handle_key`'s caller
    /// observes the kill ring's `yank_tick` change and pushes the rendered
    /// (`BufferCopy::copy`) form to the system clipboard. Keeping vim out of
    /// the system clipboard means raw markers stay in the kill ring for
    /// paste-back fidelity, while the clipboard gets `[label]` etc. A named
    /// register keeps the clipboard as it was; `"_` stores nothing.
    fn yank_range(&mut self, op: Op, start: usize, end: usize, linewise: bool) {
        // The kill ring carries plain text only; an attachment id has no
        // representation here. If the yanked range covers an
        // ATTACHMENT_MARKER, dropping the marker is the only way to keep
        // a later `p`/`P` from producing orphan marker bytes in source.
        let text = self.buf.as_str()[start..end].replace(ATTACHMENT_MARKER, "");
        let ring = &mut self.clipboard.kill_ring;
        match self.vim_state.register {
            Some('_') => {}
            Some(name) if name.is_ascii_alphabetic() => {
                ring.store(name, text, linewise);
                // The unnamed register follows the last write, as in vim.
                let (text, linewise) = ring.register(name).unwrap_or_default();
                let (text, linewise) = (text.to_owned(), linewise);
                ring.set_quietly(text, linewise, start, end);
            }
            _ => {
                if op == Op::Yank {
                    ring.store('0', text.clone(), linewise);
                }
                ring.set_with_source(text, linewise, start, end);
            }
        }
    }

    /// The text `p` puts from the pending register, and whether it is
    /// linewise. The unnamed register first takes the system clipboard when
    /// something else changed it.
    fn put_text(&mut self) -> (String, bool) {
        let ring = &self.clipboard.kill_ring;
        match self.vim_state.register {
            Some('_') => (String::new(), false),
            Some('.') => (ring.last_insert().unwrap_or_default().to_owned(), false),
            Some(name) if name == '0' || name.is_ascii_alphabetic() => ring
                .register(name)
                .map(|(text, linewise)| (text.to_owned(), linewise))
                .unwrap_or_default(),
            _ => {
                self.sync_paste_from_clipboard();
                let ring = &self.clipboard.kill_ring;
                (ring.current().to_owned(), ring.is_linewise())
            }
        }
    }

    /// Sync the kill ring from the system clipboard before a paste.
    /// If the clipboard was updated externally, overwrite the kill ring (charwise).
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

    /// Delete `buf[start..end]`. Attachment ids whose markers lived in the
    /// range are dropped automatically. Endpoints are snapped.
    fn delete_range(&mut self, start: usize, end: usize) {
        self.buf.replace_range(start..end, "");
        self.vim_state.clamp_visual_anchor(self.buf.as_str());
    }

    /// Replace `buf[start..end]` with `text`. Attachment ids whose markers
    /// survive into `text` (e.g. case-mapped markers fold to themselves)
    /// keep their ids; only markers actually removed are dropped. Endpoints
    /// are snapped.
    fn replace_range(&mut self, start: usize, end: usize, text: &str) {
        self.buf.replace_range(start..end, text);
        self.vim_state.clamp_visual_anchor(self.buf.as_str());
    }

    /// Insert `text` at `at`, returning the inserted position. Mirrors
    /// `buf.insert_str` but also clamps the visual anchor: a paste shifts
    /// every offset past `at` right by `text.len()`, so a pre-paste anchor
    /// can land mid-codepoint in the post-paste source.
    fn insert_str(&mut self, at: usize, text: &str) -> usize {
        let p = self.buf.insert_str(at, text);
        self.vim_state.clamp_visual_anchor(self.buf.as_str());
        p
    }
}

// ── Internal types ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Delete,
    Change,
    Yank,
    Upper,
    Lower,
    Toggle,
    Indent,
    Dedent,
}

impl Op {
    /// The keys that start the operator.
    fn label(self) -> &'static str {
        match self {
            Op::Delete => "d",
            Op::Change => "c",
            Op::Yank => "y",
            Op::Upper => "gU",
            Op::Lower => "gu",
            Op::Toggle => "g~",
            Op::Indent => ">",
            Op::Dedent => "<",
        }
    }

    /// The key that repeats the operator to act on whole lines (`dd`, `gUU`).
    fn char(self) -> char {
        self.label().chars().last().unwrap_or_default()
    }

    /// Operators that rewrite the case of the text they span.
    fn case_op(c: char) -> Option<Op> {
        match c {
            'U' => Some(Op::Upper),
            'u' => Some(Op::Lower),
            '~' => Some(Op::Toggle),
            _ => None,
        }
    }

    /// `text` with this operator's case applied, or `None` for other operators.
    fn recase(self, text: &str) -> Option<String> {
        match self {
            Op::Upper => Some(text.chars().map(upper_char).collect()),
            Op::Lower => Some(text.chars().map(lower_char).collect()),
            Op::Toggle => Some(text.chars().map(toggle_char).collect()),
            Op::Delete | Op::Change | Op::Yank | Op::Indent | Op::Dedent => None,
        }
    }
}

/// The find motion a key starts, waiting for the character to find.
fn find_kind(c: char) -> Option<FindKind> {
    match c {
        'f' => Some(FindKind::Forward),
        'F' => Some(FindKind::Backward),
        't' => Some(FindKind::ForwardTill),
        'T' => Some(FindKind::BackwardTill),
        _ => None,
    }
}

fn find_kind_char(kind: FindKind) -> char {
    match kind {
        FindKind::Forward => 'f',
        FindKind::ForwardTill => 't',
        FindKind::Backward => 'F',
        FindKind::BackwardTill => 'T',
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum SubState {
    #[default]
    Ready,
    WaitingOp(Op),
    WaitingG,
    WaitingZ,
    /// `[` or `]` pressed (`forward` for `]`), optionally after an operator,
    /// waiting for the key of the block kind to move to.
    WaitingBracket(Option<Op>, bool),
    /// `m`, `'` or `` ` `` pressed, waiting for the name of the mark to set or
    /// go to; `''` and `` `` `` go back to before the latest jump.
    WaitingMark(char),
    /// `"` pressed, waiting for the name of the register the next command uses.
    WaitingRegister,
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
    /// `ys` pressed, waiting for a motion or text-object prefix.
    WaitingSurroundMotion,
    /// `ysi`/`ysa` pressed, waiting for object type char.
    WaitingSurroundTextObj(bool),
    /// A surround target range was resolved, waiting for the delimiter to add.
    WaitingSurroundChar(usize, usize),
    /// `ds` pressed, waiting for delimiter kind to remove.
    WaitingDeleteSurround,
    /// `cs` pressed, waiting for delimiter kind to replace.
    WaitingChangeSurroundTarget,
    /// `cs{target}` pressed, waiting for replacement delimiter.
    WaitingChangeSurroundReplacement(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeatKey {
    Char(char),
    Left,
    Right,
    Backspace,
    Home,
    End,
}

impl RepeatKey {
    fn from_key(key: KeyEvent) -> Option<Self> {
        match key.code {
            KeyCode::Char(c) => Some(Self::Char(c)),
            KeyCode::Left => Some(Self::Left),
            KeyCode::Right => Some(Self::Right),
            KeyCode::Backspace => Some(Self::Backspace),
            KeyCode::Home => Some(Self::Home),
            KeyCode::End => Some(Self::End),
            _ => None,
        }
    }

    fn key_event(self) -> KeyEvent {
        KeyEvent::new(
            match self {
                Self::Char(c) => KeyCode::Char(c),
                Self::Left => KeyCode::Left,
                Self::Right => KeyCode::Right,
                Self::Backspace => KeyCode::Backspace,
                Self::Home => KeyCode::Home,
                Self::End => KeyCode::End,
            },
            KeyModifiers::empty(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeatCommand {
    Direct {
        command: char,
        count: usize,
    },
    Replace {
        count: usize,
        replacement: char,
    },
    OpMotion {
        op: Op,
        motion: RepeatKey,
        count: usize,
    },
    OpFind {
        op: Op,
        kind: FindKind,
        target: char,
        count: usize,
    },
    Linewise {
        op: Op,
        count: usize,
    },
    JoinWithoutSpaces {
        count: usize,
    },
    Add {
        subtract: bool,
        count: usize,
    },
    TextObject {
        op: Op,
        inner: bool,
        object: char,
        count: usize,
    },
    DeleteSurround {
        target: char,
    },
    ChangeSurround {
        target: char,
        replacement: char,
    },
    Visual {
        action: VisualAction,
        shape: VisualShape,
    },
}

// ── Vim state ───────────────────────────────────────────────────────────────

/// An insert session being recorded so `.` and a count can type it again.
#[derive(Clone, Copy, Debug)]
struct InsertSession {
    /// Where typing started, and the buffer length then.
    start: usize,
    len: usize,
    /// How many times the typed text goes in; more than one repeats it.
    count: usize,
    /// Typed text repeats on new lines (`3ofoo`), after an indent of this size.
    opens_line: bool,
    auto_indent: usize,
}

/// A visual selection as `gv` restores it.
#[derive(Clone, Copy, Debug)]
struct LastVisual {
    anchor: usize,
    cursor: usize,
    mode: VimMode,
}

/// Per-Window vim state: persistent slots (`visual_anchor`, `last_find`) and
/// in-flight key-sequence accumulators (`sub`, `count1`, `count2`).
#[derive(Clone, Copy, Debug, Default)]
pub struct VimWindowState {
    /// Visual-mode anchor byte; meaningful only in `Visual`/`VisualLine`.
    pub(crate) visual_anchor: usize,
    /// Last `f`/`t`/`F`/`T` target for `;`/`,` replay.
    pub(crate) last_find: Option<(FindKind, char)>,
    /// Last visual selection for `gv`.
    last_visual: Option<LastVisual>,
    /// The insert session in progress, started by the command that entered it.
    insert: Option<InsertSession>,
    /// Count typed before `i`, `a`, `o`... which repeats what gets typed.
    insert_count: usize,
    /// Indent the command that entered Insert mode put on the line for the
    /// user, which stays only if they type something.
    auto_indent: usize,
    /// The command that entered Insert mode opened a new line (`o`, `O`), so a
    /// count types the text again on further lines.
    opens_line: bool,
    /// Where the last insert session ended, for `gi`.
    last_insert_end: Option<usize>,
    /// Last mutating Normal-mode command for `.` repeat, with its register.
    last_change: Option<(RepeatCommand, Option<char>)>,
    /// Guard used while replaying `.` so the repeated command does not replace itself.
    replaying_change: bool,
    /// In-flight sub-state for multi-key sequences.
    pub(crate) sub: SubState,
    /// Register named with `"` for the command being typed.
    pub(crate) register: Option<char>,
    /// Count before the operator (or standalone motion).
    pub(crate) count1: Option<usize>,
    /// Count after the operator, before the motion.
    pub(crate) count2: Option<usize>,
}

impl VimWindowState {
    /// Visual anchor, snapped to a grapheme boundary in `buf`. All consumers
    /// must go through this accessor - the raw field may be stale.
    pub fn visual_anchor_at(&self, buf: &str) -> usize {
        smelt_buffer::text::snap_grapheme(buf, self.visual_anchor)
    }

    /// Raw stored anchor without snapping. For invariant checks that
    /// want to detect drift past `text().len()` before the snap happens.
    pub fn visual_anchor_raw(&self) -> usize {
        self.visual_anchor
    }

    /// Reset the visual anchor to 0. Call after wholesale source swaps so
    /// stale anchors can't outlive the bytes they pointed at.
    pub fn clear_visual_anchor(&mut self) {
        self.visual_anchor = 0;
    }

    /// Shift the visual anchor right by `delta` bytes. Call after
    /// prepending text to the buffer so the anchor keeps pointing at the
    /// same character (cpos is shifted by the caller too).
    pub fn shift_visual_anchor(&mut self, delta: usize, source: &str) {
        self.visual_anchor =
            smelt_buffer::text::ceil_grapheme(source, self.visual_anchor.saturating_add(delta));
    }

    /// Clamp the visual anchor into `source` and snap it to a grapheme
    /// boundary. Call after any in-place source shrink so the anchor
    /// preserved for `gv` can never outlive its terminal glyph.
    pub fn clamp_visual_anchor(&mut self, source: &str) {
        if self.visual_anchor > source.len() {
            self.visual_anchor = source.len();
        }
        self.visual_anchor = smelt_buffer::text::snap_grapheme(source, self.visual_anchor);
    }

    fn record_change(&mut self, command: RepeatCommand) {
        if !self.replaying_change {
            self.last_change = Some((command, self.register));
        }
    }

    fn repeat_count(&mut self, stored: usize) -> usize {
        self.count1.take().unwrap_or(stored.max(1))
    }

    /// Pop count1 (default 1), clearing both accumulators.
    fn take_count(&mut self) -> usize {
        let n = self.count1.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        n
    }

    /// Pop the typed count (`count1 × count2`), or `None` when none was typed,
    /// clearing both.
    fn take_typed_count(&mut self) -> Option<usize> {
        let typed = match (self.count1, self.count2) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(1) * b.unwrap_or(1)),
        };
        self.reset_counts();
        typed
    }

    /// Pop count1 × count2 (each default 1) and clear both.
    fn effective_count(&mut self) -> usize {
        let c1 = self.count1.unwrap_or(1);
        let c2 = self.count2.unwrap_or(1);
        self.count1 = None;
        self.count2 = None;
        c1 * c2
    }

    /// Clear count accumulators; leaves `sub` untouched.
    fn reset_counts(&mut self) {
        self.count1 = None;
        self.count2 = None;
    }

    /// Reset the entire pending sequence: `sub = Ready`, counts cleared.
    fn reset_pending(&mut self) {
        self.sub = SubState::Ready;
        self.reset_counts();
    }

    /// Name the register for the command being typed, from the key after `"`.
    /// `""` names the unnamed register; anything else cancels the sequence.
    fn name_register(&mut self, key: KeyEvent) {
        self.sub = SubState::Ready;
        match key.code {
            KeyCode::Char('"') => self.register = None,
            KeyCode::Char(c @ ('a'..='z' | 'A'..='Z' | '0' | '_' | '+' | '*' | '.')) => {
                self.register = Some(c);
            }
            _ => {
                self.register = None;
                self.reset_counts();
            }
        }
    }

    /// Forget the register once the command that named it is done, unless
    /// `named` says this key was the register's name.
    fn settle_register(&mut self, named: bool) {
        if !named && matches!(self.sub, SubState::Ready) && self.count1.is_none() {
            self.register = None;
        }
    }

    /// The register named for the command just run, which clears it. Viewer
    /// hosts read it after running a yank.
    pub fn take_register(&mut self) -> Option<char> {
        self.register.take()
    }

    /// True when no multi-key sequence and no count accumulator are pending,
    /// i.e. Esc in Normal mode would be a no-op.
    pub fn is_idle(&self) -> bool {
        self.pending_input().is_none()
    }

    /// User-facing key sequence currently waiting for more vim input.
    ///
    /// Counts are included because they are part of the pending command: after
    /// `3d2` the next key is still a motion for that exact sequence.
    pub fn pending_input(&self) -> Option<String> {
        let mut out = String::new();
        if let Some(register) = self.register {
            out.push('"');
            out.push(register);
        }
        if let Some(count) = self.count1 {
            out.push_str(&count.to_string());
        }

        let push_op = |out: &mut String, op: Op| {
            out.push_str(op.label());
            if let Some(count) = self.count2 {
                out.push_str(&count.to_string());
            }
        };

        match self.sub {
            SubState::Ready => {}
            SubState::WaitingOp(op) => push_op(&mut out, op),
            SubState::WaitingG => out.push('g'),
            SubState::WaitingZ => out.push('z'),
            SubState::WaitingBracket(op, forward) => {
                if let Some(op) = op {
                    push_op(&mut out, op);
                }
                out.push(if forward { ']' } else { '[' });
            }
            SubState::WaitingMark(quote) => out.push(quote),
            SubState::WaitingRegister => out.push('"'),
            SubState::WaitingOpG(op) => {
                push_op(&mut out, op);
                out.push('g');
            }
            SubState::WaitingR => out.push('r'),
            SubState::WaitingFind(kind) => out.push(find_kind_char(kind)),
            SubState::WaitingOpFind(op, kind) => {
                push_op(&mut out, op);
                out.push(find_kind_char(kind));
            }
            SubState::WaitingTextObj(op, inner) => {
                push_op(&mut out, op);
                out.push(if inner { 'i' } else { 'a' });
            }
            SubState::WaitingVisualTextObj(inner) => out.push(if inner { 'i' } else { 'a' }),
            SubState::WaitingSurroundMotion => out.push_str("ys"),
            SubState::WaitingSurroundTextObj(inner) => {
                out.push_str("ys");
                out.push(if inner { 'i' } else { 'a' });
            }
            SubState::WaitingSurroundChar(_, _) => out.push_str("ys"),
            SubState::WaitingDeleteSurround => out.push_str("ds"),
            SubState::WaitingChangeSurroundTarget => out.push_str("cs"),
            SubState::WaitingChangeSurroundReplacement(target) => {
                out.push_str("cs");
                out.push(target);
            }
        }

        (!out.is_empty()).then_some(out)
    }

    /// Set mode and clear the pending sequence.
    pub fn set_mode(&mut self, mode_ref: &mut VimMode, mode: VimMode) {
        *mode_ref = mode;
        self.reset_pending();
    }

    /// Remember the selection in progress so `gv` can restore it after the
    /// mode is left. Call before handling each key in Visual modes.
    pub fn remember_visual(&mut self, mode: VimMode, cursor: usize) {
        if matches!(mode, VimMode::Visual | VimMode::VisualLine) {
            self.last_visual = Some(LastVisual {
                anchor: self.visual_anchor,
                cursor,
                mode,
            });
        }
    }

    /// `gv`: re-enter the last visual selection, clamped to `buf`. Returns
    /// false when there is none.
    pub fn reselect_visual(&mut self, mode_ref: &mut VimMode, cpos: &mut usize, buf: &str) -> bool {
        let Some(last) = self.last_visual else {
            return false;
        };
        self.begin_visual(mode_ref, last.mode, last.anchor);
        self.clamp_visual_anchor(buf);
        *cpos = smelt_buffer::text::snap_grapheme(buf, last.cursor.min(buf.len()));
        true
    }

    /// Anchor a visual selection at `cpos` and enter the given visual mode.
    pub fn begin_visual(&mut self, mode_ref: &mut VimMode, mode: VimMode, cpos: usize) {
        *mode_ref = mode;
        self.reset_pending();
        self.visual_anchor = cpos;
    }
}

/// Visual selection as ordered byte offsets, or `None` outside Visual modes.
pub fn visual_range(
    state: &VimWindowState,
    buf: &str,
    cpos: usize,
    mode: VimMode,
) -> Option<(usize, usize)> {
    match mode {
        VimMode::Visual => {
            let anchor = state.visual_anchor_at(buf);
            let cursor = smelt_buffer::text::snap_grapheme(buf, cpos);
            let (a, b) = if anchor <= cursor {
                (anchor, next_grapheme_boundary(buf, cursor).min(buf.len()))
            } else {
                (cursor, next_grapheme_boundary(buf, anchor).min(buf.len()))
            };
            Some((a, b))
        }
        VimMode::VisualLine => {
            let anchor = state.visual_anchor_at(buf);
            let cursor = smelt_buffer::text::snap_grapheme(buf, cpos);
            let start = line_start(buf, anchor).min(line_start(buf, cursor));
            let end = line_end(buf, anchor).max(line_end(buf, cursor));
            Some((start, end))
        }
        _ => None,
    }
}

/// Byte range removed by linewise operations for a line-content range.
pub fn linewise_delete_range(buf: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    if range.end < buf.len() && buf.as_bytes()[range.end] == b'\n' {
        range.start..range.end + 1
    } else if range.start > 0 && buf.as_bytes()[range.start - 1] == b'\n' {
        range.start - 1..range.end
    } else {
        range
    }
}

/// Visual-mode anchor byte (snapped against `buf`), or `None` in Normal/Insert.
pub fn visual_anchor(state: &VimWindowState, buf: &str, mode: VimMode) -> Option<usize> {
    match mode {
        VimMode::Visual | VimMode::VisualLine => Some(state.visual_anchor_at(buf)),
        _ => None,
    }
}

/// Process a key event, mutating `ctx` (buffer, cursor, kill ring, undo, mode).
pub fn handle_key(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    let named = matches!(ctx.vim_state.sub, SubState::WaitingRegister);
    let action = match *ctx.mode {
        _ if named => {
            ctx.vim_state.name_register(key);
            Action::Consumed
        }
        VimMode::Insert => handle_insert(key, ctx),
        VimMode::Normal => handle_normal(key, ctx),
        VimMode::Visual | VimMode::VisualLine => handle_visual(key, ctx),
    };
    ctx.vim_state.settle_register(named);
    action
}

// ── Insert mode ─────────────────────────────────────────────────────

fn handle_insert(key: KeyEvent, ctx: &mut VimContext<'_>) -> Action {
    match key {
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
        KeyEvent {
            code: KeyCode::Char('w' | 'u'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => Action::Passthrough,
        KeyEvent {
            code: KeyCode::Char('h'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => Action::Passthrough,
        _ => Action::Passthrough,
    }
}

/// The display column the cursor wants within its line.
fn cursor_cell(ctx: &VimContext<'_>) -> usize {
    ctx.curswant.unwrap_or_else(|| {
        let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
        byte_to_cell(&ctx.buf.as_str()[sol..*ctx.cpos], *ctx.cpos - sol)
    })
}

/// Put the cursor on the line holding `pos` at display column `cell`, or on
/// its last character when the line is shorter.
fn place_on_line(ctx: &mut VimContext<'_>, pos: usize, cell: usize) {
    let pos = pos.min(ctx.buf.len());
    let sol = line_start(ctx.buf.as_str(), pos);
    let eol = line_end(ctx.buf.as_str(), pos);
    *ctx.cpos = sol + cell_to_byte(&ctx.buf.as_str()[sol..eol], cell);
    clamp_cursor(ctx);
}

// ── Mode transitions ────────────────────────────────────────────────

fn enter_insert_mode(ctx: &mut VimContext<'_>) {
    *ctx.mode = VimMode::Insert;
    ctx.vim_state.sub = SubState::Ready;
    let count = std::mem::replace(&mut ctx.vim_state.insert_count, 1);
    ctx.vim_state.insert = Some(InsertSession {
        start: *ctx.cpos,
        len: ctx.buf.len(),
        count,
        opens_line: std::mem::take(&mut ctx.vim_state.opens_line),
        auto_indent: std::mem::take(&mut ctx.vim_state.auto_indent),
    });
}

fn exit_visual(ctx: &mut VimContext<'_>) {
    *ctx.mode = VimMode::Normal;
    ctx.vim_state.reset_pending();
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

/// Normal mode keeps the cursor on a character. Visual mode lets it rest on
/// the line break so the selection can take it.
fn clamp_cursor(ctx: &mut VimContext<'_>) {
    if !matches!(*ctx.mode, VimMode::Visual | VimMode::VisualLine) {
        clamp_normal(ctx.buf.as_str(), ctx.cpos);
    }
}

/// Leave Insert mode for Normal: finish the insert session, then step the
/// cursor back onto the last typed character.
fn enter_normal(ctx: &mut VimContext<'_>) {
    finish_insert_session(ctx);
    *ctx.mode = VimMode::Normal;
    ctx.vim_state.sub = SubState::Ready;
    ctx.vim_state.reset_counts();
    ctx.vim_state.last_insert_end = Some(*ctx.cpos);
    // Leaving insert mode moves cursor left one, unless at start of line.
    let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
    if *ctx.cpos > sol {
        *ctx.cpos = prev_grapheme_boundary(ctx.buf.as_str(), *ctx.cpos);
    }
    clamp_normal(ctx.buf.as_str(), ctx.cpos);
}

/// Work out what the user typed in the insert session that is ending, if it
/// was plain typing at the cursor. Remember it for `.`, type it again for a
/// count, and drop an auto indent they did not use.
fn finish_insert_session(ctx: &mut VimContext<'_>) {
    let Some(session) = ctx.vim_state.insert.take() else {
        return;
    };
    let len = ctx.buf.len();
    let typed = (*ctx.cpos >= session.start
        && len >= session.len
        && len - session.len == *ctx.cpos - session.start)
        .then(|| smelt_buffer::text::slice(ctx.buf.as_str(), session.start..*ctx.cpos).to_string());
    ctx.clipboard.kill_ring.set_last_insert(typed.clone());
    let Some(typed) = typed else {
        return;
    };
    if typed.is_empty() && session.auto_indent > 0 {
        let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
        let eol = line_end(ctx.buf.as_str(), *ctx.cpos);
        let line = &ctx.buf.as_str()[sol..eol];
        if line.len() == session.auto_indent && line.chars().all(|c| c == ' ' || c == '\t') {
            ctx.delete_range(sol, eol);
            *ctx.cpos = sol;
        }
        return;
    }
    if session.count > 1 && !typed.is_empty() {
        let again = if session.opens_line {
            let sol = line_start(ctx.buf.as_str(), *ctx.cpos);
            let indent = ctx.buf.as_str()[sol..]
                .chars()
                .take(session.auto_indent)
                .collect::<String>();
            format!("\n{indent}{typed}")
        } else {
            typed
        };
        let repeated = again.repeat(session.count - 1);
        let at = *ctx.cpos;
        let start = ctx.insert_str(at, &repeated);
        *ctx.cpos = start + repeated.len();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod parity;

#[cfg(test)]
mod tests;
