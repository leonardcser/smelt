//! Nvim-style status input: a Buffer-backed modal overlay with history and Tab completion.

use crate::app::{search::SearchDirection, CommandAction, TuiApp};

use crate::smelt_edit::layout::Anchor;
use crate::smelt_edit::BufCreateOpts;
use crate::smelt_edit::UiHost;
use crate::smelt_edit::{Constraint, LayoutTree, Overlay, SplitConfig, WinId};
use crossterm::event::{Event, KeyCode, KeyEvent};

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
    /// Visible command-name completion opened explicitly by Tab. Dropped on close
    /// or text mutation so the next Tab re-ranks against the new query.
    pub(crate) completer: Option<CmdlineCompleter>,
}

/// Visible completer for the `:` cmdline. Holds ranked command candidates,
/// the current logical selection, and the picker leaf that renders the list.
pub(crate) struct CmdlineCompleter {
    pub(crate) items: Vec<CmdlineCompletionItem>,
    pub(crate) selected: usize,
    pub(crate) picker: Option<WinId>,
}

#[derive(Clone, Debug)]
pub(crate) struct CmdlineCompletionItem {
    pub(crate) label: String,
    pub(crate) description: Option<String>,
}

enum CmdlineInputAction {
    Cancel,
    Submit,
    CloseIfEmpty,
    HistoryPrevious,
    HistoryNext,
    CompleteOpenOrAccept,
    CompleteNext,
    CompletePrevious,
    Edit(crate::line_input::EditCommand),
}

impl CmdlineInputAction {
    fn from_key(mode: CmdlineMode, key: KeyEvent) -> Option<Self> {
        use crossterm::event::KeyModifiers as M;
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), M::CONTROL) => Some(Self::Cancel),
            (KeyCode::Enter, _) => Some(Self::Submit),
            (KeyCode::Backspace, _) | (KeyCode::Char('w'), M::CONTROL) => Some(Self::CloseIfEmpty),
            (KeyCode::Up, _) => Some(Self::HistoryPrevious),
            (KeyCode::Down, _) => Some(Self::HistoryNext),
            (KeyCode::Tab, _) => Some(Self::CompleteOpenOrAccept),
            (KeyCode::Char('j'), M::CONTROL) | (KeyCode::Char('n'), M::CONTROL) => {
                Some(Self::CompleteNext)
            }
            (KeyCode::BackTab, _) | (KeyCode::Char('p'), M::CONTROL) => {
                Some(Self::CompletePrevious)
            }
            (KeyCode::Char('k'), M::CONTROL) if matches!(mode, CmdlineMode::Command) => {
                Some(Self::CompletePrevious)
            }
            _ => crate::line_input::command_for_key(key).map(Self::Edit),
        }
    }
}

fn cmdline_picker_item(item: &CmdlineCompletionItem) -> crate::picker::PickerItem {
    let mut row = crate::picker::PickerItem::new(item.label.clone());
    if let Some(desc) = item.description.as_deref() {
        row = row.with_description(desc.to_string());
    }
    row
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
        self.cmdline_close_completer();
        if let Some(win) = self.well_known.cmdline.take() {
            self.close_overlay_leaf(win);
        }
        self.cmdline.mode = CmdlineMode::Command;
    }

    fn cmdline_close_completer(&mut self) {
        if let Some(mut completer) = self.cmdline.completer.take() {
            if let Some(win) = completer.picker.take() {
                self.close_overlay_leaf(win);
            }
        }
    }

    fn cmdline_dismiss_completer(&mut self) {
        self.cmdline_close_completer();
    }

    fn cmdline_completer_open(&self) -> bool {
        self.cmdline
            .completer
            .as_ref()
            .and_then(|c| c.picker)
            .is_some()
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
                b.set_lines(0, 1, vec![new_line.clone()]);
            }
        }
        if let Some(w) = self.ui.win_mut(win) {
            w.set_cursor_byte_single_line(&new_line, prefix.len() + cursor_in_payload);
        }
        self.cmdline_apply_status_bg();
    }

    fn cmdline_edit(&self) -> crate::line_input::LineEdit {
        let payload = self.cmdline_text();
        let prefix_len = self.cmdline.mode.prefix().len();
        let (cursor, anchor) = self
            .well_known
            .cmdline
            .and_then(|win| self.ui.win(win))
            .map(|w| {
                (
                    w.cpos().saturating_sub(prefix_len),
                    w.selection_anchor().map(|a| a.saturating_sub(prefix_len)),
                )
            })
            .unwrap_or((0, None));
        crate::line_input::LineEdit::with_selection(payload, cursor, anchor)
    }

    /// Handles an event for a focused cmdline; `Some(true)` → quit, `Some(false)` → handled, `None` → unrecognised.
    pub(crate) fn cmdline_handle_event(&mut self, ev: Event) -> Option<bool> {
        match ev {
            Event::Paste(data) => {
                self.cmdline_apply_edit(crate::line_input::EditCommand::InsertText(data));
                Some(false)
            }
            Event::Key(k) => self.cmdline_handle_key(k),
            _ => None,
        }
    }

    /// Handles a keystroke for a focused cmdline; `Some(true)` → quit, `Some(false)` → handled, `None` → unrecognised.
    pub(crate) fn cmdline_handle_key(&mut self, k: KeyEvent) -> Option<bool> {
        match CmdlineInputAction::from_key(self.cmdline.mode, k)? {
            CmdlineInputAction::Cancel => {
                if self.cmdline_completer_open() {
                    self.cmdline_dismiss_completer();
                    return Some(false);
                }
                if matches!(self.cmdline.mode, CmdlineMode::Search { .. }) {
                    self.clear_search();
                }
                self.close_cmdline();
                Some(false)
            }
            CmdlineInputAction::Submit => Some(self.cmdline_submit()),
            CmdlineInputAction::CloseIfEmpty if self.cmdline_text().is_empty() => {
                self.close_cmdline();
                Some(false)
            }
            CmdlineInputAction::CloseIfEmpty => {
                let command = crate::line_input::command_for_key(k)?;
                self.cmdline_apply_edit(command);
                Some(false)
            }
            CmdlineInputAction::HistoryPrevious => {
                if self.cmdline_completer_open() {
                    self.cmdline_complete_move(1);
                } else {
                    self.cmdline_history_up();
                }
                Some(false)
            }
            CmdlineInputAction::HistoryNext => {
                if self.cmdline_completer_open() {
                    self.cmdline_complete_move(-1);
                } else {
                    self.cmdline_history_down();
                }
                Some(false)
            }
            CmdlineInputAction::CompleteOpenOrAccept => {
                self.cmdline_complete_open_or_accept();
                Some(false)
            }
            CmdlineInputAction::CompleteNext => {
                self.cmdline_complete_move(-1);
                Some(false)
            }
            CmdlineInputAction::CompletePrevious => {
                self.cmdline_complete_move(1);
                Some(false)
            }
            CmdlineInputAction::Edit(command) => {
                self.cmdline_apply_edit(command);
                Some(false)
            }
        }
    }

    fn cmdline_apply_edit(&mut self, command: crate::line_input::EditCommand) {
        let mut edit = self.cmdline_edit();
        let old_text = edit.text().to_string();
        edit.apply(command);
        let text = edit.text().to_string();
        let cursor = edit.cursor();
        let selection_anchor = edit.selection_anchor();
        self.cmdline_set_payload(&text, cursor);
        if let Some(win) = self.well_known.cmdline {
            if let Some(w) = self.ui.win_mut(win) {
                let prefix_len = self.cmdline.mode.prefix().len();
                w.set_selection_anchor(selection_anchor.map(|a| prefix_len + a));
            }
        }
        if text != old_text {
            self.cmdline_dismiss_completer();
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
                let cursor = entry.len();
                self.cmdline_set_payload(&entry, cursor);
                self.cmdline_dismiss_completer();
            }
            HistoryStepOwned::Restore { stash } => {
                self.cmdline.history_browse = None;
                self.cmdline.history_stash = String::new();
                let cursor = stash.len();
                self.cmdline_set_payload(&stash, cursor);
                self.cmdline_dismiss_completer();
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

    fn cmdline_complete_open_or_accept(&mut self) {
        if self.cmdline_completer_open() {
            self.cmdline_accept_completion();
            return;
        }
        self.cmdline_open_completer();
    }

    fn cmdline_complete_move(&mut self, delta: isize) {
        if !self.cmdline_completer_open() {
            return;
        }
        let Some(comp) = self.cmdline.completer.as_ref() else {
            return;
        };
        let n = comp.items.len();
        if n == 0 {
            return;
        }
        let next = (comp.selected as isize + delta).rem_euclid(n as isize) as usize;
        self.cmdline_select_completion(next);
    }

    fn cmdline_open_completer(&mut self) {
        if !matches!(self.cmdline.mode, CmdlineMode::Command) {
            return;
        }
        let typed = self.cmdline_text();
        let items = self.lua.command_completion_items();
        let ranked: Vec<CmdlineCompletionItem> = if typed.is_empty() {
            items
                .into_iter()
                .map(|item| CmdlineCompletionItem {
                    label: item.name,
                    description: item.description,
                })
                .collect()
        } else {
            let labels: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
            smelt_core::fuzzy::fuzzy_rank(&typed, &labels)
                .into_iter()
                .map(|i| CmdlineCompletionItem {
                    label: items[i].name.clone(),
                    description: items[i].description.clone(),
                })
                .collect()
        };
        if ranked.is_empty() {
            return;
        }

        let picker_items = ranked.iter().map(cmdline_picker_item).collect();
        let Some(picker) = crate::picker::open(
            self,
            picker_items,
            0,
            crate::picker::PickerPlacement::CmdlineDocked { max_rows: 8 },
            false,
            false,
            50,
        ) else {
            return;
        };
        self.cmdline.completer = Some(CmdlineCompleter {
            items: ranked,
            selected: 0,
            picker: Some(picker),
        });
    }

    fn cmdline_accept_completion(&mut self) {
        let label = self
            .cmdline
            .completer
            .as_ref()
            .and_then(|comp| comp.items.get(comp.selected))
            .map(|item| item.label.clone());
        let Some(label) = label else {
            return;
        };
        self.cmdline_dismiss_completer();
        let cursor = label.len();
        self.cmdline_set_payload(&label, cursor);
    }

    fn cmdline_select_completion(&mut self, selected: usize) {
        let (picker, selected) = {
            let Some(comp) = self.cmdline.completer.as_mut() else {
                return;
            };
            if comp.items.is_empty() {
                return;
            }
            comp.selected = selected.min(comp.items.len() - 1);
            (comp.picker, comp.selected)
        };
        if let Some(win) = picker {
            crate::picker::set_selected(self, win, selected);
        }
    }
}
