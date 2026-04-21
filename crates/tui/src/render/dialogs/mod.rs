pub(crate) mod confirm;
mod question;

pub use confirm::ConfirmDialog;
pub use question::{parse_questions, Question, QuestionDialog, QuestionOption};

use crate::app::AgentToolEntry;
use std::sync::{Arc, Mutex};

/// Snapshot of a tracked agent's state, published by the main loop
/// and consumed by the agents dialog.
#[derive(Clone)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub prompt: Arc<String>,
    pub tool_calls: Vec<AgentToolEntry>,
    pub context_tokens: Option<u32>,
    pub cost_usd: f64,
}

/// Shared, live-updating list of agent snapshots.
pub type SharedSnapshots = Arc<Mutex<Vec<AgentSnapshot>>>;

use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::{cursor, QueueableCommand};

use super::{draw_soft_cursor, wrap_line, ConfirmChoice, RenderOut};

pub enum DialogResult {
    Dismissed,
    Confirm {
        choice: ConfirmChoice,
        message: Option<String>,
        tool_name: String,
        request_id: u64,
    },
    Question {
        answer: Option<String>,
        request_id: u64,
    },
}

pub trait Dialog {
    /// Whether the agent is blocked on a reply for this dialog.
    fn blocks_agent(&self) -> bool {
        false
    }
    fn height(&self) -> u16;
    fn mark_dirty(&mut self);
    /// Render the dialog. `granted_rows` is the exact row budget from
    /// draw_frame — the dialog must not exceed `start_row + granted_rows`.
    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16);
    fn handle_resize(&mut self);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult>;

    /// Whether the layout engine should apply a dynamic height cap
    /// (`max(h/2, natural_space)`) to limit scroll-up.  List-based
    /// dialogs return true; confirm/question dialogs return false.
    fn constrain_height(&self) -> bool {
        false
    }

    /// Seed the dialog's kill ring from the main input's kill ring.
    fn set_kill_ring(&mut self, _contents: String) {}
    /// Retrieve the dialog's kill ring so the main input can sync it back.
    fn kill_ring(&self) -> Option<&str> {
        None
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

pub(crate) struct TextArea {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
}

impl TextArea {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn visual_row_count(&self, wrap_w: usize) -> u16 {
        self.lines
            .iter()
            .map(|l| wrap_line(l, wrap_w).len() as u16)
            .sum()
    }

    pub fn wrap(&self, wrap_w: usize) -> (Vec<String>, (usize, usize)) {
        let mut visual = Vec::new();
        let mut cursor = (0, 0);

        for (li, line) in self.lines.iter().enumerate() {
            let vis_start = visual.len();
            let chunks = wrap_line(line, wrap_w);
            visual.extend(chunks);

            if li == self.row {
                let char_count = line.chars().count();
                let col = self.col.min(char_count);
                if char_count == 0 || wrap_w == 0 {
                    cursor = (vis_start, col);
                } else {
                    let vis_offset = col / wrap_w;
                    let vis_col = col % wrap_w;
                    let num_vis = visual.len() - vis_start;
                    if vis_offset >= num_vis {
                        cursor = (
                            vis_start + num_vis - 1,
                            visual[vis_start + num_vis - 1].chars().count(),
                        );
                    } else {
                        cursor = (vis_start + vis_offset, vis_col);
                    }
                }
            }
        }

        (visual, cursor)
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }

    pub fn insert_char(&mut self, c: char) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].insert(byte, c);
        self.col += 1;
    }

    pub fn insert_newline(&mut self) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        let rest = self.lines[self.row][byte..].to_string();
        self.lines[self.row].truncate(byte);
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, rest);
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            let byte = char_to_byte(&self.lines[self.row], self.col);
            self.lines[self.row].remove(byte);
        } else if self.row > 0 {
            let removed = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&removed);
        }
    }

    pub fn delete_word_backward(&mut self) {
        if self.col == 0 {
            return;
        }
        let line = &self.lines[self.row];
        let byte_pos = char_to_byte(line, self.col);
        let target = crate::text_utils::word_backward_pos(
            line,
            byte_pos,
            crate::text_utils::CharClass::Word,
        );
        let target_col = line[..target].chars().count();
        let end_byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].drain(target..end_byte);
        self.col = target_col;
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
        }
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    pub fn move_word_forward(&mut self) {
        let line = &self.lines[self.row];
        let len = line.chars().count();
        if self.col >= len {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.col = 0;
            }
            return;
        }
        let byte = char_to_byte(line, self.col);
        let target =
            crate::text_utils::word_forward_pos(line, byte, crate::text_utils::CharClass::Word);
        self.col = line[..target].chars().count();
    }

    pub fn move_word_backward(&mut self) {
        if self.col == 0 {
            if self.row > 0 {
                self.row -= 1;
                self.col = self.lines[self.row].chars().count();
            }
            return;
        }
        let line = &self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        let target =
            crate::text_utils::word_backward_pos(line, byte, crate::text_utils::CharClass::Word);
        self.col = line[..target].chars().count();
    }

    pub fn delete_char_forward(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            self.lines[self.row].remove(byte);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn delete_word_forward(&mut self) {
        let line = &self.lines[self.row];
        let len = line.chars().count();
        if self.col >= len {
            return;
        }
        let byte = char_to_byte(line, self.col);
        let target =
            crate::text_utils::word_forward_pos(line, byte, crate::text_utils::CharClass::Word);
        self.lines[self.row].drain(byte..target);
    }

    pub fn kill_to_end_of_line(&mut self, kill_ring: &mut String) {
        let line = &self.lines[self.row];
        let byte = char_to_byte(line, self.col);
        *kill_ring = line[byte..].to_string();
        self.lines[self.row].truncate(byte);
    }

    pub fn kill_to_start_of_line(&mut self, kill_ring: &mut String) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        *kill_ring = self.lines[self.row][..byte].to_string();
        self.lines[self.row].drain(..byte);
        self.col = 0;
    }

    pub fn delete_to_start_of_line(&mut self) {
        let byte = char_to_byte(&self.lines[self.row], self.col);
        self.lines[self.row].drain(..byte);
        self.col = 0;
    }

    pub fn yank(&mut self, kill_ring: &str) {
        if !kill_ring.is_empty() {
            let byte = char_to_byte(&self.lines[self.row], self.col);
            self.lines[self.row].insert_str(byte, kill_ring);
            self.col += kill_ring.chars().count();
        }
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        // Note: kill/yank need external kill_ring; handled via handle_key_with_kill_ring.
        match (code, modifiers) {
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => self.insert_char(c),
            (KeyCode::Enter, _) => self.insert_newline(),
            // Ctrl+A: start of line
            (KeyCode::Char('a'), m) if m.contains(KeyModifiers::CONTROL) => self.move_home(),
            // Ctrl+E: end of line
            (KeyCode::Char('e'), m) if m.contains(KeyModifiers::CONTROL) => self.move_end(),
            // Ctrl+F: char forward
            (KeyCode::Char('f'), m) if m.contains(KeyModifiers::CONTROL) => self.move_right(),
            // Ctrl+B: char backward
            (KeyCode::Char('b'), m) if m.contains(KeyModifiers::CONTROL) => self.move_left(),
            // Alt+F: word forward
            (KeyCode::Char('f'), m) if m.contains(KeyModifiers::ALT) => self.move_word_forward(),
            // Alt+B: word backward
            (KeyCode::Char('b'), m) if m.contains(KeyModifiers::ALT) => self.move_word_backward(),
            // Ctrl+D: delete char forward
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.delete_char_forward()
            }
            // Alt+D: delete word forward
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::ALT) => self.delete_word_forward(),
            // Ctrl+W: delete word backward
            (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.delete_word_backward()
            }
            (KeyCode::Backspace, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
            {
                self.delete_word_backward()
            }
            // Cmd+Backspace: delete to start of line
            (KeyCode::Backspace, m) if m.contains(KeyModifiers::SUPER) => {
                self.delete_to_start_of_line()
            }
            (KeyCode::Backspace, _) => self.backspace(),
            // Delete (forward delete key)
            (KeyCode::Delete, m) if m.contains(KeyModifiers::ALT) => self.delete_word_forward(),
            (KeyCode::Delete, _) => self.delete_char_forward(),
            // Alt+Left: word backward
            (KeyCode::Left, m) if m.contains(KeyModifiers::ALT) => self.move_word_backward(),
            // Cmd+Left: start of line
            (KeyCode::Left, m) if m.contains(KeyModifiers::SUPER) => self.move_home(),
            (KeyCode::Left, _) => self.move_left(),
            // Alt+Right: word forward
            (KeyCode::Right, m) if m.contains(KeyModifiers::ALT) => self.move_word_forward(),
            // Cmd+Right: end of line
            (KeyCode::Right, m) if m.contains(KeyModifiers::SUPER) => self.move_end(),
            (KeyCode::Right, _) => self.move_right(),
            // Cmd+Up: start of buffer
            (KeyCode::Up, m) if m.contains(KeyModifiers::SUPER) => {
                self.row = 0;
                self.col = 0;
            }
            (KeyCode::Up, _) => self.move_up(),
            // Cmd+Down: end of buffer
            (KeyCode::Down, m) if m.contains(KeyModifiers::SUPER) => {
                self.row = self.lines.len() - 1;
                self.col = self.lines[self.row].chars().count();
            }
            (KeyCode::Down, _) => self.move_down(),
            (KeyCode::Home, _) => self.move_home(),
            (KeyCode::End, _) => self.move_end(),
            _ => return false,
        }
        true
    }

    /// Like `handle_key` but with kill ring support for Ctrl+K/U/Y.
    pub fn handle_key_with_kill_ring(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        kill_ring: &mut String,
    ) -> bool {
        match (code, modifiers) {
            (KeyCode::Char('k'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.kill_to_end_of_line(kill_ring);
                true
            }
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.kill_to_start_of_line(kill_ring);
                true
            }
            (KeyCode::Char('y'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.yank(kill_ring);
                true
            }
            _ => self.handle_key(code, modifiers),
        }
    }
}

pub(crate) fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

pub(crate) fn render_inline_textarea(
    out: &mut RenderOut,
    ta: &TextArea,
    editing: bool,
    text_col: u16,
    wrap_w: usize,
    mut row: u16,
) -> (u16, Option<(u16, u16)>) {
    let (vis_lines, vis_cursor) = ta.wrap(wrap_w);
    let pad: String = " ".repeat(text_col as usize);
    let mut cursor_pos = None;
    for (vi, vl) in vis_lines.iter().enumerate() {
        if vi == 0 {
            out.print(", ");
        } else {
            out.print(&pad);
        }
        out.print(vl);
        if editing && vi == vis_cursor.0 {
            cursor_pos = Some((text_col + vis_cursor.1 as u16, row));
        }
        out.newline();
        row += 1;
    }
    (row, cursor_pos)
}

pub(crate) fn begin_dialog_draw(out: &mut RenderOut, start_row: u16) {
    // Reset styling to a clean state before the dialog paints.
    // Use the tracked reset (not force) to avoid emitting unnecessary
    // SGR codes that can flash on screen.
    out.reset_style();
    let _ = out.queue(cursor::MoveTo(0, start_row));
    out.row = Some(start_row);
}

pub(crate) fn finish_dialog_frame(
    out: &mut RenderOut,
    cursor_pos: Option<(u16, u16)>,
    editing: bool,
) {
    if editing {
        if let Some((col, r)) = cursor_pos {
            draw_soft_cursor(out, col, r, " ");
        }
    }
}
