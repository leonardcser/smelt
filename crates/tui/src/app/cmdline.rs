//! Nvim-style `:`-command line as a Buffer-backed input leaf in a
//! modal overlay anchored above the status bar.
//!
//! All key handling lives in `cmdline_handle_key`, called by
//! `events.rs` before compositor dispatch. The overlay leaf carries no
//! Window-level keymap recipe; cmdline state mutates through the TuiApp
//! directly, including text editing, history navigation, completer
//! cycling, command execution, and dismissal.

use crate::app::{CommandAction, TuiApp};

use crate::ui::buffer::BufCreateOpts;
use crate::ui::layout::Anchor;
use crate::ui::UiHost;
use crate::ui::{Constraint, LayoutTree, Overlay, SplitConfig};
use crossterm::event::{KeyCode, KeyEvent};

/// Visible prefix glyph rendered as the first cell of the cmdline
/// buffer. Cursor positions and editing operations clamp to columns
/// `>= PREFIX_LEN` so the prefix can't be deleted.
const PREFIX: &str = ":";
const PREFIX_LEN: u16 = 1;

impl TuiApp {
    pub(crate) fn cmdline_is_focused(&self) -> bool {
        self.well_known
            .cmdline
            .is_some_and(|win| self.ui.focus() == Some(win))
    }

    pub(crate) fn open_cmdline(&mut self) {
        if self.well_known.cmdline.is_some() {
            return;
        }

        let buf = self.buf_create(BufCreateOpts::default());
        if let Some(b) = self.buf_mut(buf) {
            b.set_all_lines(vec![PREFIX.to_string()]);
        }

        let Some(win) = self.win_open_split(
            buf,
            SplitConfig {
                region: "cmdline_overlay".into(),
                gutters: Default::default(),
            },
        ) else {
            return;
        };
        if let Some(w) = self.win_mut(win) {
            w.cursor_line = 0;
            w.cursor_col = PREFIX_LEN;
            w.scroll_top = 0;
        }

        // Single-row leaf at the bottom of the screen, one row above
        // the status bar. Inner Hbox uses `Percentage(100)` so the
        // overlay's natural width follows the terminal each frame.
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(1),
            LayoutTree::hbox(vec![(Constraint::Percentage(100), LayoutTree::leaf(win))]),
        )]);
        let _ = self
            .overlay_open(Overlay::new(layout, Anchor::ScreenBottom { above_rows: 1 }).modal(true));

        self.set_focus(win);
        self.well_known.cmdline = Some(win);
        self.cmdline_completer = None;
    }

    fn close_cmdline(&mut self) {
        if let Some(win) = self.well_known.cmdline.take() {
            self.close_overlay_leaf(win);
        }
        self.cmdline_completer = None;
    }

    /// Read the cmdline's typed text (without the leading prefix).
    fn cmdline_text(&self) -> String {
        let Some(win) = self.well_known.cmdline else {
            return String::new();
        };
        let buf_id = self.ui.win(win).map(|w| w.buf);
        let line = buf_id
            .and_then(|b| self.ui.buf(b))
            .and_then(|b| b.get_line(0).map(|s| s.to_string()))
            .unwrap_or_default();
        line.strip_prefix(PREFIX).unwrap_or(&line).to_string()
    }

    /// Replace the cmdline buffer with `prefix + payload` and place the
    /// cursor at column `prefix_len + cursor_in_payload`.
    fn cmdline_set_payload(&mut self, payload: &str, cursor_in_payload: usize) {
        let Some(win) = self.well_known.cmdline else {
            return;
        };
        let new_line = format!("{PREFIX}{payload}");
        if let Some(buf_id) = self.ui.win(win).map(|w| w.buf) {
            if let Some(b) = self.ui.buf_mut(buf_id) {
                b.set_lines(0, 1, vec![new_line]);
            }
        }
        if let Some(w) = self.ui.win_mut(win) {
            w.cursor_col = PREFIX_LEN + cursor_in_payload as u16;
        }
    }

    /// Cursor position within the payload (always `>= 0`). Returns 0
    /// when the cmdline isn't open.
    fn cmdline_cursor_in_payload(&self) -> usize {
        let Some(win) = self.well_known.cmdline else {
            return 0;
        };
        let cur = self.ui.win(win).map(|w| w.cursor_col).unwrap_or(PREFIX_LEN);
        cur.saturating_sub(PREFIX_LEN) as usize
    }

    /// Single dispatcher for every keystroke routed at a focused
    /// cmdline. Returns `Some(true)` when the command resolved to
    /// `Quit` (so the main loop tears down), `Some(false)` when we
    /// handled but stay alive, and `None` only when the key isn't
    /// ours. The overlay leaf carries no callbacks so a `None` return
    /// silently swallows the key — same as the legacy widget's
    /// `Status::Ignored` path.
    pub(crate) fn cmdline_handle_key(&mut self, k: KeyEvent) -> Option<bool> {
        use crossterm::event::KeyModifiers as M;
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), M::CONTROL) => {
                self.close_cmdline();
                Some(false)
            }
            (KeyCode::Enter, _) => Some(self.cmdline_submit()),
            (KeyCode::Backspace, _) => self.cmdline_backspace(),
            (KeyCode::Delete, _) => {
                self.cmdline_delete_forward();
                Some(false)
            }
            (KeyCode::Left, _) => {
                self.cmdline_move(-1);
                Some(false)
            }
            (KeyCode::Right, _) => {
                self.cmdline_move(1);
                Some(false)
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), M::CONTROL) => {
                self.cmdline_move_home();
                Some(false)
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), M::CONTROL) => {
                self.cmdline_move_end();
                Some(false)
            }
            (KeyCode::Up, _) => {
                self.cmdline_history_up();
                Some(false)
            }
            (KeyCode::Down, _) => {
                self.cmdline_history_down();
                Some(false)
            }
            (KeyCode::Char('w'), M::CONTROL) => self.cmdline_delete_word_back(),
            (KeyCode::Char('u'), M::CONTROL) => {
                self.cmdline_clear();
                Some(false)
            }
            (KeyCode::Tab, _)
            | (KeyCode::Char('j'), M::CONTROL)
            | (KeyCode::Char('n'), M::CONTROL) => {
                self.cmdline_cycle_completer(true);
                Some(false)
            }
            (KeyCode::BackTab, _)
            | (KeyCode::Char('k'), M::CONTROL)
            | (KeyCode::Char('p'), M::CONTROL) => {
                self.cmdline_cycle_completer(false);
                Some(false)
            }
            (KeyCode::Char(c), mods) if mods.is_empty() || mods == M::SHIFT => {
                self.cmdline_insert_char(c);
                Some(false)
            }
            _ => None,
        }
    }

    fn cmdline_insert_char(&mut self, c: char) {
        let payload = self.cmdline_text();
        let cur = self
            .cmdline_cursor_in_payload()
            .min(payload.chars().count());
        let chars: Vec<char> = payload.chars().collect();
        let new: String = chars[..cur]
            .iter()
            .copied()
            .chain(std::iter::once(c))
            .chain(chars[cur..].iter().copied())
            .collect();
        self.cmdline_set_payload(&new, cur + 1);
        self.cmdline_completer = None;
    }

    fn cmdline_backspace(&mut self) -> Option<bool> {
        let payload = self.cmdline_text();
        let cur = self.cmdline_cursor_in_payload();
        if payload.is_empty() {
            // Backspace on the bare prefix dismisses (mirrors the
            // legacy widget's "empty buffer" exit).
            self.close_cmdline();
            return Some(false);
        }
        if cur == 0 {
            // Cursor is at the prefix boundary but text exists ahead;
            // nothing to delete to the left.
            return Some(false);
        }
        let chars: Vec<char> = payload.chars().collect();
        let new: String = chars[..cur - 1]
            .iter()
            .copied()
            .chain(chars[cur..].iter().copied())
            .collect();
        self.cmdline_set_payload(&new, cur - 1);
        self.cmdline_completer = None;
        Some(false)
    }

    fn cmdline_delete_forward(&mut self) {
        let payload = self.cmdline_text();
        let cur = self.cmdline_cursor_in_payload();
        let count = payload.chars().count();
        if cur >= count {
            return;
        }
        let chars: Vec<char> = payload.chars().collect();
        let new: String = chars[..cur]
            .iter()
            .copied()
            .chain(chars[cur + 1..].iter().copied())
            .collect();
        self.cmdline_set_payload(&new, cur);
        self.cmdline_completer = None;
    }

    fn cmdline_delete_word_back(&mut self) -> Option<bool> {
        let payload = self.cmdline_text();
        if payload.is_empty() {
            // Same exit semantics as the legacy widget: Ctrl+W on a
            // blank cmdline closes.
            self.close_cmdline();
            return Some(false);
        }
        let cur = self.cmdline_cursor_in_payload();
        let chars: Vec<char> = payload.chars().collect();
        let split = cur.min(chars.len());
        let prefix: String = chars[..split].iter().collect();
        let trimmed_end = prefix.trim_end();
        // Walk backwards from the cursor to the previous word
        // boundary (alphanumeric / `_`). Land just past the
        // boundary char so consecutive Ctrl+W deletes successive
        // words including their trailing whitespace.
        let new_cursor = match trimmed_end.rfind(|c: char| !c.is_alphanumeric() && c != '_') {
            Some(boundary) => {
                let boundary_char_len = trimmed_end[boundary..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                trimmed_end[..boundary + boundary_char_len].chars().count()
            }
            None => 0,
        };
        let head: String = chars[..new_cursor].iter().collect();
        let tail: String = chars[split..].iter().collect();
        let new = format!("{head}{tail}");
        self.cmdline_set_payload(&new, new_cursor);
        self.cmdline_completer = None;
        Some(false)
    }

    fn cmdline_clear(&mut self) {
        self.cmdline_set_payload("", 0);
        self.cmdline_completer = None;
    }

    fn cmdline_move(&mut self, delta: i32) {
        let payload = self.cmdline_text();
        let count = payload.chars().count() as i32;
        let cur = self.cmdline_cursor_in_payload() as i32;
        let new = (cur + delta).clamp(0, count) as usize;
        if let Some(win) = self.well_known.cmdline {
            if let Some(w) = self.ui.win_mut(win) {
                w.cursor_col = PREFIX_LEN + new as u16;
            }
        }
    }

    fn cmdline_move_home(&mut self) {
        if let Some(win) = self.well_known.cmdline {
            if let Some(w) = self.ui.win_mut(win) {
                w.cursor_col = PREFIX_LEN;
            }
        }
    }

    fn cmdline_move_end(&mut self) {
        let count = self.cmdline_text().chars().count() as u16;
        if let Some(win) = self.well_known.cmdline {
            if let Some(w) = self.ui.win_mut(win) {
                w.cursor_col = PREFIX_LEN + count;
            }
        }
    }

    fn cmdline_history_up(&mut self) {
        if self.cmdline_history.is_empty() {
            return;
        }
        let next_idx = match self.cmdline_history_browse {
            None => self.cmdline_history.len().saturating_sub(1),
            Some(0) => 0,
            Some(i) => i.saturating_sub(1),
        };
        if self.cmdline_history_browse.is_none() {
            self.cmdline_history_stash = self.cmdline_text();
        }
        self.cmdline_history_browse = Some(next_idx);
        let entry = self.cmdline_history[next_idx].clone();
        let cursor = entry.chars().count();
        self.cmdline_set_payload(&entry, cursor);
        self.cmdline_completer = None;
    }

    fn cmdline_history_down(&mut self) {
        let Some(idx) = self.cmdline_history_browse else {
            return;
        };
        if idx + 1 >= self.cmdline_history.len() {
            self.cmdline_history_browse = None;
            let stash = std::mem::take(&mut self.cmdline_history_stash);
            let cursor = stash.chars().count();
            self.cmdline_set_payload(&stash, cursor);
        } else {
            let next_idx = idx + 1;
            self.cmdline_history_browse = Some(next_idx);
            let entry = self.cmdline_history[next_idx].clone();
            let cursor = entry.chars().count();
            self.cmdline_set_payload(&entry, cursor);
        }
        self.cmdline_completer = None;
    }

    /// Persist the current line into history (if non-empty and not a
    /// duplicate of the most-recent entry), close the overlay, then
    /// dispatch the command. Returns `true` when `pending_quit` is set
    /// (e.g. the line was `q` / `quit` / `exit`) so the caller can break
    /// the main loop on the next tick.
    fn cmdline_submit(&mut self) -> bool {
        let line = self.cmdline_text();
        let last = self.cmdline_history.last().cloned();
        if !line.is_empty() && last.as_deref() != Some(line.as_str()) {
            self.cmdline_history.push(line.clone());
        }
        self.close_cmdline();
        if line.is_empty() {
            return false;
        }
        let action = crate::commands::run_command(self, &format!(":{line}"));
        match action {
            CommandAction::Exec(rx, kill) => {
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
                false
            }
            CommandAction::Continue => self.pending_quit,
        }
    }

    /// Lazily build the shared completer from the static command list +
    /// Lua-registered names, then advance / rewind the selection and
    /// apply the selected label as the cmdline text.
    fn cmdline_cycle_completer(&mut self, next: bool) {
        use crate::completer::{Completer, CompletionItem};
        if self.cmdline_completer.is_none() {
            let typed = self.cmdline_text();
            let mut comp = Completer::commands(0);
            let lua_cmds = self.lua.command_names();
            if !lua_cmds.is_empty() {
                let mut items: Vec<CompletionItem> = comp.all_items().to_vec();
                for name in lua_cmds {
                    if !items.iter().any(|i| i.label == name) {
                        items.push(CompletionItem {
                            label: name,
                            description: Some("(lua)".into()),
                            ..Default::default()
                        });
                    }
                }
                comp.refresh_items(items);
            }
            comp.update_query(typed);
            self.cmdline_completer = Some(comp);
        } else if let Some(ref mut comp) = self.cmdline_completer {
            // Reversed picker style: `next` advances upward toward the
            // best match near the prompt.
            if next {
                comp.move_up();
            } else {
                comp.move_down();
            }
        }
        if let Some(ref comp) = self.cmdline_completer {
            if let Some(item) = comp.selected_item() {
                let label = item.label.clone();
                let cursor = label.chars().count();
                self.cmdline_set_payload(&label, cursor);
            }
        }
    }
}
