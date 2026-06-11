//! Nvim-style status input: a Buffer-backed modal overlay with history and Tab completion.

use crate::app::{search::SearchDirection, CommandAction, TuiApp};

use crate::smelt_edit::layout::Anchor;
use crate::smelt_edit::BufCreateOpts;
use crate::smelt_edit::UiHost;
use crate::smelt_edit::{Constraint, LayoutTree, Overlay, SplitConfig};
use crossterm::event::{KeyCode, KeyEvent};

/// Prefix glyph; cursor and editing clamp past the one-cell prefix so it cannot be deleted.
const PREFIX_LEN: u16 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CmdlineMode {
    #[default]
    Command,
    Search {
        target: crate::smelt_edit::WinId,
        direction: SearchDirection,
    },
}

impl CmdlineMode {
    fn prefix(self) -> &'static str {
        match self {
            Self::Command => ":",
            Self::Search {
                direction: SearchDirection::Forward,
                ..
            } => "/",
            Self::Search {
                direction: SearchDirection::Backward,
                ..
            } => "?",
        }
    }
}

/// Cmdline state persisted across open/close cycles.
#[derive(Default)]
pub(crate) struct CmdlineState {
    pub(crate) mode: CmdlineMode,
    pub(crate) history: Vec<String>,
    pub(crate) search_history: Vec<String>,
    /// Index into `history` while browsing with Up/Down; `None` otherwise.
    pub(crate) history_browse: Option<usize>,
    /// Live input snapshot saved when history browsing begins; restored on Down past the newest entry.
    pub(crate) history_stash: String,
    /// Tab-cycle state: command-name candidates ordered by fuzzy match against the
    /// query at first Tab, plus the current cursor into that list. Dropped on close
    /// or text mutation so the next Tab re-ranks against the new query.
    pub(crate) completer: Option<CmdlineCompleter>,
}

/// Linear tab-cycling completer for the `:` cmdline. Holds the candidate
/// command names ranked best-first and the current selection index.
pub(crate) struct CmdlineCompleter {
    pub(crate) labels: Vec<String>,
    pub(crate) selected: usize,
}

impl TuiApp {
    pub(crate) fn cmdline_is_focused(&self) -> bool {
        self.well_known
            .cmdline
            .is_some_and(|win| self.ui.focus() == Some(win))
    }

    pub(crate) fn open_cmdline(&mut self) {
        self.open_status_input(CmdlineMode::Command);
    }

    pub(crate) fn open_search_cmdline(
        &mut self,
        target: crate::smelt_edit::WinId,
        direction: SearchDirection,
    ) {
        self.open_status_input(CmdlineMode::Search { target, direction });
    }

    fn open_status_input(&mut self, mode: CmdlineMode) {
        if self.well_known.cmdline.is_some() {
            return;
        }
        self.cmdline.mode = mode;
        let prefix = mode.prefix();

        let buf = self.buf_create(BufCreateOpts::default());
        if let Some(b) = self.buf_mut(buf) {
            b.set_all_lines(vec![prefix.to_string()]);
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
            w.set_cursor_col_single_line(PREFIX_LEN);
            w.pin_scroll(0);
        }

        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(1),
            LayoutTree::hbox(vec![(Constraint::Percentage(100), LayoutTree::leaf(win))]),
        )]);
        let _ = self
            .overlay_open(Overlay::new(layout, Anchor::ScreenBottom { above_rows: 0 }).modal(true));

        self.set_focus(win);
        self.well_known.cmdline = Some(win);
        self.cmdline.completer = None;
        self.cmdline_apply_status_bg();
    }

    /// Re-applied after every payload mutation: `Buffer::set_lines` wipes decorations in the replaced range.
    fn cmdline_apply_status_bg(&mut self) {
        let Some(win) = self.well_known.cmdline else {
            return;
        };
        let Some(buf_id) = self.ui.win(win).map(|w| w.buf) else {
            return;
        };
        let bg = self.ui.theme().get("SmeltStatusBg").bg;
        let Some(bg) = bg else { return };
        if let Some(b) = self.ui.buf_mut(buf_id) {
            b.set_decoration(
                0,
                smelt_buffer::buffer::LineDecoration {
                    fill_bg: Some(bg),
                    ..Default::default()
                },
            );
        }
    }

    pub(crate) fn close_cmdline(&mut self) {
        if let Some(win) = self.well_known.cmdline.take() {
            self.close_overlay_leaf(win);
        }
        self.cmdline.completer = None;
        self.cmdline.mode = CmdlineMode::Command;
    }

    fn cmdline_text(&self) -> String {
        let Some(win) = self.well_known.cmdline else {
            return String::new();
        };
        let buf_id = self.ui.win(win).map(|w| w.buf);
        let line = buf_id
            .and_then(|b| self.ui.buf(b))
            .and_then(|b| b.get_line(0).map(|s| s.to_string()))
            .unwrap_or_default();
        let prefix = self.cmdline.mode.prefix();
        line.strip_prefix(prefix).unwrap_or(&line).to_string()
    }

    fn cmdline_set_payload(&mut self, payload: &str, cursor_in_payload: usize) {
        let Some(win) = self.well_known.cmdline else {
            return;
        };
        let prefix = self.cmdline.mode.prefix();
        let new_line = format!("{prefix}{payload}");
        if let Some(buf_id) = self.ui.win(win).map(|w| w.buf) {
            if let Some(b) = self.ui.buf_mut(buf_id) {
                b.set_lines(0, 1, vec![new_line]);
            }
        }
        if let Some(w) = self.ui.win_mut(win) {
            w.set_cursor_col_single_line(PREFIX_LEN + cursor_in_payload as u16);
        }
        self.cmdline_apply_status_bg();
    }

    fn cmdline_cursor_in_payload(&self) -> usize {
        let Some(win) = self.well_known.cmdline else {
            return 0;
        };
        let cur = self
            .ui
            .win(win)
            .map(|w| w.cursor_col())
            .unwrap_or(PREFIX_LEN);
        cur.saturating_sub(PREFIX_LEN) as usize
    }

    /// Handles a keystroke for a focused cmdline; `Some(true)` → quit, `Some(false)` → handled, `None` → unrecognised.
    pub(crate) fn cmdline_handle_key(&mut self, k: KeyEvent) -> Option<bool> {
        use crossterm::event::KeyModifiers as M;
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), M::CONTROL) => {
                if matches!(self.cmdline.mode, CmdlineMode::Search { .. }) {
                    self.clear_search();
                    self.close_cmdline();
                } else {
                    self.close_cmdline();
                }
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
        let (new, cur) = super::cmdline_edit::insert_char(
            &self.cmdline_text(),
            self.cmdline_cursor_in_payload(),
            c,
        );
        self.cmdline_set_payload(&new, cur);
        self.cmdline.completer = None;
    }

    fn cmdline_backspace(&mut self) -> Option<bool> {
        match super::cmdline_edit::backspace(&self.cmdline_text(), self.cmdline_cursor_in_payload())
        {
            None => {
                self.close_cmdline();
                Some(false)
            }
            Some((new, cur)) => {
                self.cmdline_set_payload(&new, cur);
                self.cmdline.completer = None;
                Some(false)
            }
        }
    }

    fn cmdline_delete_forward(&mut self) {
        let (new, cur) = super::cmdline_edit::delete_forward(
            &self.cmdline_text(),
            self.cmdline_cursor_in_payload(),
        );
        self.cmdline_set_payload(&new, cur);
        self.cmdline.completer = None;
    }

    fn cmdline_delete_word_back(&mut self) -> Option<bool> {
        match super::cmdline_edit::delete_word_back(
            &self.cmdline_text(),
            self.cmdline_cursor_in_payload(),
        ) {
            None => {
                self.close_cmdline();
                Some(false)
            }
            Some((new, cur)) => {
                self.cmdline_set_payload(&new, cur);
                self.cmdline.completer = None;
                Some(false)
            }
        }
    }

    fn cmdline_clear(&mut self) {
        self.cmdline_set_payload("", 0);
        self.cmdline.completer = None;
    }

    fn cmdline_move(&mut self, delta: i32) {
        let count = self.cmdline_text().chars().count();
        let new = super::cmdline_edit::clamp_move(count, self.cmdline_cursor_in_payload(), delta);
        if let Some(win) = self.well_known.cmdline {
            if let Some(w) = self.ui.win_mut(win) {
                w.set_cursor_col_single_line(PREFIX_LEN + new as u16);
            }
        }
    }

    fn cmdline_move_home(&mut self) {
        if let Some(win) = self.well_known.cmdline {
            if let Some(w) = self.ui.win_mut(win) {
                w.set_cursor_col_single_line(PREFIX_LEN);
            }
        }
    }

    fn cmdline_move_end(&mut self) {
        let count = self.cmdline_text().chars().count() as u16;
        if let Some(win) = self.well_known.cmdline {
            if let Some(w) = self.ui.win_mut(win) {
                w.set_cursor_col_single_line(PREFIX_LEN + count);
            }
        }
    }

    fn active_history(&self) -> &[String] {
        match self.cmdline.mode {
            CmdlineMode::Command => &self.cmdline.history,
            CmdlineMode::Search { .. } => &self.cmdline.search_history,
        }
    }

    fn cmdline_history_up(&mut self) {
        let current = self.cmdline_text();
        let history = self.active_history().to_vec();
        let owned =
            super::cmdline_edit::history_up(&history, self.cmdline.history_browse).into_owned();
        self.apply_history_step(owned, current);
    }

    fn cmdline_history_down(&mut self) {
        let stash = self.cmdline.history_stash.clone();
        let history = self.active_history().to_vec();
        let owned =
            super::cmdline_edit::history_down(&history, self.cmdline.history_browse, &stash)
                .into_owned();
        self.apply_history_step(owned, String::new());
    }

    /// Apply the result of a history-navigation step (Up/Down) back to live
    /// cmdline state. `current_for_stash` is the live payload that should
    /// be saved when `stash_current` is true (Up from a fresh state).
    fn apply_history_step(
        &mut self,
        step: super::cmdline_edit::HistoryStepOwned,
        current_for_stash: String,
    ) {
        use super::cmdline_edit::HistoryStepOwned;
        match step {
            HistoryStepOwned::NoHistory | HistoryStepOwned::Boundary => {}
            HistoryStepOwned::Browse {
                idx,
                entry,
                stash_current,
            } => {
                if stash_current {
                    self.cmdline.history_stash = current_for_stash;
                }
                self.cmdline.history_browse = Some(idx);
                let cursor = entry.chars().count();
                self.cmdline_set_payload(&entry, cursor);
                self.cmdline.completer = None;
            }
            HistoryStepOwned::Restore { stash } => {
                self.cmdline.history_browse = None;
                self.cmdline.history_stash = String::new();
                let cursor = stash.chars().count();
                self.cmdline_set_payload(&stash, cursor);
                self.cmdline.completer = None;
            }
        }
    }

    fn cmdline_submit(&mut self) -> bool {
        let line = self.cmdline_text();
        let mode = self.cmdline.mode;
        match mode {
            CmdlineMode::Command => {
                let last = self.cmdline.history.last().cloned();
                if !line.is_empty() && last.as_deref() != Some(line.as_str()) {
                    self.cmdline.history.push(line.clone());
                }
                self.close_cmdline();
                if line.is_empty() {
                    return false;
                }
                let action = crate::commands::run_command(self, &format!(":{line}"));
                match action {
                    CommandAction::Exec(handle) => {
                        self.exec = Some(handle);
                        false
                    }
                    CommandAction::Continue => self.pending_quit,
                }
            }
            CmdlineMode::Search { target, direction } => {
                let last = self.cmdline.search_history.last().cloned();
                if !line.is_empty() && last.as_deref() != Some(line.as_str()) {
                    self.cmdline.search_history.push(line.clone());
                }
                self.close_cmdline();
                self.submit_search(target, direction, line);
                false
            }
        }
    }

    fn cmdline_cycle_completer(&mut self, next: bool) {
        if !matches!(self.cmdline.mode, CmdlineMode::Command) {
            return;
        }
        if self.cmdline.completer.is_none() {
            let typed = self.cmdline_text();
            let labels = self.lua.command_names();
            let ranked: Vec<String> = if typed.is_empty() {
                labels
            } else {
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                smelt_core::fuzzy::fuzzy_rank(&typed, &refs)
                    .into_iter()
                    .map(|i| labels[i].clone())
                    .collect()
            };
            if ranked.is_empty() {
                return;
            }
            self.cmdline.completer = Some(CmdlineCompleter {
                labels: ranked,
                selected: 0,
            });
        } else if let Some(comp) = self.cmdline.completer.as_mut() {
            let n = comp.labels.len();
            if n == 0 {
                return;
            }
            comp.selected = if next {
                (comp.selected + n - 1) % n
            } else {
                (comp.selected + 1) % n
            };
        }
        let payload = self
            .cmdline
            .completer
            .as_ref()
            .and_then(|c| c.labels.get(c.selected).cloned());
        if let Some(label) = payload {
            let cursor = label.chars().count();
            self.cmdline_set_payload(&label, cursor);
        }
    }
}
