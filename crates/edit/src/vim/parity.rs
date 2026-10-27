//! Differential driver for checking prompt vim behavior against neovim.
//!
//! `dump_parity_cases` is ignored by default. It reads cases from the file named
//! by `SMELT_PARITY_CASES` and writes the outcomes to `SMELT_PARITY_OUT`, one
//! tab separated line per case. A script runs the same cases in `nvim --headless`
//! and diffs the two.
//!
//! Case line: `text<TAB>keys<TAB>line<TAB>col`. In `text` and `keys`, `\n` is a
//! newline, `\\` a backslash and `\t` a tab; `keys` also takes `<Esc>`, `<CR>`,
//! `<BS>`, `<Tab>`, `<lt>` and `<C-x>`. `line` is 1-based and `col` a 0-based
//! byte column.
//!
//! Result line: `text<TAB>line<TAB>col<TAB>register<TAB>mode<TAB>regtype`, where
//! `regtype` is `V` for a linewise register, `v` for a charwise one.

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

fn unescape(field: &str) -> String {
    let mut out = String::new();
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    }
}

fn parse_keys(keys: &str) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut rest = keys;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            if let Some(end) = rest.find('>') {
                let name = &rest[1..end];
                let event = match name {
                    "Esc" => Some(key_event(KeyCode::Esc, KeyModifiers::empty())),
                    "CR" => Some(key_event(KeyCode::Enter, KeyModifiers::empty())),
                    "BS" => Some(key_event(KeyCode::Backspace, KeyModifiers::empty())),
                    "Tab" => Some(key_event(KeyCode::Tab, KeyModifiers::empty())),
                    "lt" => Some(key_event(KeyCode::Char('<'), KeyModifiers::empty())),
                    _ => name
                        .strip_prefix("C-")
                        .and_then(|k| k.chars().next())
                        .map(|k| key_event(KeyCode::Char(k), KeyModifiers::CONTROL)),
                };
                if let Some(event) = event {
                    out.push(event);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        let modifiers = if c.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::empty()
        };
        out.push(key_event(KeyCode::Char(c), modifiers));
        rest = &rest[c.len_utf8()..];
    }
    out
}

struct Session {
    buf: String,
    cpos: usize,
    attachments: Vec<smelt_buffer::attachment::AttachmentId>,
    clipboard: Clipboard,
    history: UndoHistory,
    mode: VimMode,
    curswant: Option<usize>,
    vim_state: VimWindowState,
}

impl Session {
    fn press(&mut self, key: KeyEvent) {
        let mut ctx = VimContext {
            buf: AttachedTextMut::new(&mut self.buf, &mut self.attachments),
            cpos: &mut self.cpos,
            history: &mut self.history,
            clipboard: &mut self.clipboard,
            mode: &mut self.mode,
            curswant: &mut self.curswant,
            vim_state: &mut self.vim_state,
            now: std::time::Instant::now(),
        };
        let action = handle_key(key, &mut ctx);
        if action == Action::Passthrough && self.mode == VimMode::Insert {
            self.type_in_insert(key);
        }
    }

    /// Insert mode typing is the host's job; this does the plain cases.
    fn type_in_insert(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        let mut buf = AttachedTextMut::new(&mut self.buf, &mut self.attachments);
        match key.code {
            KeyCode::Char(c) => {
                buf.insert(self.cpos, c);
                self.cpos += c.len_utf8();
            }
            KeyCode::Enter => {
                buf.insert(self.cpos, '\n');
                self.cpos += 1;
            }
            KeyCode::Backspace if self.cpos > 0 => {
                let prev = prev_grapheme_boundary(buf.as_str(), self.cpos);
                buf.replace_range(prev..self.cpos, "");
                self.cpos = prev;
            }
            _ => {}
        }
    }
}

fn run_case(line: &str) -> String {
    let mut fields = line.split('\t');
    let text = unescape(fields.next().unwrap_or_default());
    let keys = parse_keys(&unescape(fields.next().unwrap_or_default()));
    let row: usize = fields.next().and_then(|f| f.parse().ok()).unwrap_or(1);
    let col: usize = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let cpos = text
        .split('\n')
        .take(row.saturating_sub(1))
        .map(|l| l.len() + 1)
        .sum::<usize>()
        + col;
    let mut session = Session {
        buf: text,
        cpos,
        attachments: Vec::new(),
        clipboard: Clipboard::null(),
        history: UndoHistory::new(None),
        mode: VimMode::Normal,
        curswant: None,
        vim_state: VimWindowState::default(),
    };
    for key in keys {
        session.press(key);
    }
    let before = &session.buf[..session.cpos.min(session.buf.len())];
    let out_row = before.matches('\n').count() + 1;
    let out_col = before.len() - before.rfind('\n').map_or(0, |i| i + 1);
    // A linewise register holds its lines without a final newline; vim's
    // holds the newline, and keeps an empty line.
    let linewise = session.clipboard.kill_ring.is_linewise();
    let mut register = session.clipboard.kill_ring.current().to_string();
    if linewise {
        register.push('\n');
    }
    let regtype = match (register.is_empty(), linewise) {
        (true, _) => "",
        (false, true) => "V",
        (false, false) => "v",
    };
    let mode = match session.mode {
        VimMode::Insert => "i",
        VimMode::Normal => "n",
        VimMode::Visual => "v",
        VimMode::VisualLine => "V",
    };
    format!(
        "{}\t{out_row}\t{out_col}\t{}\t{mode}\t{regtype}",
        escape(&session.buf),
        escape(&register)
    )
}

#[test]
#[ignore = "driven by the nvim parity script"]
fn dump_parity_cases() {
    let (Ok(cases), Ok(out)) = (
        std::env::var("SMELT_PARITY_CASES"),
        std::env::var("SMELT_PARITY_OUT"),
    ) else {
        return;
    };
    let results = std::fs::read_to_string(cases)
        .expect("cases file")
        .lines()
        .map(|line| std::panic::catch_unwind(|| run_case(line)).unwrap_or_else(|_| "PANIC".into()))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(out, results).expect("write results");
}

/// Outcomes checked against neovim, kept as plain tests so they run in CI
/// without it. Each case is `(text, keys, resulting text)` from the first line.
#[cfg(test)]
mod pinned {
    use super::*;

    fn text_after(text: &str, keys: &str) -> String {
        let line = format!("{}\t{keys}\t1\t0", escape(text));
        let result = run_case(&line);
        unescape(result.split('\t').next().unwrap_or_default())
    }

    #[test]
    fn insert_counts_and_dot_repeat_replay_the_typed_text() {
        for (text, keys, expected) in [
            ("a", "3ix<Esc>", "xxxa"),
            ("a", "3ox<Esc>", "a\nx\nx\nx"),
            ("a", "2Ax<Esc>", "axx"),
            ("a b", "ix<Esc>.", "xxa b"),
            ("one two three", "cwab<Esc>", "ab two three"),
            ("one two three", "cwab<Esc>w.", "ab ab three"),
        ] {
            assert_eq!(text_after(text, keys), expected, "{keys}");
        }
    }

    #[test]
    fn gi_resumes_insert_where_it_last_stopped() {
        assert_eq!(text_after("abc def", "ix<Esc>wgiy<Esc>"), "xyabc def");
    }

    #[test]
    fn open_line_keeps_indent_and_esc_strips_unused_indent() {
        assert_eq!(text_after("  ab", "o<Esc>"), "  ab\n");
        assert_eq!(text_after("  ab", "Ox<Esc>"), "  x\n  ab");
    }

    #[test]
    fn counted_dd_fails_when_lines_run_out() {
        assert_eq!(text_after("a\nb", "j2dd"), "a\nb");
    }

    #[test]
    fn join_onto_an_empty_current_line_adds_no_space() {
        assert_eq!(text_after("\nb", "J"), "b");
    }

    #[test]
    fn replace_with_newline_keeps_the_indent() {
        assert_eq!(text_after("  ab  cd", "4lr<CR>"), "  ab\n  cd");
    }

    /// Word, bracket and quote objects take counts, search forward on the line and extend Visual selections.
    #[test]
    fn word_and_bracket_objects_take_counts_and_search_forward() {
        for (text, keys, expected) in [
            ("one two three", "daw", "two three"),
            ("one two three", "wdaw", "one three"),
            ("one two three", "wwdaw", "one two"),
            ("one two three", "wdiw", "one  three"),
            ("one two three", "d2aw", "three"),
            ("one two three", "d2iw", "two three"),
            ("one two three", "wviwiwd", "one three"),
            ("one two three", "vawawd", "three"),
            ("foo (bar) baz", "di(", "foo () baz"),
            ("foo (bar) baz", "da(", "foo  baz"),
            ("f(a (b) c) g", "fbd2i(", "f() g"),
            ("f(a (b) c) g", "fbd2a(", "f g"),
            ("f(a (b) c) g", "fbdi(", "f(a () c) g"),
            ("f(a (b) c) g", "fbvi(i(d", "f(a () c) g"),
            ("x \"a b\" y", "di\"", "x \"\" y"),
            ("x \"a b\" y", "da\"", "x y"),
            ("x \"a b\" y", "fad2i\"", "x  y"),
            ("x 'a b' y", "ci'z<Esc>", "x 'z' y"),
        ] {
            assert_eq!(text_after(text, keys), expected, "{keys}");
        }
    }

    /// Quote, sentence and paragraph objects and motions.
    #[test]
    fn quote_sentence_and_paragraph_objects() {
        for (text, keys, expected) in [
            ("One. Two. Three.", "das", "Two. Three."),
            ("One. Two. Three.", "wwdas", "One. Three."),
            ("One. Two. Three.", "wdis", " Two. Three."),
            ("One. Two. Three.", "d)", "Two. Three."),
            ("One. Two. Three.", "$d(", "One. Two. ."),
            ("One. Two. Three.", "2d)", "Three."),
            ("One. Two. Three.", "))", "One. Two. Three."),
            ("a\nb\n\nc", "dap", "c"),
            ("a\nb\n\nc", "dip", "\nc"),
            ("a\nb\n\nc\nd", "jjjdap", "a\nb"),
            ("a\nb\n\nc\nd", "d}", "\nc\nd"),
            ("a\nb\n\nc\nd", "2d}", ""),
            ("a\nb\n\nc\nd", "3d}", "a\nb\n\nc\nd"),
        ] {
            assert_eq!(text_after(text, keys), expected, "{keys}");
        }
    }

    /// Word motions on line ends, empty lines and the buffer's last line.
    #[test]
    fn word_motions_stop_where_vim_does() {
        for (text, keys, expected) in [
            ("a\nb\n\nc\nd", "Gd{", "a\nb\nd"),
            ("one two\nthree", "dw", "two\nthree"),
            ("one two\nthree", "wdw", "one \nthree"),
            ("one two\nthree", "wcwx<Esc>", "one x\nthree"),
            ("one two\nthree", "de", " two\nthree"),
            ("one two\nthree", "wde", "one \nthree"),
            ("one two\nthree", "wwdb", "one \nthree"),
            ("a\n\nb", "dw", "\n\nb"),
            ("a\n\nb", "jdw", "a\nb"),
            ("a b\n", "wdw", "a \n"),
            ("a b\n", "wde", "a "),
            ("a b\n", "3e", "a b\n"),
        ] {
            assert_eq!(text_after(text, keys), expected, "{keys}");
        }
    }

    /// `.` after a Visual command selects the same amount of text from the cursor.
    #[test]
    fn visual_commands_repeat_over_the_same_amount_of_text() {
        for (text, keys, expected) in [
            ("a b\n", "3ex", "a b\n"),
            ("one two three", "vld.", "two three"),
            ("one two three", "vlld..", "hree"),
            ("one\ntwo\nthree\nfour", "Vd.", "three\nfour"),
            ("one\ntwo\nthree\nfour", "vjd.", "hree\nfour"),
            ("one two\nthree four", "vjd.", "ree four"),
            ("one two three", "vlU.", "ONe two three"),
            ("one two three", "vlr.w.", "....two three"),
            ("one two three", "vlcab<Esc>w.", "abe abo three"),
            ("one\ntwo\nthree", "VjJ", "one two\nthree"),
            ("one\ntwo\nthree", "VjgJ", "onetwo\nthree"),
            ("one\n  two\nthree", "vjJ", "one two\nthree"),
            ("one\ntwo", "vJ", "one two"),
        ] {
            assert_eq!(text_after(text, keys), expected, "{keys}");
        }
    }

    /// Visual `$` and `l` can take the line break, `p` and `J` work on the selection.
    #[test]
    fn visual_commands_reach_the_line_break() {
        for (text, keys, expected) in [
            ("one\ntwo", "vgJ", "onetwo"),
            ("one two", "v$d", ""),
            ("one\ntwo", "v$d", "two"),
            ("one\ntwo", "vld", "e\ntwo"),
            ("one\ntwo", "v$y$p", "oneone\n\ntwo"),
            ("ab\ncd", "vj$d", ""),
            ("one two", "wv$hd", "one "),
            ("one two three", "ywwvep", "one one  three"),
            ("one two three", "ywwvlp", "one one o three"),
            ("one\ntwo", "yyjvp", "one\n\none\nwo"),
            ("one\ntwo", "yyjVp", "one\none"),
            ("one two", "ywVp", "one "),
            ("one two", "ywwVP", "one "),
            ("one two", "ywvepa<Esc>", "one  two"),
        ] {
            assert_eq!(text_after(text, keys), expected, "{keys}");
        }
    }
}
