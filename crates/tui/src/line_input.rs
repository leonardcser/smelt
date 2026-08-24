//! Shared single-line text editing primitives for command/search and dialog inputs.
//!
//! Cursor and selection offsets are byte indices into `text`. Every operation
//! snaps offsets through `smelt_buffer::text` so stale positions never panic on
//! UTF-8 boundaries.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use smelt_buffer::text;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineEdit {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct LineInputKeyBinding {
    pub(crate) bind: crate::smelt_edit::KeyBind,
    pub(crate) command: EditCommand,
}

impl LineInputKeyBinding {
    fn new(code: KeyCode, mods: KeyModifiers, command: EditCommand) -> LineInputKeyBinding {
        LineInputKeyBinding {
            bind: crate::smelt_edit::KeyBind { code, mods },
            command,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditCommand {
    InsertChar(char),
    InsertText(String),
    Backspace,
    Delete,
    DeleteWordBack,
    DeleteWordForward,
    DeleteToStart,
    DeleteToEnd,
    MoveLeft { select: bool },
    MoveRight { select: bool },
    MoveWordLeft { select: bool },
    MoveWordRight { select: bool },
    MoveHome { select: bool },
    MoveEnd { select: bool },
    SelectAll,
}

impl LineEdit {
    pub(crate) fn new(text: String, cursor: usize) -> Self {
        let cursor = text::snap_grapheme(&text, cursor);
        Self {
            text,
            cursor,
            selection_anchor: None,
        }
    }

    pub(crate) fn with_selection(
        text: String,
        cursor: usize,
        selection_anchor: Option<usize>,
    ) -> Self {
        let cursor = text::snap_grapheme(&text, cursor);
        let selection_anchor = selection_anchor
            .map(|anchor| text::snap_grapheme(&text, anchor))
            .filter(|anchor| *anchor != cursor);
        Self {
            text,
            cursor,
            selection_anchor,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn selection_anchor(&self) -> Option<usize> {
        self.selection_anchor
    }

    pub(crate) fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.selection_anchor?;
        let cursor = self.cursor;
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        (start != end).then_some(start..end)
    }

    pub(crate) fn apply(&mut self, command: EditCommand) -> bool {
        let before = self.clone();
        match command {
            EditCommand::InsertChar(c) => {
                let mut buf = [0; 4];
                let s = c.encode_utf8(&mut buf);
                self.insert_text(s);
            }
            EditCommand::InsertText(s) => {
                let normalized = normalize_single_line(&s);
                if !normalized.is_empty() {
                    self.insert_text(&normalized);
                }
            }
            EditCommand::Backspace => self.backspace(),
            EditCommand::Delete => self.delete_forward(),
            EditCommand::DeleteWordBack => self.delete_word_back(),
            EditCommand::DeleteWordForward => self.delete_word_forward(),
            EditCommand::DeleteToStart => self.delete_to(0),
            EditCommand::DeleteToEnd => self.delete_to(self.text.len()),
            EditCommand::MoveLeft { select } => {
                let new = text::prev_grapheme_boundary(&self.text, self.cursor);
                self.move_to(new, select);
            }
            EditCommand::MoveRight { select } => {
                let new = text::next_grapheme_boundary(&self.text, self.cursor);
                self.move_to(new, select);
            }
            EditCommand::MoveWordLeft { select } => self.move_to(self.prev_word_boundary(), select),
            EditCommand::MoveWordRight { select } => {
                self.move_to(self.next_word_boundary(), select)
            }
            EditCommand::MoveHome { select } => self.move_to(0, select),
            EditCommand::MoveEnd { select } => self.move_to(self.text.len(), select),
            EditCommand::SelectAll => {
                self.cursor = self.text.len();
                self.selection_anchor = (!self.text.is_empty()).then_some(0);
            }
        }
        *self != before
    }

    fn insert_text(&mut self, s: &str) {
        if self.replace_selection(s) {
            return;
        }
        self.replace_selection_or_range(self.cursor..self.cursor, s);
    }

    fn backspace(&mut self) {
        if self.replace_selection("") {
            return;
        }
        let start = text::prev_grapheme_boundary(&self.text, self.cursor);
        self.replace_selection_or_range(start..self.cursor, "");
    }

    fn delete_forward(&mut self) {
        if self.replace_selection("") {
            return;
        }
        let end = text::next_grapheme_boundary(&self.text, self.cursor);
        self.replace_selection_or_range(self.cursor..end, "");
    }

    fn delete_word_back(&mut self) {
        if self.replace_selection("") {
            return;
        }
        let start = self.prev_word_boundary();
        self.replace_selection_or_range(start..self.cursor, "");
    }

    fn delete_word_forward(&mut self) {
        if self.replace_selection("") {
            return;
        }
        let end = self.next_word_boundary();
        self.replace_selection_or_range(self.cursor..end, "");
    }

    fn delete_to(&mut self, target: usize) {
        if self.replace_selection("") {
            return;
        }
        let target = text::snap_grapheme(&self.text, target);
        let (start, end) = if target <= self.cursor {
            (target, self.cursor)
        } else {
            (self.cursor, target)
        };
        self.replace_selection_or_range(start..end, "");
    }

    fn replace_selection(&mut self, with: &str) -> bool {
        let Some(range) = self.selection_range() else {
            return false;
        };
        self.replace_selection_or_range(range, with);
        true
    }

    fn replace_selection_or_range(&mut self, range: std::ops::Range<usize>, with: &str) {
        let range = text::snapped_grapheme_range(&self.text, range);
        let start = range.start;
        text::replace_range(&mut self.text, range, with);
        let cursor = start + with.len();
        self.cursor = if with.is_empty() {
            text::snap_grapheme(&self.text, cursor)
        } else {
            text::ceil_grapheme(&self.text, cursor)
        };
        self.selection_anchor = None;
    }

    fn move_to(&mut self, pos: usize, select: bool) {
        let pos = text::snap_grapheme(&self.text, pos);
        if select {
            if self.selection_anchor.is_none() && pos != self.cursor {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = pos;
        if self.selection_anchor == Some(self.cursor) {
            self.selection_anchor = None;
        }
    }

    fn prev_word_boundary(&self) -> usize {
        let mut pos = text::snap_grapheme(&self.text, self.cursor);
        if pos == 0 {
            return 0;
        }
        while pos > 0 {
            let prev = text::prev_grapheme_boundary(&self.text, pos);
            let ch = text::slice(&self.text, prev..pos).chars().next();
            if !matches!(ch, Some(c) if c.is_whitespace()) {
                break;
            }
            pos = prev;
        }
        if pos == 0 {
            return 0;
        }
        let prev = text::prev_grapheme_boundary(&self.text, pos);
        let word = text::slice(&self.text, prev..pos)
            .chars()
            .next()
            .is_some_and(is_word_char);
        while pos > 0 {
            let prev = text::prev_grapheme_boundary(&self.text, pos);
            let ch = text::slice(&self.text, prev..pos).chars().next();
            if ch.is_some_and(is_word_char) != word || ch.is_some_and(char::is_whitespace) {
                break;
            }
            pos = prev;
        }
        pos
    }

    fn next_word_boundary(&self) -> usize {
        let mut pos = text::snap_grapheme(&self.text, self.cursor);
        let len = self.text.len();
        if pos >= len {
            return len;
        }
        while pos < len {
            let next = text::next_grapheme_boundary(&self.text, pos);
            let ch = text::slice(&self.text, pos..next).chars().next();
            if !matches!(ch, Some(c) if c.is_whitespace()) {
                break;
            }
            pos = next;
        }
        if pos >= len {
            return len;
        }
        let next = text::next_grapheme_boundary(&self.text, pos);
        let word = text::slice(&self.text, pos..next)
            .chars()
            .next()
            .is_some_and(is_word_char);
        while pos < len {
            let next = text::next_grapheme_boundary(&self.text, pos);
            let ch = text::slice(&self.text, pos..next).chars().next();
            if ch.is_some_and(is_word_char) != word || ch.is_some_and(char::is_whitespace) {
                break;
            }
            pos = next;
        }
        pos
    }
}

pub(crate) fn default_key_bindings() -> Vec<LineInputKeyBinding> {
    use EditCommand as E;
    use KeyCode as K;
    use KeyModifiers as M;

    vec![
        LineInputKeyBinding::new(K::Backspace, M::NONE, E::Backspace),
        LineInputKeyBinding::new(K::Backspace, M::ALT, E::DeleteWordBack),
        LineInputKeyBinding::new(K::Backspace, M::CONTROL, E::DeleteWordBack),
        LineInputKeyBinding::new(K::Delete, M::NONE, E::Delete),
        LineInputKeyBinding::new(K::Delete, M::ALT, E::DeleteWordForward),
        LineInputKeyBinding::new(K::Delete, M::CONTROL, E::DeleteWordForward),
        LineInputKeyBinding::new(K::Left, M::NONE, E::MoveLeft { select: false }),
        LineInputKeyBinding::new(K::Left, M::SHIFT, E::MoveLeft { select: true }),
        LineInputKeyBinding::new(K::Left, M::ALT, E::MoveWordLeft { select: false }),
        LineInputKeyBinding::new(K::Left, M::ALT | M::SHIFT, E::MoveWordLeft { select: true }),
        LineInputKeyBinding::new(K::Left, M::CONTROL, E::MoveWordLeft { select: false }),
        LineInputKeyBinding::new(
            K::Left,
            M::CONTROL | M::SHIFT,
            E::MoveWordLeft { select: true },
        ),
        LineInputKeyBinding::new(K::Right, M::NONE, E::MoveRight { select: false }),
        LineInputKeyBinding::new(K::Right, M::SHIFT, E::MoveRight { select: true }),
        LineInputKeyBinding::new(K::Right, M::ALT, E::MoveWordRight { select: false }),
        LineInputKeyBinding::new(
            K::Right,
            M::ALT | M::SHIFT,
            E::MoveWordRight { select: true },
        ),
        LineInputKeyBinding::new(K::Right, M::CONTROL, E::MoveWordRight { select: false }),
        LineInputKeyBinding::new(
            K::Right,
            M::CONTROL | M::SHIFT,
            E::MoveWordRight { select: true },
        ),
        LineInputKeyBinding::new(K::Home, M::NONE, E::MoveHome { select: false }),
        LineInputKeyBinding::new(K::Home, M::SHIFT, E::MoveHome { select: true }),
        LineInputKeyBinding::new(K::End, M::NONE, E::MoveEnd { select: false }),
        LineInputKeyBinding::new(K::End, M::SHIFT, E::MoveEnd { select: true }),
        LineInputKeyBinding::new(K::Char('a'), M::CONTROL, E::MoveHome { select: false }),
        LineInputKeyBinding::new(K::Char('e'), M::CONTROL, E::MoveEnd { select: false }),
        LineInputKeyBinding::new(K::Char('w'), M::CONTROL, E::DeleteWordBack),
        LineInputKeyBinding::new(K::Char('u'), M::CONTROL, E::DeleteToStart),
        LineInputKeyBinding::new(K::Char('k'), M::CONTROL, E::DeleteToEnd),
        LineInputKeyBinding::new(K::Char('a'), M::SUPER, E::SelectAll),
    ]
}

pub(crate) fn command_for_key(k: KeyEvent) -> Option<EditCommand> {
    let text_mods = k.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
    if let KeyCode::Char(c) = k.code {
        if text_mods == KeyModifiers::NONE {
            return Some(EditCommand::InsertChar(c));
        }
    }

    let bind = crate::smelt_edit::KeyBind::new(k.code, k.modifiers);
    default_key_bindings()
        .into_iter()
        .find(|entry| entry.bind == bind)
        .map(|entry| entry.command)
}

pub(crate) fn normalize_single_line(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for c in input.chars() {
        if matches!(c, '\n' | '\r') {
            pending_space = true;
            continue;
        }
        if pending_space
            && !out.is_empty()
            && !out.chars().next_back().is_some_and(char::is_whitespace)
            && !c.is_whitespace()
        {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_text_at_cursor() {
        let mut edit = LineEdit::new("helo".into(), 2);
        assert!(edit.apply(EditCommand::InsertChar('l')));
        assert_eq!(edit.text(), "hello");
        assert_eq!(edit.cursor(), 3);
    }

    #[test]
    fn paste_collapses_newlines_to_spaces() {
        let mut edit = LineEdit::new("say ".into(), 4);
        edit.apply(EditCommand::InsertText("hello\nworld\r\nagain".into()));
        assert_eq!(edit.text(), "say hello world again");
    }

    #[test]
    fn cursor_motion_is_utf8_safe() {
        let mut edit = LineEdit::new("a日本b".into(), 1);
        edit.apply(EditCommand::MoveRight { select: false });
        assert_eq!(edit.cursor(), 4);
        edit.apply(EditCommand::MoveRight { select: false });
        assert_eq!(edit.cursor(), 7);
        edit.apply(EditCommand::Backspace);
        assert_eq!(edit.text(), "a日b");
        assert_eq!(edit.cursor(), 4);
    }

    #[test]
    fn cursor_selection_and_deletion_keep_graphemes_atomic() {
        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let text = format!("a{grapheme}b");
            let start = 1;
            let end = start + grapheme.len();
            let mut edit = LineEdit::new(text.clone(), start);

            edit.apply(EditCommand::MoveRight { select: true });
            assert_eq!(edit.selection_range(), Some(start..end), "{grapheme:?}");
            edit.apply(EditCommand::Backspace);
            assert_eq!(edit.text(), "ab", "{grapheme:?}");

            let mut edit = LineEdit::new(text, end);
            edit.apply(EditCommand::Backspace);
            assert_eq!(edit.text(), "ab", "{grapheme:?}");
            assert_eq!(edit.cursor(), start, "{grapheme:?}");
        }
    }

    #[test]
    fn pasted_graphemes_are_preserved_exactly() {
        let pasted = "besta\u{308}tigt 👩\u{200d}💻 9\u{fe0f}";
        let mut edit = LineEdit::new(String::new(), 0);
        edit.apply(EditCommand::InsertText(pasted.into()));
        assert_eq!(edit.text(), pasted);
        assert_eq!(edit.cursor(), pasted.len());
    }

    #[test]
    fn insert_cursor_moves_after_graphemes_formed_with_following_text() {
        for (inserted, following) in [
            ("e", "\u{301}z"),
            ("9", "\u{fe0f}z"),
            ("👩", "\u{200d}💻z"),
            ("🇨", "🇦z"),
        ] {
            let mut edit = LineEdit::new(following.into(), 0);
            edit.apply(EditCommand::InsertText(inserted.into()));
            assert_eq!(edit.text(), format!("{inserted}{following}"));
            assert_eq!(
                text::snap_grapheme(edit.text(), edit.cursor()),
                edit.cursor()
            );
            assert_eq!(edit.cursor(), text::next_grapheme_boundary(edit.text(), 0));
        }
    }

    #[test]
    fn deletion_cursor_stays_valid_when_neighbors_form_a_grapheme() {
        let mut edit = LineEdit::new("🇨 🇦z".into(), "🇨 ".len());
        edit.apply(EditCommand::Backspace);
        assert_eq!(edit.text(), "🇨🇦z");
        assert_eq!(
            text::snap_grapheme(edit.text(), edit.cursor()),
            edit.cursor()
        );
    }

    #[test]
    fn replaces_selection_on_insert() {
        let mut edit = LineEdit::with_selection("hello".into(), 4, Some(1));
        edit.apply(EditCommand::InsertText("i".into()));
        assert_eq!(edit.text(), "hio");
        assert_eq!(edit.cursor(), 2);
        assert_eq!(edit.selection_anchor(), None);
    }

    #[test]
    fn shift_motion_creates_selection() {
        let mut edit = LineEdit::new("hello".into(), 5);
        edit.apply(EditCommand::MoveLeft { select: true });
        edit.apply(EditCommand::MoveLeft { select: true });
        assert_eq!(edit.selection_range(), Some(3..5));
        edit.apply(EditCommand::Backspace);
        assert_eq!(edit.text(), "hel");
    }

    #[test]
    fn word_delete_uses_word_boundaries() {
        let mut edit = LineEdit::new("hello world".into(), "hello world".len());
        edit.apply(EditCommand::DeleteWordBack);
        assert_eq!(edit.text(), "hello ");
        assert_eq!(edit.cursor(), 6);
    }
}
