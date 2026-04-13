use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::draw_bar;
use crate::{theme, utils::format_duration};
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::Print;
use crossterm::QueueableCommand;
use engine::tools::ProcessInfo;

use super::{end_dialog_draw, truncate_str, DialogResult, ListState, RenderOut};

pub struct PsDialog {
    registry: engine::tools::ProcessRegistry,
    procs: Vec<ProcessInfo>,
    list: ListState,
    killed: Vec<String>,
}

impl PsDialog {
    pub fn new(registry: engine::tools::ProcessRegistry) -> Self {
        let procs = Self::fetch_procs(&registry, &[]);
        let list = ListState::new(procs.len().max(1));
        Self {
            registry,
            procs,
            list,
            killed: Vec::new(),
        }
    }

    fn fetch_procs(
        registry: &engine::tools::ProcessRegistry,
        killed: &[String],
    ) -> Vec<ProcessInfo> {
        registry
            .list()
            .into_iter()
            .filter(|p| !killed.contains(&p.id))
            .collect()
    }
}

impl super::Dialog for PsDialog {
    fn height(&self) -> u16 {
        self.list.height(self.procs.len().max(1), 4)
    }

    fn constrain_height(&self) -> bool {
        true
    }

    fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.list.handle_resize();
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult> {
        // Ps-specific: backspace kills a process.
        if code == KeyCode::Backspace {
            if let Some(p) = self.procs.get(self.list.selected) {
                self.killed.push(p.id.clone());
                self.procs = Self::fetch_procs(&self.registry, &self.killed);
                self.list.set_items(self.procs.len().max(1));
            }
            return None;
        }

        let n = self.procs.len();
        match nav_lookup(code, mods) {
            Some(NavAction::Dismiss) => Some(DialogResult::PsClosed),
            Some(NavAction::Up) => {
                self.list.select_prev(n);
                None
            }
            Some(NavAction::Down) => {
                self.list.select_next(n);
                None
            }
            Some(NavAction::PageUp) => {
                self.list.page_up();
                None
            }
            Some(NavAction::PageDown) => {
                self.list.page_down(n);
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        let fresh = Self::fetch_procs(&self.registry, &self.killed);
        if fresh.len() != self.procs.len() {
            self.list.set_items(fresh.len().max(1));
        }
        self.procs = fresh;

        let Some(w) = self.list.begin_draw(
            out,
            start_row,
            self.procs.len().max(1),
            width,
            granted_rows,
            4,
        ) else {
            return;
        };
        let now = std::time::Instant::now();

        draw_bar(out, w, None, None, theme::accent());
        out.overlay_newline();

        out.push_dim();
        let _ = out.queue(Print(" Background Processes"));
        out.pop_style();
        out.overlay_newline();

        if self.procs.is_empty() {
            out.push_dim();
            let _ = out.queue(Print("  No processes"));
            out.pop_style();
            out.overlay_newline();
        } else {
            let range = self.list.visible_range(self.procs.len());
            for (i, proc) in self
                .procs
                .iter()
                .enumerate()
                .take(range.end)
                .skip(range.start)
            {
                let time = format_duration(now.duration_since(proc.started_at).as_secs());
                let meta = format!(" {time} {}", proc.id);
                let meta_len = meta.chars().count() + 1;
                let max_cmd = w.saturating_sub(meta_len + 4);
                let cmd_display = truncate_str(&proc.command, max_cmd);
                if i == self.list.selected {
                    let _ = out.queue(Print("  "));
                    out.push_fg(theme::accent());
                    let _ = out.queue(Print(&cmd_display));
                    out.pop_style();
                } else {
                    let _ = out.queue(Print("  "));
                    let _ = out.queue(Print(&cmd_display));
                }
                let _ = out.queue(Print(" "));
                out.push_dim();
                let _ = out.queue(Print(format!("{time} {}", proc.id)));
                out.pop_style();
                out.overlay_newline();
            }
        }

        out.overlay_newline();
        out.push_dim();
        let _ = out.queue(Print(&hints::join(&[hints::CLOSE, hints::KILL_PROC])));
        out.pop_style();
        end_dialog_draw(out);
    }
}
