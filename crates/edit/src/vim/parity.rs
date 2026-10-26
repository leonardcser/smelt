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
//! Result line: `text<TAB>line<TAB>col<TAB>register<TAB>mode`.

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
    let mut register = session.clipboard.kill_ring.current().to_string();
    if session.clipboard.kill_ring.is_linewise() {
        if register.ends_with('\n') {
            register.pop();
        }
        register.push('\n');
    }
    let mode = match session.mode {
        VimMode::Insert => "i",
        VimMode::Normal => "n",
        VimMode::Visual => "v",
        VimMode::VisualLine => "V",
    };
    format!(
        "{}\t{out_row}\t{out_col}\t{}\t{mode}",
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
